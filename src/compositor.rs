//! Avatar compositor — the universal source of truth for what the avatar looks
//! like at any moment.
//!
//! All layer PNGs are decoded once at startup. [`Compositor::render`] composites
//! the static base + eye + mouth layers + any custom animation overlays into a
//! single RGBA frame that feeds the virtual webcam.

use crate::assets::AssetCatalog;
use crate::protocol::{EyeState, MouthState};
use anyhow::{bail, Context, Result};
use image::imageops;
use image::RgbaImage;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    idle_layer: Option<Layer>,
}

impl Compositor {
    /// Decode every layer referenced by the catalog under `asset_root`.
    ///
    /// `output_size`, if set, scales every layer to that square edge at load
    /// time (all character art is 1:1), so the whole pipeline — composite, blend,
    /// device write — runs at that resolution instead of the (often much larger)
    /// native art size. Cost scales with pixels, so this is the single biggest
    /// perf lever for a webcam output.
    pub fn new(
        catalog: Arc<AssetCatalog>,
        asset_root: &Path,
        output_size: Option<u32>,
    ) -> Result<Self> {
        let mut layers: HashMap<String, Layer> = HashMap::new();

        // Base.
        let mut base = Vec::new();
        for rel in &catalog.catalog().base {
            decode_into(asset_root, rel, &mut layers, output_size)?;
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
            decode_into(asset_root, rel, &mut layers, output_size)?;
        }
        // Default eyes + every emotion eye-set.
        let e = &catalog.catalog().default_eyes;
        if let Some(r) = &e.open {
            decode_into(asset_root, r, &mut layers, output_size)?;
        }
        if let Some(r) = &e.closed {
            decode_into(asset_root, r, &mut layers, output_size)?;
        }
        for set in catalog.catalog().emotions.values() {
            if let Some(r) = &set.open {
                decode_into(asset_root, r, &mut layers, output_size)?;
            }
            if let Some(r) = &set.closed {
                decode_into(asset_root, r, &mut layers, output_size)?;
            }
        }

        // Custom animation channels from character.toml.
        let (anim_instances, anim_config) = load_anim(asset_root, output_size)?;

        // Idle resting overlay (the "smile after silence" feature). Optional;
        // loaded from anim/smile/ if present.
        let idle_layer = load_idle_layer(asset_root, output_size)?;
        if idle_layer.is_some() {
            info!("loaded idle resting overlay (anim/smile)");
        }

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
            idle_layer,
        })
    }

    /// True if an idle resting overlay (`anim/smile/*.png`) was loaded, i.e. the
    /// "smile after silence" feature can ever engage.
    pub fn has_idle_layer(&self) -> bool {
        self.idle_layer.is_some()
    }

    /// Composite the avatar for the current state. Copies the pre-composited
    /// base (memcpy) then overlays the eye/mouth/anim layers. Each overlay is
    /// cropped to its precomputed opaque bounding box, so a sparse layer (a
    /// small worm sprite on a 2048² canvas) blends only its occupied pixels.
    /// `show_idle` composites the idle resting overlay (`anim/smile/`) on top of
    /// everything else — the character's "resting face" shown after silence.
    pub fn anim_config(&self) -> &[AnimSchedulerConfig] {
        &self.anim_config
    }

    pub fn render(
        &self,
        emotion: Option<&str>,
        mouth: MouthState,
        eyes: EyeState,
        anim_frames: &[usize],
        show_idle: bool,
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
        if show_idle {
            if let Some(layer) = &self.idle_layer {
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

/// Strip the `/frames/` prefix from a resolved layer path to get its catalog
/// rel-path (the key into the decoded layer cache).
fn rel_of(url: &str) -> &str {
    url.strip_prefix(crate::assets::FRAMES_URL_PREFIX)
        .unwrap_or(url)
        .strip_prefix('/')
        .unwrap_or(url)
}

fn load_layer(
    root: &Path,
    rel: &str,
    output_size: Option<u32>,
) -> Result<RgbaImage> {
    let path = root.join(rel);
    let img = image::open(&path)
        .with_context(|| format!("compositor: decoding {}", path.display()))?;
    let rgba = img.to_rgba8();
    if rgba.width() == 0 || rgba.height() == 0 {
        anyhow::bail!("compositor: empty image {}", path.display());
    }
    Ok(fit(rgba, output_size))
}

/// Scale a square layer to `output_size` (Lanczos3, one-time at load). No-op
/// when `output_size` is `None` or already matches. All character art is 1:1.
fn fit(img: RgbaImage, output_size: Option<u32>) -> RgbaImage {
    match output_size {
        Some(s) if s != img.width() || s != img.height() => {
            imageops::resize(&img, s, s, imageops::FilterType::Lanczos3)
        }
        _ => img,
    }
}

/// Decode `rel` into the shared layer cache (no-op if empty or already cached).
fn decode_into(
    root: &Path,
    rel: &str,
    layers: &mut HashMap<String, Layer>,
    output_size: Option<u32>,
) -> Result<()> {
    if rel.is_empty() || layers.contains_key(rel) {
        return Ok(());
    }
    let img = load_layer(root, rel, output_size)?;
    let bbox = opaque_bbox(&img);
    layers.insert(rel.to_string(), Layer { img, bbox });
    Ok(())
}

/// Load custom animation groups from `character.toml` + decode their frames.
/// Returns `(instances, scheduler_configs)` — one instance per independent
/// animated layer, plus the timing config the scheduler needs.
fn load_anim(
    root: &Path,
    output_size: Option<u32>,
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
                    let img = fit(img, output_size);
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

/// Load the idle resting overlay from `anim/smile/`. This is the character's
/// "resting face" (e.g. a smile) shown on top of everything else after a
/// configurable silence period.
///
/// The folder is optional — returns `Ok(None)` if it doesn't exist (the feature
/// stays inactive). When it exists, a single PNG is picked: `smile.png` if
/// present, otherwise the first PNG (sorted by name). Multiple frames aren't
/// supported here (that's the `[[anim]]` random-cycle system's job); the idle
/// overlay is one static layer.
fn load_idle_layer(
    root: &Path,
    output_size: Option<u32>,
) -> Result<Option<Layer>> {
    let dir = root.join("anim").join("smile");
    let mut paths: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.is_file()
                    && p.extension().and_then(|e| e.to_str()) == Some("png")
            })
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(e) => {
            return Err(e)
                .with_context(|| format!("reading {}", dir.display()));
        }
    };
    if paths.is_empty() {
        warn!("anim/smile/ exists but contains no PNG; idle overlay disabled");
        return Ok(None);
    }
    paths.sort();
    // Prefer a file literally named smile.png; fall back to the first PNG.
    let path = paths
        .iter()
        .find(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("smile"))
                .unwrap_or(false)
        })
        .or(paths.first())
        .expect("paths is non-empty");
    let img = image::open(path)
        .with_context(|| format!("decoding idle overlay {}", path.display()))?
        .to_rgba8();
    if img.width() == 0 || img.height() == 0 {
        bail!("idle overlay {} is empty", path.display());
    }
    let img = fit(img, output_size);
    let bbox = opaque_bbox(&img);
    Ok(Some(Layer { img, bbox }))
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
        let comp = Compositor::new(cat, &root, None).unwrap();
        // Mouth=Partial snaps to closed (nearest). The top-left pixel should be
        // the mouth (green) since the mouth is drawn over the white eyes over
        // the red base — last-wins at (0,0) is mouth green.
        let frame =
            comp.render(None, MouthState::Partial, EyeState::Open, &[], false);
        assert_eq!((frame.width, frame.height), (2, 2));
        let p = &frame.rgba[0..4];
        assert_eq!(p, &[0, 255, 0, 255]); // green closed mouth wins
    }

    #[test]
    fn idle_overlay_loads_and_renders_on_top() {
        let dir = tempdir();
        let (cat, root) = fake_catalog(&dir);
        // Drop an idle smile PNG: opaque yellow, covering the whole canvas.
        write_png(
            &root.join("anim/smile/smile.png"),
            &[255u8, 255, 0, 255].repeat(4),
            2,
            2,
        );
        let comp = Compositor::new(cat, &root, None).unwrap();
        assert!(comp.has_idle_layer());
        // Without idle: closed mouth green wins at (0,0).
        let f1 =
            comp.render(None, MouthState::Closed, EyeState::Open, &[], false);
        assert_eq!(&f1.rgba[0..4], &[0, 255, 0, 255]);
        // With idle: the yellow smile is composited on top of everything.
        let f2 =
            comp.render(None, MouthState::Closed, EyeState::Open, &[], true);
        assert_eq!(&f2.rgba[0..4], &[255, 255, 0, 255]);
    }

    #[test]
    fn idle_overlay_absent_means_feature_inactive() {
        let dir = tempdir();
        let (cat, root) = fake_catalog(&dir);
        // No anim/smile/ folder.
        let comp = Compositor::new(cat, &root, None).unwrap();
        assert!(!comp.has_idle_layer());
        // show_idle=true is a harmless no-op when no layer is loaded.
        let f =
            comp.render(None, MouthState::Closed, EyeState::Open, &[], true);
        assert_eq!(&f.rgba[0..4], &[0, 255, 0, 255]);
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
