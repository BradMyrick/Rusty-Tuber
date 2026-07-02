//! Server-side avatar compositor — the universal source of truth for what the
//! avatar looks like at any moment.
//!
//! All layer PNGs are decoded once at startup. [`Compositor::render`] composites
//! the static base + eye + mouth layers + any custom animation overlays into a
//! single RGBA frame that feeds the virtual webcam.

use crate::assets::AssetCatalog;
use crate::protocol::{EyeState, MouthState};
use anyhow::{Context, Result};
use image::imageops;
use image::RgbaImage;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

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
/// Scheduler parameters for one animation group.
pub struct AnimSchedulerConfig {
    pub instances: usize,
    pub frames: usize,
    pub min_interval: f32,
    pub max_interval: f32,
}

struct AnimInstance {
    frames: Vec<Layer>,
}

/// A decoded overlay layer paired with the bounding box of its non-transparent
/// pixels (in canvas coordinates). The compositor only blends inside the box,
/// so a small worm sprite on a 2048² canvas skips ~99% of the pixels.
struct Layer {
    img: RgbaImage,
    bbox: BBox,
}

/// Half-open pixel rectangle `[x0..x1) × [y0..y1)`. Empty (`x0==x1 || y0==y1`)
/// means the layer is fully transparent.
#[derive(Clone, Copy)]
struct BBox {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

impl BBox {
    const EMPTY: BBox = BBox {
        x0: 0,
        y0: 0,
        x1: 0,
        y1: 0,
    };
}

/// Scan every pixel once and return the tight bounding box of non-transparent
/// pixels. Used at load time so the per-frame render touches only the region
/// that actually carries opacity.
fn opaque_bbox(img: &RgbaImage) -> BBox {
    let (w, h) = (img.width(), img.height());
    let raw = img.as_raw();
    let mut x0 = w;
    let mut y0 = h;
    let mut x1 = 0u32;
    let mut y1 = 0u32;
    for y in 0..h {
        for x in 0..w {
            let a = raw[((y * w + x) * 4 + 3) as usize];
            if a != 0 {
                if x < x0 {
                    x0 = x;
                }
                if x >= x1 {
                    x1 = x + 1;
                }
                if y < y0 {
                    y0 = y;
                }
                y1 = y + 1;
            }
        }
    }
    if x0 >= x1 || y0 >= y1 {
        BBox::EMPTY
    } else {
        BBox { x0, y0, x1, y1 }
    }
}

#[derive(Deserialize)]
struct CharacterToml {
    #[serde(default)]
    anim: Vec<AnimDef>,
}

#[derive(Deserialize)]
struct AnimDef {
    name: String,
    #[serde(default = "d_one")]
    instances: usize,
    frames: usize,
    file_pattern: String,
    #[serde(default = "d_min_int")]
    min_interval: f32,
    #[serde(default = "d_max_int")]
    max_interval: f32,
}

fn d_one() -> usize {
    1
}
fn d_min_int() -> f32 {
    0.1
}
fn d_max_int() -> f32 {
    0.5
}

pub struct Compositor {
    width: u32,
    height: u32,
    base_composited: Vec<u8>,
    layers: HashMap<String, Layer>,
    catalog: Arc<AssetCatalog>,
    anim_instances: Vec<AnimInstance>,
    anim_config: Vec<AnimSchedulerConfig>,
}

impl Compositor {
    /// Decode every layer referenced by the catalog under `asset_root`.
    pub fn new(catalog: Arc<AssetCatalog>, asset_root: &Path) -> Result<Self> {
        let mut layers: HashMap<String, Layer> = HashMap::new();

        // Base.
        let mut base = Vec::new();
        for rel in &catalog.catalog().base {
            decode_into(asset_root, rel, &mut layers)?;
            if let Some(layer) = layers.get(rel) {
                base.push(layer.img.clone());
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

        // Custom animation channels from character.toml.
        let (anim_instances, anim_config) = load_anim(asset_root)?;

        info!(
            width,
            height,
            layers = layers.len(),
            anim_instances = anim_instances.len(),
            "compositor ready"
        );
        Ok(Compositor {
            width,
            height,
            base_composited,
            layers,
            catalog,
            anim_instances,
            anim_config,
        })
    }

    /// Composite the avatar for the current state. Copies the pre-composited
    /// base (memcpy) then overlays the eye/mouth/anim layers. Each overlay is
    /// cropped to its precomputed opaque bounding box, so a sparse layer (a
    /// small worm sprite on a 2048² canvas) blends only its occupied pixels.
    pub fn anim_config(&self) -> &[AnimSchedulerConfig] {
        &self.anim_config
    }

    pub fn render(
        &self,
        emotion: Option<&str>,
        mouth: MouthState,
        eyes: EyeState,
        anim_frames: &[usize],
    ) -> Frame {
        let mut rgba = Vec::with_capacity(self.base_composited.len());
        rgba.extend_from_slice(&self.base_composited);
        if let Some(url) = self.catalog.eyes_frame(emotion, eyes) {
            if let Some(layer) = self.layers.get(rel_of(&url)) {
                overlay_bbox(&mut rgba, layer, self.width);
            }
        }
        if let Some(url) = self.catalog.mouth_frame(mouth) {
            if let Some(layer) = self.layers.get(rel_of(&url)) {
                overlay_bbox(&mut rgba, layer, self.width);
            }
        }
        for (i, inst) in self.anim_instances.iter().enumerate() {
            let f = anim_frames.get(i).copied().unwrap_or(0);
            if let Some(layer) = inst.frames.get(f) {
                overlay_bbox(&mut rgba, layer, self.width);
            }
        }
        Frame {
            width: self.width,
            height: self.height,
            rgba,
        }
    }
}

/// Alpha-over `layer` onto `bottom` (flat RGBA), restricted to the layer's
/// opaque bounding box. Fully-transparent source pixels are skipped and opaque
/// ones are fast-pathed; semi-transparent ones get the exact `/255` blend.
fn overlay_bbox(bottom: &mut [u8], layer: &Layer, width: u32) {
    let bbox = layer.bbox;
    if bbox.x0 >= bbox.x1 || bbox.y0 >= bbox.y1 {
        return;
    }
    let top = layer.img.as_raw();
    for y in bbox.y0..bbox.y1 {
        let row_start = (y * width + bbox.x0) as usize * 4;
        let row_end = (y * width + bbox.x1) as usize * 4;
        let mut bi = row_start;
        for ti in (row_start..row_end).step_by(4) {
            let a = top[ti + 3] as u32;
            if a == 0 {
                bi += 4;
                continue;
            }
            if a == 255 {
                bottom[bi] = top[ti];
                bottom[bi + 1] = top[ti + 1];
                bottom[bi + 2] = top[ti + 2];
                bottom[bi + 3] = 255;
            } else {
                let inv = 255 - a;
                let div = |sum: u32| (sum + (sum >> 8) + 1) >> 8;
                bottom[bi] =
                    div(a * top[ti] as u32 + inv * bottom[bi] as u32) as u8;
                bottom[bi + 1] =
                    div(a * top[ti + 1] as u32 + inv * bottom[bi + 1] as u32)
                        as u8;
                bottom[bi + 2] =
                    div(a * top[ti + 2] as u32 + inv * bottom[bi + 2] as u32)
                        as u8;
                bottom[bi + 3] = 255;
            }
            bi += 4;
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
    layers: &mut HashMap<String, Layer>,
) -> Result<()> {
    if rel.is_empty() || layers.contains_key(rel) {
        return Ok(());
    }
    let img = load_layer(root, rel)?;
    let bbox = opaque_bbox(&img);
    layers.insert(rel.to_string(), Layer { img, bbox });
    Ok(())
}

/// Load custom animation groups from `character.toml` + decode their frames.
/// Returns `(instances, scheduler_configs)` — one instance per independent
/// animated layer, plus the timing config the scheduler needs.
fn load_anim(
    root: &Path,
) -> Result<(Vec<AnimInstance>, Vec<AnimSchedulerConfig>)> {
    let toml_path = root.join("character.toml");
    if !toml_path.exists() {
        return Ok((Vec::new(), Vec::new()));
    }
    let raw = std::fs::read_to_string(&toml_path)
        .with_context(|| format!("reading {}", toml_path.display()))?;
    let cfg: CharacterToml = toml::from_str(&raw)
        .with_context(|| format!("parsing {}", toml_path.display()))?;

    let mut instances = Vec::new();
    let mut configs = Vec::new();
    for def in &cfg.anim {
        let dir = root.join("anim").join(&def.name);
        for n in 1..=def.instances {
            let mut frames = Vec::new();
            for f in 1..=def.frames {
                let fname = def
                    .file_pattern
                    .replace("{n}", &n.to_string())
                    .replace("{f}", &f.to_string());
                let path = dir.join(&fname);
                if path.exists() {
                    let img = image::open(&path)?.to_rgba8();
                    let bbox = opaque_bbox(&img);
                    frames.push(Layer { img, bbox });
                } else {
                    warn!(file = %path.display(), "animation frame missing");
                }
            }
            if !frames.is_empty() {
                instances.push(AnimInstance { frames });
            }
        }
        configs.push(AnimSchedulerConfig {
            instances: def.instances,
            frames: def.frames,
            min_interval: def.min_interval,
            max_interval: def.max_interval,
        });
        info!(
            group = %def.name,
            instances = def.instances,
            frames = def.frames,
            "loaded animation group"
        );
    }
    Ok((instances, configs))
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
        let frame = comp.render(None, MouthState::Partial, EyeState::Open, &[]);
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
