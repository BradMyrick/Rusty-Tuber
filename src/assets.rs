//! Data-driven asset catalog.
//!
//! Walks the configured asset root once at startup and builds a map of
//! `emotion -> FrameGrid`. Each emotion folder holds mouth frames for one or
//! both eye states, using a flat filename convention:
//!
//! - `<mouth>.png`         -> eyes open   (e.g. `open.png`)
//! - `<mouth>-blink.png`   -> eyes closed (e.g. `open-blink.png`)
//!
//! `closed.png` and `open.png` (eyes-open) are mandatory; `slight`/`medium`
//! are optional. The whole eyes-closed set is optional — when absent, blinks
//! fall back to the eyes-open frame. When present but partial,
//! [`resolve_frame`] snaps to the nearest available mouth **within the desired
//! eye state** (so a blink while talking still shows closed eyes if any
//! closed-eye frame exists).

use crate::protocol::{EyeState, FrameGrid, MouthSet, MouthState};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;
use tracing::warn;

/// URL prefix under which the HTTP layer serves the asset root.
pub const FRAMES_URL_PREFIX: &str = "/frames";

/// Mouth level names ordered from resting to fully open.
const MOUTH_NAMES: [&str; 4] = ["closed", "slight", "medium", "open"];

/// The on-disk asset catalog: emotion name (lower-cased) -> frame grid.
#[derive(Debug, Clone)]
pub struct AssetCatalog(pub BTreeMap<String, FrameGrid>);

impl AssetCatalog {
    /// Scan `asset_root` and build the catalog.
    pub fn load(asset_root: &Path) -> Result<Self> {
        let mut map: BTreeMap<String, FrameGrid> = BTreeMap::new();

        let entries = std::fs::read_dir(asset_root).with_context(|| {
            format!("reading asset root {}", asset_root.display())
        })?;

        for entry in entries.flatten() {
            let dir_path = entry.path();
            if !dir_path.is_dir() {
                continue;
            }
            let Some(dir_name) = dir_path.file_name().and_then(|n| n.to_str())
            else {
                continue;
            };
            let dir_name = dir_name.to_string();
            let key = dir_name.to_ascii_lowercase();

            let mut eyes_open = MouthSet::default();
            let mut eyes_closed = MouthSet::default();

            for &mouth in &MOUTH_NAMES {
                // Eyes-open frame: <mouth>.png
                if dir_path.join(format!("{mouth}.png")).exists() {
                    set_mouth(
                        &mut eyes_open,
                        mouth,
                        format!("{dir_name}/{mouth}.png"),
                    );
                }
                // Eyes-closed (blink) frame: <mouth>-blink.png
                if dir_path.join(format!("{mouth}-blink.png")).exists() {
                    set_mouth(
                        &mut eyes_closed,
                        mouth,
                        format!("{dir_name}/{mouth}-blink.png"),
                    );
                }
            }

            if eyes_open.closed.is_none() || eyes_open.open.is_none() {
                warn!(
                    emotion = %dir_name,
                    "skipping emotion folder: needs closed.png and open.png (eyes-open)"
                );
                continue;
            }

            let grid = FrameGrid {
                eyes_open,
                eyes_closed: if is_empty(&eyes_closed) {
                    None
                } else {
                    Some(eyes_closed)
                },
            };

            if map.contains_key(&key) {
                warn!(emotion = %key, "duplicate emotion folder (case-insensitive); overwriting");
            }
            map.insert(key, grid);
        }

        if map.is_empty() {
            bail!(
                "no usable emotion folders found under {} (each needs closed.png + open.png)",
                asset_root.display()
            );
        }

        Ok(AssetCatalog(map))
    }

    /// Look up the frame grid for an emotion name (case-insensitive).
    pub fn get(&self, emotion: &str) -> Option<&FrameGrid> {
        self.0.get(&emotion.to_ascii_lowercase())
    }

    /// Sorted emotion names.
    pub fn emotions(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// Borrow the underlying emotion -> grid map (for the wire catalog).
    pub fn catalog(&self) -> &crate::protocol::Catalog {
        &self.0
    }

    /// Resolve the absolute URL path for the best frame for `mouth` / `eyes`
    /// under `emotion`, or `None` if the emotion is unknown.
    pub fn frame_url(
        &self,
        emotion: &str,
        mouth: MouthState,
        eyes: EyeState,
    ) -> Option<String> {
        self.get(emotion).map(|g| frame_url(g, mouth, eyes))
    }
}

/// Pick the on-disk frame for a mouth level + eye state.
///
/// Rule: if the desired eye state has any frames, snap to the nearest available
/// mouth within it (tie -> less open). Otherwise fall back to the other eye
/// state (so a legacy emotion with no `-blink` art simply doesn't blink).
pub fn resolve_frame(
    grid: &FrameGrid,
    mouth: MouthState,
    eyes: EyeState,
) -> Option<&str> {
    let primary: Option<&MouthSet> = match eyes {
        EyeState::Open => Some(&grid.eyes_open),
        EyeState::Closed => grid.eyes_closed.as_ref(),
    };
    if let Some(set) = primary {
        if let Some(p) = nearest(set, mouth) {
            return Some(p);
        }
    }
    let other: Option<&MouthSet> = match eyes {
        EyeState::Open => grid.eyes_closed.as_ref(),
        EyeState::Closed => Some(&grid.eyes_open),
    };
    other.and_then(|s| nearest(s, mouth))
}

/// Build the servable URL (`/frames/<rel>`) for a mouth level + eye state.
pub fn frame_url(
    grid: &FrameGrid,
    mouth: MouthState,
    eyes: EyeState,
) -> String {
    format!(
        "{FRAMES_URL_PREFIX}/{}",
        resolve_frame(grid, mouth, eyes).unwrap_or_default()
    )
}

fn level_path(set: &MouthSet, level: u8) -> Option<&str> {
    match level {
        0 => set.closed.as_deref(),
        1 => set.slight.as_deref(),
        2 => set.medium.as_deref(),
        3 => set.open.as_deref(),
        _ => None,
    }
}

/// Nearest available mouth level, tie-breaking toward less open.
fn nearest(set: &MouthSet, mouth: MouthState) -> Option<&str> {
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

fn set_mouth(set: &mut MouthSet, name: &str, val: String) {
    match name {
        "closed" => set.closed = Some(val),
        "slight" => set.slight = Some(val),
        "medium" => set.medium = Some(val),
        "open" => set.open = Some(val),
        _ => {}
    }
}

fn is_empty(set: &MouthSet) -> bool {
    set.closed.is_none()
        && set.slight.is_none()
        && set.medium.is_none()
        && set.open.is_none()
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
        // full-set emotion with blink variants
        for m in ["closed", "slight", "medium", "open"] {
            touch(root.join(format!("calm/{m}.png")));
            touch(root.join(format!("calm/{m}-blink.png")));
        }
        // two-frame emotion, no blink
        touch(root.join("angry/closed.png"));
        touch(root.join("angry/open.png"));
        // two-frame emotion with only closed/open blink
        touch(root.join("surprised/closed.png"));
        touch(root.join("surprised/open.png"));
        touch(root.join("surprised/closed-blink.png"));
        touch(root.join("surprised/open-blink.png"));
        // incomplete -> skipped
        touch(root.join("broken/closed.png"));
        root
    }

    #[test]
    fn quantizer_full_set_picks_exact() {
        let d = tempdir_shim::Dir::new();
        let root = make_catalog_root(&d);
        let grid = AssetCatalog::load(&root).unwrap();
        let calm = grid.get("calm").unwrap();
        assert_eq!(
            resolve_frame(calm, MouthState::Closed, EyeState::Open).unwrap(),
            "calm/closed.png"
        );
        assert_eq!(
            resolve_frame(calm, MouthState::Medium, EyeState::Open).unwrap(),
            "calm/medium.png"
        );
        assert_eq!(
            resolve_frame(calm, MouthState::Open, EyeState::Closed).unwrap(),
            "calm/open-blink.png"
        );
        assert_eq!(
            resolve_frame(calm, MouthState::Slight, EyeState::Closed).unwrap(),
            "calm/slight-blink.png"
        );
    }

    #[test]
    fn blink_falls_back_to_eyes_open_when_absent() {
        let d = tempdir_shim::Dir::new();
        let root = make_catalog_root(&d);
        let grid = AssetCatalog::load(&root).unwrap();
        let angry = grid.get("angry").unwrap();
        // No blink art: a "closed eyes" request resolves to an eyes-open frame.
        assert_eq!(
            resolve_frame(angry, MouthState::Open, EyeState::Closed).unwrap(),
            "angry/open.png"
        );
        assert_eq!(
            resolve_frame(angry, MouthState::Medium, EyeState::Closed).unwrap(),
            "angry/open.png"
        );
    }

    #[test]
    fn blink_snaps_mouth_within_eyes_closed() {
        let d = tempdir_shim::Dir::new();
        let root = make_catalog_root(&d);
        let grid = AssetCatalog::load(&root).unwrap();
        let s = grid.get("surprised").unwrap();
        // eyes-closed only has closed(0) + open(3); medium(2) snaps to open.
        assert_eq!(
            resolve_frame(s, MouthState::Medium, EyeState::Closed).unwrap(),
            "surprised/open-blink.png"
        );
        assert_eq!(
            resolve_frame(s, MouthState::Slight, EyeState::Closed).unwrap(),
            "surprised/closed-blink.png"
        );
    }

    #[test]
    fn load_skips_incomplete_and_lowercases_keys() {
        let d = tempdir_shim::Dir::new();
        let root = make_catalog_root(&d);
        let cat = AssetCatalog::load(&root).expect("load");
        assert!(cat.get("calm").is_some());
        assert!(cat.get("angry").is_some());
        assert!(cat.get("surprised").is_some());
        assert!(cat.get("broken").is_none());

        // calm has blink art; angry does not.
        assert!(cat.get("calm").unwrap().eyes_closed.is_some());
        assert!(cat.get("angry").unwrap().eyes_closed.is_none());
    }

    #[test]
    fn frame_url_helper() {
        let d = tempdir_shim::Dir::new();
        let root = make_catalog_root(&d);
        let cat = AssetCatalog::load(&root).unwrap();
        let url = cat
            .frame_url("CALM", MouthState::Open, EyeState::Closed)
            .unwrap();
        assert_eq!(url, "/frames/calm/open-blink.png");
    }

    #[test]
    fn load_errors_when_empty() {
        let dir = tempdir_shim::Dir::new();
        fs::create_dir_all(dir.path().join("empty")).unwrap();
        let err = AssetCatalog::load(dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("no usable emotion folders"));
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
