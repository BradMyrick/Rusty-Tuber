//! Layered asset catalog.
//!
//! Walks the configured asset root once at startup and builds a
//! [`LayerCatalog`] describing the independent transparent PNG layers that
//! composite into the avatar. All layers must share the same canvas size (the
//! web client stacks them pixel-aligned).
//!
//! ## Layout
//!
//! ```text
//! <asset_root>/
//! ├── base/
//! │   └── *.png            one or more static body images (stacked bottom-up,
//! │                         in filename order). At least one is required.
//! ├── mouths/
//! │   ├── closed.png       level 0 — resting mouth        (required)
//! │   ├── partial.png      level 1                         (optional)
//! │   ├── medium.png       level 2                         (optional)
//! │   └── open.png         level 3 — fully open            (required)
//! └── eyes/
//!     ├── open.png         resting eyes                    (required)
//!     ├── closed.png       eyes-closed (blink)             (optional)
//!     └── <emotion>/
//!         ├── open.png     this emotion's resting eyes     (required)
//!         └── closed.png   this emotion's blink eyes       (optional)
//! ```
//!
//! Optional frames are snapped to the nearest available level (tie → less open),
//! so a 2-frame mouth set (`closed` + `open`) still works, and an emotion with
//! only `open.png` simply doesn't blink. Emotions are entirely optional; with
//! none present, the avatar is just base + default eyes + mic mouth.

use crate::protocol::{
    EyeLayers, EyeState, LayerCatalog, MouthLayers, MouthState,
};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;
use tracing::warn;

/// URL prefix under which the HTTP layer serves the asset root.
pub const FRAMES_URL_PREFIX: &str = "/frames";

/// Mouth level names ordered from resting to fully open.
const MOUTH_NAMES: [&str; 4] = ["closed", "partial", "medium", "open"];

/// The on-disk layered asset catalog.
#[derive(Debug, Clone)]
pub struct AssetCatalog(pub LayerCatalog);

impl AssetCatalog {
    /// Scan `asset_root` and build the catalog.
    pub fn load(asset_root: &Path) -> Result<Self> {
        let base = load_base(asset_root)?;
        let mouths = load_mouths(asset_root)?;
        let default_eyes =
            load_eye_set(asset_root, &asset_root.join("eyes"), "default eyes");
        let emotions = load_emotions(asset_root)?;

        // `default_eyes.open` is mandatory — without it there's nothing to show.
        if default_eyes.open.is_none() {
            bail!(
                "eyes/open.png is required under {} (the resting eyes frame)",
                asset_root.display()
            );
        }
        // `mouths.closed` + `mouths.open` are mandatory anchors.
        if mouths.closed.is_none() || mouths.open.is_none() {
            bail!(
                "mouths/closed.png and mouths/open.png are required under {}",
                asset_root.display()
            );
        }

        Ok(AssetCatalog(LayerCatalog {
            base,
            mouths,
            default_eyes,
            emotions,
        }))
    }

    /// Borrow the wire catalog (for the `Welcome` message / `/api/catalog`).
    pub fn catalog(&self) -> &LayerCatalog {
        &self.0
    }

    /// Emotion (eye-expression set) names, sorted.
    pub fn emotions(&self) -> impl Iterator<Item = &str> {
        self.0.emotions.keys().map(String::as_str)
    }

    /// True if `emotion` (case-insensitive) names a known eye-expression set.
    pub fn has_emotion(&self, emotion: &str) -> bool {
        self.0.emotions.contains_key(&emotion.to_ascii_lowercase())
    }

    /// Resolve the `/frames/...` URL for the current mouth level (snapping to
    /// the nearest available frame), or `None` if the catalog has none.
    pub fn mouth_frame(&self, mouth: MouthState) -> Option<String> {
        nearest_mouth(&self.0.mouths, mouth)
            .map(|rel| format!("{FRAMES_URL_PREFIX}/{rel}"))
    }

    /// Resolve the `/frames/...` URL for the current eye state, honouring the
    /// active emotion's eye set (falling back to the default eyes, and to
    /// `open` when a blink has no `closed` frame for this expression).
    pub fn eyes_frame(
        &self,
        emotion: Option<&str>,
        eyes: EyeState,
    ) -> Option<String> {
        let set = emotion
            .and_then(|e| self.0.emotions.get(&e.to_ascii_lowercase()))
            .unwrap_or(&self.0.default_eyes);
        resolve_eye(set, eyes)
            .or_else(|| resolve_eye(&self.0.default_eyes, eyes))
            .map(|rel| format!("{FRAMES_URL_PREFIX}/{rel}"))
    }
}

// ---------------------------------------------------------------------------
// Loaders
// ---------------------------------------------------------------------------

/// Load every `*.png` under `base/` (sorted by name for stable stacking).
fn load_base(root: &Path) -> Result<Vec<String>> {
    let dir = root.join("base");
    let mut out: Vec<String> = Vec::new();
    if let Some(entries) = read_dir_sorted(&dir)? {
        for path in entries {
            if path.extension().and_then(|e| e.to_str()) != Some("png") {
                continue;
            }
            out.push(rel_from(root, &path));
        }
    }
    if out.is_empty() {
        bail!(
            "no base body images found under {}/base/*.png",
            root.display()
        );
    }
    Ok(out)
}

/// Load the four mouth levels from `mouths/`.
fn load_mouths(root: &Path) -> Result<MouthLayers> {
    let dir = root.join("mouths");
    let mut layers = MouthLayers::default();
    if let Some(entries) = read_dir_sorted(&dir)? {
        // Build a name → path lookup of files only (subdirs ignored).
        let mut by_name = BTreeMap::new();
        for path in entries {
            if path.is_dir() {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("png") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                by_name.insert(stem.to_string(), path);
            }
        }
        for &name in &MOUTH_NAMES {
            if let Some(p) = by_name.get(name) {
                set_mouth(&mut layers, name, rel_from(root, p));
            }
        }
    }
    Ok(layers)
}

/// Load the default eyes from `eyes/open.png` / `eyes/closed.png`, and every
/// emotion eye-set from `eyes/<emotion>/...`.
fn load_emotions(root: &Path) -> Result<BTreeMap<String, EyeLayers>> {
    let dir = root.join("eyes");
    let mut emotions = BTreeMap::new();
    if let Some(entries) = read_dir_sorted(&dir)? {
        for path in entries {
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let key = name.to_ascii_lowercase();
            let set = load_eye_set(root, &path, &format!("emotion {name}"));
            if set.open.is_some() {
                if emotions.contains_key(&key) {
                    warn!(emotion = %key, "duplicate emotion eye-set (case-insensitive); overwriting");
                }
                emotions.insert(key, set);
            }
        }
    }
    Ok(emotions)
}

/// Read `open.png` and `closed.png` from an eyes directory, returning paths
/// relative to the asset root. Missing `open.png` is logged and yields an empty
/// set (the caller decides whether that's fatal).
fn load_eye_set(root: &Path, dir: &Path, what: &str) -> EyeLayers {
    let open = dir.join("open.png");
    let closed = dir.join("closed.png");
    let mut layers = EyeLayers::default();
    if open.exists() {
        layers.open = Some(rel_from(root, &open));
    } else {
        warn!(set = what, "missing open.png; this eye-set will be ignored");
    }
    if closed.exists() {
        layers.closed = Some(rel_from(root, &closed));
    }
    layers
}

/// Path of `file` relative to the asset root, with forward slashes so it's
/// URL-ready (e.g. `eyes/happy/open.png`). Falls back to the bare filename if
/// `file` isn't under `root`.
fn rel_from(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .ok()
        .and_then(|r| r.to_str())
        .map(|s| s.replace('\\', "/"))
        .unwrap_or_else(|| {
            file.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        })
}

/// Read a directory and return its entries sorted by path, or `Ok(None)` if the
/// directory doesn't exist (callers decide whether that's an error).
fn read_dir_sorted(dir: &Path) -> Result<Option<Vec<std::path::PathBuf>>> {
    match std::fs::read_dir(dir) {
        Ok(rd) => {
            let mut paths: Vec<_> =
                rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
            paths.sort();
            Ok(Some(paths))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", dir.display())),
    }
}

// ---------------------------------------------------------------------------
// Resolution helpers
// ---------------------------------------------------------------------------

/// Pick the on-disk mouth frame for a level, snapping to the nearest available
/// level (tie → less open).
pub fn nearest_mouth(set: &MouthLayers, mouth: MouthState) -> Option<&str> {
    let desired = mouth.level();
    let mut best: Option<(u8, &str)> = None;
    for cand in 0..=3u8 {
        let Some(path) = level_path(set, cand) else {
            continue;
        };
        let dist = cand.abs_diff(desired);
        match &best {
            None => best = Some((dist, path)),
            Some((bd, _)) if dist < *bd => best = Some((dist, path)),
            _ => {}
        }
    }
    best.map(|(_, p)| p)
}

fn level_path(set: &MouthLayers, level: u8) -> Option<&str> {
    match level {
        0 => set.closed.as_deref(),
        1 => set.partial.as_deref(),
        2 => set.medium.as_deref(),
        3 => set.open.as_deref(),
        _ => None,
    }
}

/// Resolve the eye frame for a state within one expression set. A blink
/// (`Closed`) with no `closed.png` falls back to `open.png` so the expression
/// stays consistent (that expression just doesn't visibly blink).
pub fn resolve_eye(set: &EyeLayers, eyes: EyeState) -> Option<&str> {
    match eyes {
        EyeState::Open => set.open.as_deref(),
        EyeState::Closed => set.closed.as_deref().or(set.open.as_deref()),
    }
}

fn set_mouth(set: &mut MouthLayers, name: &str, val: String) {
    match name {
        "closed" => set.closed = Some(val),
        "partial" => set.partial = Some(val),
        "medium" => set.medium = Some(val),
        "open" => set.open = Some(val),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn touch(p: PathBuf) -> PathBuf {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, b"png").unwrap();
        p
    }

    fn make_catalog_root(dir: &tempdir_shim::Dir) -> PathBuf {
        let root = dir.path().to_path_buf();
        // base body
        touch(root.join("base/body.png"));
        // full mouth set (4 levels)
        for m in ["closed", "partial", "medium", "open"] {
            touch(root.join(format!("mouths/{m}.png")));
        }
        // default eyes (open + closed)
        touch(root.join("eyes/open.png"));
        touch(root.join("eyes/closed.png"));
        // an emotion eye-set with open + closed
        touch(root.join("eyes/happy/open.png"));
        touch(root.join("eyes/happy/closed.png"));
        // an emotion with only open (no blink)
        touch(root.join("eyes/angry/open.png"));
        root
    }

    fn make_minimal_root(dir: &tempdir_shim::Dir) -> PathBuf {
        let root = dir.path().to_path_buf();
        touch(root.join("base/body.png"));
        // 2-frame mouth (snapping)
        touch(root.join("mouths/closed.png"));
        touch(root.join("mouths/open.png"));
        // eyes open only (no blink)
        touch(root.join("eyes/open.png"));
        root
    }

    #[test]
    fn loads_layered_catalog() {
        let d = tempdir_shim::Dir::new();
        let root = make_catalog_root(&d);
        let cat = AssetCatalog::load(&root).unwrap();
        assert_eq!(cat.0.base, vec!["base/body.png".to_string()]);
        // mouths all present
        assert_eq!(cat.0.mouths.partial.as_deref(), Some("mouths/partial.png"));
        // default eyes
        assert_eq!(cat.0.default_eyes.open.as_deref(), Some("eyes/open.png"));
        assert_eq!(
            cat.0.default_eyes.closed.as_deref(),
            Some("eyes/closed.png")
        );
        // emotions
        let emotions: Vec<_> = cat.emotions().collect();
        assert_eq!(emotions, vec!["angry", "happy"]);
        assert!(cat.has_emotion("HAPPY"));
    }

    #[test]
    fn mouth_snaps_to_nearest_when_partial_set() {
        let d = tempdir_shim::Dir::new();
        let root = make_minimal_root(&d);
        let cat = AssetCatalog::load(&root).unwrap();
        // only closed + open: partial(1) snaps to closed(0), medium(2) to open(3)
        assert_eq!(
            cat.mouth_frame(MouthState::Partial).unwrap(),
            "/frames/mouths/closed.png"
        );
        assert_eq!(
            cat.mouth_frame(MouthState::Medium).unwrap(),
            "/frames/mouths/open.png"
        );
        assert_eq!(
            cat.mouth_frame(MouthState::Open).unwrap(),
            "/frames/mouths/open.png"
        );
    }

    #[test]
    fn eyes_resolve_for_default_and_emotion() {
        let d = tempdir_shim::Dir::new();
        let root = make_catalog_root(&d);
        let cat = AssetCatalog::load(&root).unwrap();
        // default eyes
        assert_eq!(
            cat.eyes_frame(None, EyeState::Open).unwrap(),
            "/frames/eyes/open.png"
        );
        assert_eq!(
            cat.eyes_frame(None, EyeState::Closed).unwrap(),
            "/frames/eyes/closed.png"
        );
        // happy emotion
        assert_eq!(
            cat.eyes_frame(Some("happy"), EyeState::Closed).unwrap(),
            "/frames/eyes/happy/closed.png"
        );
        // angry has no closed → blink falls back to angry open (no visible blink)
        assert_eq!(
            cat.eyes_frame(Some("angry"), EyeState::Closed).unwrap(),
            "/frames/eyes/angry/open.png"
        );
    }

    #[test]
    fn missing_default_eyes_open_is_fatal() {
        let d = tempdir_shim::Dir::new();
        let root = d.path().to_path_buf();
        touch(root.join("base/body.png"));
        touch(root.join("mouths/closed.png"));
        touch(root.join("mouths/open.png"));
        // no eyes/open.png
        let err = AssetCatalog::load(&root).unwrap_err();
        assert!(format!("{err:#}").contains("eyes/open.png"));
    }

    #[test]
    fn missing_mouth_anchors_is_fatal() {
        let d = tempdir_shim::Dir::new();
        let root = d.path().to_path_buf();
        touch(root.join("base/body.png"));
        touch(root.join("eyes/open.png"));
        // no mouths
        let err = AssetCatalog::load(&root).unwrap_err();
        assert!(format!("{err:#}").contains("mouths/closed.png"));
    }

    #[test]
    fn empty_base_is_fatal() {
        let d = tempdir_shim::Dir::new();
        let root = d.path().to_path_buf();
        fs::create_dir_all(root.join("base")).unwrap(); // empty base dir
        touch(root.join("mouths/closed.png"));
        touch(root.join("mouths/open.png"));
        touch(root.join("eyes/open.png"));
        let err = AssetCatalog::load(&root).unwrap_err();
        assert!(format!("{err:#}").contains("base body"));
    }

    // Minimal tempdir helper so we don't pull the `tempfile` dev-dep just for assets.
    mod tempdir_shim {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Self {
                let mut p = std::env::temp_dir();
                p.push(format!("rusty-tuber-{}", std::process::id()));
                p.push(format!(
                    "{}-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos(),
                    std::thread::current().name().unwrap_or("t")
                ));
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
