//! Server-side avatar compositor — the universal source of truth for what the
//! avatar looks like at any moment.
//!
//! All layer PNGs are decoded once at startup into in-memory [`RgbaImage`]s.
//! [`Compositor::render`] then composites the static base + the current eye
//! layer + the current mouth layer into a single RGBA frame (transparent where
//! the avatar is empty). That frame feeds every output sink:
//!
//! - the **browser sink** encodes it to PNG (alpha preserved) for the
//!   transparent OBS Browser Source;
//! - the **webcam sink** composites it over a chroma background and writes it
//!   to a v4l2loopback device.
//!
//! Compositing is pure CPU alpha-over and runs only when the visible state
//! changes (a few times per second while talking), so the hot audio path and
//! the 20 Hz meter updates are untouched.

use crate::assets::AssetCatalog;
use crate::protocol::{EyeState, MouthState};
use anyhow::{Context, Result};
use image::imageops;
use image::RgbaImage;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tracing::info;

/// A composited RGBA frame. `rgba` is `width * height * 4` bytes, straight
/// (non-premultiplied) alpha, row-major top-to-bottom.
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Decoded layer cache + renderer. Cheap to `render` (the static base is
/// pre-composited once; each render copies it and overlays the sparse eye/mouth
/// layers with a skip-transparent blit); shared behind an `Arc`.
pub struct Compositor {
    width: u32,
    height: u32,
    /// The static body, fully composited once at startup into a flat RGBA
    /// buffer. Each render just copies this (one memcpy, no alpha math).
    base_composited: Vec<u8>,
    /// Every decodeable layer keyed by its catalog rel-path
    /// (e.g. `mouths/open.png`, `eyes/happy/closed.png`).
    layers: HashMap<String, RgbaImage>,
    catalog: Arc<AssetCatalog>,
}

impl Compositor {
    /// Decode every layer referenced by the catalog under `asset_root`.
    pub fn new(catalog: Arc<AssetCatalog>, asset_root: &Path) -> Result<Self> {
        let mut layers: HashMap<String, RgbaImage> = HashMap::new();

        // Base.
        let mut base = Vec::new();
        for rel in &catalog.catalog().base {
            decode_into(asset_root, rel, &mut layers)?;
            if let Some(img) = layers.get(rel) {
                base.push(img.clone());
            }
        }
        if base.is_empty() {
            anyhow::bail!("compositor: no base layers decoded");
        }
        let (width, height) = (base[0].width(), base[0].height());

        // Pre-composite the static base once (one-time cost), so per-frame
        // renders just copy it instead of re-running alpha-over on every layer.
        let mut canvas = RgbaImage::new(width, height);
        for b in &base {
            imageops::overlay(&mut canvas, b, 0, 0);
        }
        let base_composited = canvas.into_raw();

        // Mouths.
        let m = &catalog.catalog().mouths;
        for rel in [&m.closed, &m.partial, &m.medium, &m.open]
            .into_iter()
            .flatten()
        {
            decode_into(asset_root, rel, &mut layers)?;
        }
        // Default eyes + every emotion eye-set.
        let e = &catalog.catalog().default_eyes;
        if let Some(r) = &e.open {
            decode_into(asset_root, r, &mut layers)?;
        }
        if let Some(r) = &e.closed {
            decode_into(asset_root, r, &mut layers)?;
        }
        for set in catalog.catalog().emotions.values() {
            if let Some(r) = &set.open {
                decode_into(asset_root, r, &mut layers)?;
            }
            if let Some(r) = &set.closed {
                decode_into(asset_root, r, &mut layers)?;
            }
        }

        info!(width, height, layers = layers.len(), "compositor ready");
        Ok(Compositor {
            width,
            height,
            base_composited,
            layers,
            catalog,
        })
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Composite the avatar for the current state. Copies the pre-composited
    /// base (cheap memcpy) then overlays the eye + mouth layers with a
    /// skip-transparent blit — those layers are sparse, so most pixels are
    /// skipped. The result keeps a transparent background; sinks fill it.
    pub fn render(
        &self,
        emotion: Option<&str>,
        mouth: MouthState,
        eyes: EyeState,
    ) -> Frame {
        // Start from the pre-composited base (no alpha math, no zero-fill).
        let mut rgba = Vec::with_capacity(self.base_composited.len());
        rgba.extend_from_slice(&self.base_composited);
        if let Some(url) = self.catalog.eyes_frame(emotion, eyes) {
            if let Some(img) = self.layers.get(rel_of(&url)) {
                overlay_skip(&mut rgba, img.as_raw());
            }
        }
        if let Some(url) = self.catalog.mouth_frame(mouth) {
            if let Some(img) = self.layers.get(rel_of(&url)) {
                overlay_skip(&mut rgba, img.as_raw());
            }
        }
        Frame {
            width: self.width,
            height: self.height,
            rgba,
        }
    }
}

/// Alpha-over `top` onto `bottom` (both flat RGBA byte slices), skipping fully
/// transparent source pixels and fast-pathing opaque ones. Eye/mouth layers are
/// mostly transparent, so this is far cheaper than the generic per-pixel blend.
fn overlay_skip(bottom: &mut [u8], top: &[u8]) {
    let pixels = bottom.len() / 4;
    for i in 0..pixels {
        let j = i * 4;
        let a = top[j + 3] as u32;
        if a == 0 {
            continue;
        }
        if a == 255 {
            // Fully opaque source — straight copy.
            bottom[j] = top[j];
            bottom[j + 1] = top[j + 1];
            bottom[j + 2] = top[j + 2];
            bottom[j + 3] = 255;
        } else {
            let inv = 255 - a;
            bottom[j] =
                ((a * top[j] as u32 + inv * bottom[j] as u32) / 255) as u8;
            bottom[j + 1] = ((a * top[j + 1] as u32
                + inv * bottom[j + 1] as u32)
                / 255) as u8;
            bottom[j + 2] = ((a * top[j + 2] as u32
                + inv * bottom[j + 2] as u32)
                / 255) as u8;
            bottom[j + 3] = (a + inv * bottom[j + 3] as u32 / 255) as u8;
        }
    }
}

/// Strip the `/frames/` prefix from a resolved layer URL to get its catalog
/// rel-path (the key into the decoded layer cache).
fn rel_of(url: &str) -> &str {
    url.strip_prefix(crate::assets::FRAMES_URL_PREFIX)
        .unwrap_or(url)
        .strip_prefix('/')
        .unwrap_or(url)
}

fn load_layer(root: &Path, rel: &str) -> Result<RgbaImage> {
    let path = root.join(rel);
    let img = image::open(&path)
        .with_context(|| format!("compositor: decoding {}", path.display()))?;
    let rgba = img.to_rgba8();
    if rgba.width() == 0 || rgba.height() == 0 {
        anyhow::bail!("compositor: empty image {}", path.display());
    }
    Ok(rgba)
}

/// Decode `rel` into the shared layer cache (no-op if empty or already cached).
fn decode_into(
    root: &Path,
    rel: &str,
    layers: &mut HashMap<String, RgbaImage>,
) -> Result<()> {
    if rel.is_empty() || layers.contains_key(rel) {
        return Ok(());
    }
    let img = load_layer(root, rel)?;
    layers.insert(rel.to_string(), img);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{EyeLayers, LayerCatalog, MouthLayers};
    use std::collections::BTreeMap;
    use std::fs;

    fn write_png(path: &std::path::Path, rgba: &[u8], w: u32, h: u32) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let img: RgbaImage = RgbaImage::from_raw(w, h, rgba.to_vec()).unwrap();
        img.save(path).unwrap();
    }

    fn fake_catalog(
        dir: &std::path::Path,
    ) -> (Arc<AssetCatalog>, std::path::PathBuf) {
        let root = dir.to_path_buf();
        // Solid red base.
        write_png(
            &root.join("base/body.png"),
            &[200u8, 0, 0, 255].repeat(4),
            2,
            2,
        );
        // closed mouth: opaque green strip; open: opaque blue strip.
        write_png(
            &root.join("mouths/closed.png"),
            &[0, 255, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 0, 255],
            2,
            2,
        );
        write_png(
            &root.join("mouths/open.png"),
            &[
                0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255,
            ],
            2,
            2,
        );
        // eyes open: opaque.
        write_png(
            &root.join("eyes/open.png"),
            &[255u8, 255, 255, 255].repeat(4),
            2,
            2,
        );
        let cat = AssetCatalog(LayerCatalog {
            base: vec!["base/body.png".into()],
            mouths: MouthLayers {
                closed: Some("mouths/closed.png".into()),
                partial: None,
                medium: None,
                open: Some("mouths/open.png".into()),
            },
            default_eyes: EyeLayers {
                open: Some("eyes/open.png".into()),
                closed: None,
            },
            emotions: BTreeMap::new(),
        });
        (Arc::new(cat), root)
    }

    #[test]
    fn render_composites_and_resolves_nearest_mouth() {
        let dir = tempdir();
        let (cat, root) = fake_catalog(&dir);
        let comp = Compositor::new(cat, &root).unwrap();
        // Mouth=Partial snaps to closed (nearest). The top-left pixel should be
        // the mouth (green) since the mouth is drawn over the white eyes over
        // the red base — last-wins at (0,0) is mouth green.
        let frame = comp.render(None, MouthState::Partial, EyeState::Open);
        assert_eq!((frame.width, frame.height), (2, 2));
        let p = &frame.rgba[0..4];
        assert_eq!(p, &[0, 255, 0, 255]); // green closed mouth wins
    }

    // Minimal tempdir (avoid pulling `tempfile` just for these tests).
    fn tempdir() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "rusty-tuber-compositor-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }
}
