//! Avatar domain model: mouth levels, eye state, the layered asset catalog, and
//! the envelope/mouth tuning types shared across config, state, assets, and the
//! compositor.
//!
//! ## Layered art model
//!
//! The avatar is a stack of independent transparent PNG layers (all sharing the
//! same canvas size), composited bottom-up:
//!
//! ```text
//!   base/body.png         (static body — never changes)
//!   eyes/open|closed.png  (eye layer; "closed" is shown during a blink)
//!   mouths/<level>.png    (mouth layer; level driven by mic volume)
//! ```
//!
//! A **mouth level** is one of `closed < partial < medium < open`. Only
//! `closed` and `open` are required; `partial`/`medium` are optional and the
//! resolver snaps to the nearest available level.
//!
//! An **emotion** is an optional named eye-expression set under
//! `eyes/<emotion>/open.png` + `closed.png`. Triggering an emotion swaps the eye
//! layer to that expression; the mouth keeps reacting to the mic. With no
//! emotion art, the resting avatar is just base + default eyes + mic mouth.
//!
//! ## Observation seam
//!
//! [`ServerMessage`] is posted on a `tokio::sync::broadcast` channel whenever
//! the visible avatar state changes. The headless binary has no subscribers
//! (sends are free), but a program embedding this crate can subscribe to mirror
//! or log avatar state. The serde tagging is kept stable for that purpose:
//! `{"type":"StateUpdate","payload":{...}}`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The four mouth-aperture levels, ordered from resting to fully open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouthState {
    Closed,
    Partial,
    Medium,
    Open,
}

impl MouthState {
    /// Numeric rank in `[0, 3]`; higher means more open.
    pub const fn level(self) -> u8 {
        match self {
            MouthState::Closed => 0,
            MouthState::Partial => 1,
            MouthState::Medium => 2,
            MouthState::Open => 3,
        }
    }

    /// Case-insensitive parse, tolerant of the JSON-serialized form.
    pub fn from_str_ci(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "closed" => Some(MouthState::Closed),
            "partial" => Some(MouthState::Partial),
            "medium" => Some(MouthState::Medium),
            "open" => Some(MouthState::Open),
            _ => None,
        }
    }
}

/// Runtime-configurable mouth mapping: which levels are active and the volume
/// threshold at which each (non-resting) level engages.
///
/// `enabled` is indexed by [`MouthState::level()`] — `[closed, partial, medium,
/// open]`. The lowest enabled level is the resting mouth (shown at silence);
/// each higher enabled level engages when volume reaches its threshold.
/// Disable `closed` to make `partial` the resting mouth and A/B-test 3 vs 4
/// positions. Thresholds must stay strictly ordered `partial < medium < open`
/// regardless of which levels are enabled (disabled levels' thresholds are
/// simply unused until re-enabled).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MouthConfig {
    pub enabled: [bool; 4],
    pub partial: f32,
    pub medium: f32,
    pub open: f32,
}

impl MouthConfig {
    /// All four levels enabled with the given thresholds.
    pub fn all_enabled(partial: f32, medium: f32, open: f32) -> Self {
        MouthConfig {
            enabled: [true, true, true, true],
            partial,
            medium,
            open,
        }
    }

    pub fn is_enabled(&self, level: MouthState) -> bool {
        self.enabled[level.level() as usize]
    }

    /// Activation threshold for a level, or `None` for `Closed` (the resting
    /// level has no threshold — it's the default at low volume).
    pub fn threshold(&self, level: MouthState) -> Option<f32> {
        match level {
            MouthState::Partial => Some(self.partial),
            MouthState::Medium => Some(self.medium),
            MouthState::Open => Some(self.open),
            MouthState::Closed => None,
        }
    }

    /// Enforce: at least one level enabled, thresholds strictly ordered
    /// `partial < medium < open`, all finite and in `[0, 1]`.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled.iter().any(|&e| e) {
            return Err("at least one mouth level must be enabled".into());
        }
        for v in [self.partial, self.medium, self.open] {
            if !v.is_finite() || !(0.0..=1.0).contains(&v) {
                return Err(format!(
                    "thresholds must be finite and in [0.0, 1.0] (got {v})"
                ));
            }
        }
        if !(self.partial < self.medium && self.medium < self.open) {
            return Err(format!(
                "thresholds must satisfy partial < medium < open (got partial={}, medium={}, open={})",
                self.partial, self.medium, self.open
            ));
        }
        Ok(())
    }
}

/// Audio envelope tuning: `attack_ms` is how fast the mouth opens on talk-start
/// (smaller = snappier), `release_ms` how gently it closes on talk-end (larger =
/// smoother, less flutter). Drives the asymmetric one-pole envelope in the audio
/// callback; configured via `[audio]` in `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvelopeConfig {
    pub attack_ms: f32,
    pub release_ms: f32,
}

impl EnvelopeConfig {
    pub fn validate(&self) -> Result<(), String> {
        for v in [self.attack_ms, self.release_ms] {
            if !v.is_finite() || !(0.1..=2000.0).contains(&v) {
                return Err(format!(
                    "attack_ms/release_ms must be in [0.1, 2000] ms (got {v})"
                ));
            }
        }
        Ok(())
    }
}

/// Eye state. `Open` is the resting state; `Closed` is shown during a blink.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum EyeState {
    #[default]
    Open,
    Closed,
}

impl EyeState {
    /// Case-insensitive parse, tolerant of the JSON-serialized form.
    pub fn from_str_ci(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "open" => Some(EyeState::Open),
            "closed" => Some(EyeState::Closed),
            _ => None,
        }
    }
}

/// The four mouth frames (paths relative to the asset root). `closed` and
/// `open` are required anchors; `partial`/`medium` are optional.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MouthLayers {
    pub closed: Option<String>,
    pub partial: Option<String>,
    pub medium: Option<String>,
    pub open: Option<String>,
}

/// A pair of eye frames for one expression: `open` is the resting frame, and
/// `closed` (optional) is shown during a blink. When `closed` is absent the
/// resolver falls back to `open`, i.e. that expression simply doesn't blink.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EyeLayers {
    pub open: Option<String>,
    pub closed: Option<String>,
}

/// The resolved on-disk asset catalog: base layers + mouths + the default eye
/// expression + optional emotion eye-sets.
///
/// - `base` is stacked bottom-up (filename order) and rendered statically.
/// - `default_eyes` is the resting expression.
/// - `emotions` maps an emotion name to its eye-expression set (optional).
/// - `mouths` is shared across all emotions (mic-driven).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LayerCatalog {
    pub base: Vec<String>,
    pub mouths: MouthLayers,
    pub default_eyes: EyeLayers,
    pub emotions: BTreeMap<String, EyeLayers>,
}

/// Avatar-state observation messages, posted on the library's `broadcast`
/// channel (see [`crate::run`]). The headless binary has no subscribers, so
/// these sends are effectively free; embedding code can subscribe to mirror or
/// log state. Tagged as `{"type":"...","payload":{...}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "PascalCase")]
pub enum ServerMessage {
    /// Authoritative avatar state. Posted on every visible change and throttled
    /// for volume-only drift. `eyes_frame` / `mouth_frame` are the resolved
    /// layer paths to composite; the base layer is static and never changes.
    /// `overridden` is true if any override is active; `mouth_overridden` /
    /// `eyes_overridden` flag the per-channel overrides.
    StateUpdate {
        emotion: String,
        mouth: MouthState,
        eyes: EyeState,
        volume: f32,
        overridden: bool,
        mouth_overridden: bool,
        eyes_overridden: bool,
        eyes_frame: String,
        mouth_frame: String,
        default_emotion: String,
    },
    /// Posted when the mouth-level configuration changes, so subscribers stay
    /// in sync.
    MouthConfigUpdate { config: MouthConfig },
    /// Posted when the audio envelope changes.
    EnvelopeUpdate { config: EnvelopeConfig },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_state_update_roundtrips() {
        let msg = ServerMessage::StateUpdate {
            emotion: "happy".into(),
            mouth: MouthState::Open,
            eyes: EyeState::Closed,
            volume: 0.42,
            overridden: true,
            mouth_overridden: false,
            eyes_overridden: true,
            eyes_frame: "/frames/eyes/happy/closed.png".into(),
            mouth_frame: "/frames/mouths/open.png".into(),
            default_emotion: "default".into(),
        };
        let s = serde_json::to_string(&msg).unwrap();
        let back: ServerMessage = serde_json::from_str(&s).unwrap();
        match back {
            ServerMessage::StateUpdate {
                emotion,
                mouth,
                eyes,
                volume,
                eyes_overridden,
                eyes_frame,
                mouth_frame,
                ..
            } => {
                assert_eq!(emotion, "happy");
                assert_eq!(mouth, MouthState::Open);
                assert_eq!(eyes, EyeState::Closed);
                assert!((volume - 0.42).abs() < 1e-9);
                assert!(eyes_overridden);
                assert_eq!(eyes_frame, "/frames/eyes/happy/closed.png");
                assert_eq!(mouth_frame, "/frames/mouths/open.png");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn mouth_state_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&MouthState::Medium).unwrap(),
            r#""medium""#
        );
        assert_eq!(
            MouthState::from_str_ci("PARTIAL"),
            Some(MouthState::Partial)
        );
    }

    #[test]
    fn mouth_config_validates_order_and_enablement() {
        assert!(MouthConfig::all_enabled(0.02, 0.08, 0.18)
            .validate()
            .is_ok());
        // equal thresholds rejected
        assert!(MouthConfig::all_enabled(0.05, 0.05, 0.1)
            .validate()
            .is_err());
        // nothing enabled rejected
        assert!(MouthConfig {
            enabled: [false; 4],
            partial: 0.02,
            medium: 0.08,
            open: 0.18,
        }
        .validate()
        .is_err());
    }
}
