//! Data-driven asset catalog.
//!
//! Walks the configured asset root once at startup and builds a map of
//! `emotion -> FrameSet`. `closed.png` and `open.png` are mandatory per
//! emotion folder; `slight.png` / `medium.png` are optional. The
//! [`resolve_frame`] quantizer snaps a desired [`MouthState`] to the nearest
//! available frame when an intermediate frame is missing, so a 2-frame emotion
//! (e.g. only `closed` + `open`) still works across all four mouth levels.

use crate::protocol::{FrameSet, MouthState};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;
use tracing::warn;

/// URL prefix under which the HTTP layer serves the asset root.
pub const FRAMES_URL_PREFIX: &str = "/frames";

/// The on-disk asset catalog: emotion name (lower-cased) -> frame set.
#[derive(Debug, Clone)]
pub struct AssetCatalog(pub BTreeMap<String, FrameSet>);

impl AssetCatalog {
    /// Scan `asset_root` and build the catalog.
    pub fn load(asset_root: &Path) -> Result<Self> {
        let mut map: BTreeMap<String, FrameSet> = BTreeMap::new();

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

            let closed_disk = dir_path.join("closed.png");
            let open_disk = dir_path.join("open.png");
            if !closed_disk.exists() || !open_disk.exists() {
                warn!(
                    emotion = %dir_name,
                    "skipping emotion folder: missing closed.png and/or open.png"
                );
                continue;
            }

            let slight = dir_path
                .join("slight.png")
                .exists()
                .then(|| format!("{dir_name}/slight.png"));
            let medium = dir_path
                .join("medium.png")
                .exists()
                .then(|| format!("{dir_name}/medium.png"));

            let frames = FrameSet {
                closed: format!("{dir_name}/closed.png"),
                slight,
                medium,
                open: format!("{dir_name}/open.png"),
            };

            if map.contains_key(&key) {
                warn!(emotion = %key, "duplicate emotion folder (case-insensitive); overwriting");
            }
            map.insert(key, frames);
        }

        if map.is_empty() {
            bail!(
                "no usable emotion folders found under {} (each needs closed.png + open.png)",
                asset_root.display()
            );
        }

        Ok(AssetCatalog(map))
    }

    /// Look up the frames for an emotion name (case-insensitive).
    pub fn get(&self, emotion: &str) -> Option<&FrameSet> {
        self.0.get(&emotion.to_ascii_lowercase())
    }

    /// Sorted emotion names.
    pub fn emotions(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// Borrow the underlying emotion -> frames map (for the wire catalog).
    pub fn catalog(&self) -> &crate::protocol::Catalog {
        &self.0
    }

    /// Resolve the absolute URL path for the best frame for `mouth` under
    /// `emotion`, or `None` if the emotion is unknown.
    pub fn frame_url(
        &self,
        emotion: &str,
        mouth: MouthState,
    ) -> Option<String> {
        self.get(emotion).map(|f| frame_url(f, mouth))
    }
}

/// Pick the on-disk frame for a mouth level, snapping to the nearest available.
///
/// Tie-breaks toward the *less open* frame to avoid flicker (e.g. with only
/// `closed` + `open`, `Medium` resolves to `open` but `Slight` resolves to
/// `closed`).
pub fn resolve_frame(frames: &FrameSet, mouth: MouthState) -> &str {
    let desired = mouth.level();
    let mut best: Option<(u8, &str)> = None;
    for cand in 0..=3u8 {
        let Some(path) = level_path(frames, cand) else {
            continue;
        };
        let dist = cand.abs_diff(desired);
        match &best {
            None => best = Some((dist, path)),
            Some((bd, _)) if dist < *bd => best = Some((dist, path)),
            _ => {}
        }
    }
    // closed + open are mandatory, so there is always a winner.
    best.expect("FrameSet always contains closed and open").1
}

fn level_path(frames: &FrameSet, level: u8) -> Option<&str> {
    match level {
        0 => Some(frames.closed.as_str()),
        1 => frames.slight.as_deref(),
        2 => frames.medium.as_deref(),
        3 => Some(frames.open.as_str()),
        _ => None,
    }
}

/// Build the servable URL (`/frames/<rel>`) for a mouth level.
pub fn frame_url(frames: &FrameSet, mouth: MouthState) -> String {
    format!("{FRAMES_URL_PREFIX}/{}", resolve_frame(frames, mouth))
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
        // full-set emotion
        touch(root.join("calm/closed.png"));
        touch(root.join("calm/slight.png"));
        touch(root.join("calm/medium.png"));
        touch(root.join("calm/open.png"));
        // two-frame emotion
        touch(root.join("angry/closed.png"));
        touch(root.join("angry/open.png"));
        // incomplete -> skipped
        touch(root.join("broken/closed.png"));
        root
    }

    #[test]
    fn quantizer_full_set_picks_exact() {
        let frames = FrameSet {
            closed: "c".into(),
            slight: Some("s".into()),
            medium: Some("m".into()),
            open: "o".into(),
        };
        assert_eq!(resolve_frame(&frames, MouthState::Closed), "c");
        assert_eq!(resolve_frame(&frames, MouthState::Slight), "s");
        assert_eq!(resolve_frame(&frames, MouthState::Medium), "m");
        assert_eq!(resolve_frame(&frames, MouthState::Open), "o");
    }

    #[test]
    fn quantizer_two_frame_snaps_nearest() {
        let frames = FrameSet {
            closed: "c".into(),
            slight: None,
            medium: None,
            open: "o".into(),
        };
        // slight (1) is distance 1 from closed(0), distance 2 from open(3) -> closed
        assert_eq!(resolve_frame(&frames, MouthState::Slight), "c");
        // medium (2) is distance 2 from closed, distance 1 from open -> open
        assert_eq!(resolve_frame(&frames, MouthState::Medium), "o");
        assert_eq!(resolve_frame(&frames, MouthState::Closed), "c");
        assert_eq!(resolve_frame(&frames, MouthState::Open), "o");
    }

    #[test]
    fn quantizer_tie_breaks_to_less_open() {
        // only closed(0) + medium(2); Slight(1) is equidistant -> resolves to closed
        let frames = FrameSet {
            closed: "c".into(),
            slight: None,
            medium: Some("m".into()),
            open: "o".into(),
        };
        assert_eq!(resolve_frame(&frames, MouthState::Slight), "c");
    }

    #[test]
    fn load_builds_catalog_and_skips_incomplete() {
        let dir = tempdir_shim::Dir::new();
        let root = make_catalog_root(&dir);
        let cat = AssetCatalog::load(&root).expect("load");

        assert!(cat.get("calm").is_some());
        assert!(cat.get("angry").is_some());
        assert!(
            cat.get("broken").is_none(),
            "incomplete folder must be skipped"
        );

        let url = cat.frame_url("angry", MouthState::Medium).unwrap();
        assert_eq!(url, "/frames/angry/open.png");

        let url2 = cat.frame_url("CALM", MouthState::Open).unwrap();
        assert_eq!(url2, "/frames/calm/open.png");
    }

    #[test]
    fn load_errors_when_empty() {
        let dir = tempdir_shim::Dir::new();
        // a folder with no pngs -> nothing usable
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
