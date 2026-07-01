//! Wire protocol shared between the Rust server and the web app.
//!
//! Messages use the adjacent-tagged envelope:
//! `{"type":"TriggerEmotion","payload":{"emotion":"happy"}}` — implemented via
//! `#[serde(tag = "type", content = "payload")]`.
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
//! The server resolves the current eye/mouth layer URLs and sends them in every
//! `StateUpdate`, so the web client only has to stack three `<img>` layers.

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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
/// callback; adjustable live from the panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// The full layered asset catalog, sent to clients on connect.
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

/// Serializable avatar snapshot, used for the REST `GET /api/state` endpoint.
/// Serialize-only server-side; clients parse `StateUpdate` over the WS instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvatarSnapshot {
    pub emotion: String,
    pub mouth: MouthState,
    pub eyes: EyeState,
    pub volume: f32,
    /// True while any override (emotion / mouth / eyes) is active.
    pub overridden: bool,
    /// True while a mouth override is active (client uses this for UI state).
    pub mouth_overridden: bool,
    /// True while an eyes override is active (client uses this for UI state).
    pub eyes_overridden: bool,
    /// Resolved `/frames/...` URL for the current eye layer.
    pub eyes_frame: String,
    /// Resolved `/frames/...` URL for the current mouth layer.
    pub mouth_frame: String,
    pub default_emotion: String,
}

// ---------------------------------------------------------------------------
// Client -> Server
// ---------------------------------------------------------------------------

/// Messages accepted from web-app / API clients.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", content = "payload", rename_all = "PascalCase")]
pub enum ClientMessage {
    /// Switch to an emotion (eye-expression set); auto-reverts after its
    /// configured timer (if any).
    TriggerEmotion { emotion: String },
    /// Drop any active emotion override and return to the resting emotion.
    ClearOverride,
    /// Change the resting emotion (applied when no override is active).
    SetDefault { emotion: String },
    /// Force a specific mouth shape, ignoring mic input.
    SetMouthOverride { mouth: MouthState },
    /// Release a forced mouth shape; resume mic-driven motion.
    ClearMouthOverride,
    /// Update the mouth-level configuration (enabled levels + thresholds).
    SetMouthConfig { config: MouthConfig },
    /// Update the audio envelope (attack/release).
    SetEnvelope { config: EnvelopeConfig },
    /// Force the eyes open or closed, ignoring the blink scheduler.
    SetEyesOverride { eyes: EyeState },
    /// Release a forced eye state; resume blinking.
    ClearEyesOverride,
    /// First message a client sends after connecting (optional handshake).
    Hello,
}

// ---------------------------------------------------------------------------
// Server -> Client
// ---------------------------------------------------------------------------

/// Messages pushed from the server to connected clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "PascalCase")]
pub enum ServerMessage {
    /// Sent once on (re)connect: the full layered asset catalog (for the panel
    /// UI), the current resting emotion, the mouth-level configuration, and the
    /// composited frame dimensions (so the client sizes its canvas for the raw
    /// RGBA frames it receives over the binary WS channel).
    Welcome {
        catalog: LayerCatalog,
        default_emotion: String,
        mouth_config: MouthConfig,
        envelope: EnvelopeConfig,
        latency: String,
        frame_width: u32,
        frame_height: u32,
    },
    /// Authoritative avatar state. Sent on every change and throttled for
    /// volume-only drift. `eyes_frame` / `mouth_frame` are the resolved layer
    /// URLs to display; the base layer comes from the `Welcome` catalog and
    /// never changes. `overridden` is true if any override is active;
    /// `mouth_overridden` / `eyes_overridden` flag the per-channel overrides so
    /// every client highlights the same control without tracking local state.
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
    /// Human-readable error (e.g. unknown emotion).
    Error { message: String },
    /// Broadcast whenever the mouth-level configuration changes (via panel or
    /// REST) so every connected panel stays in sync. OBS sources ignore this.
    MouthConfigUpdate { config: MouthConfig },
    /// Broadcast whenever the audio envelope changes.
    EnvelopeUpdate { config: EnvelopeConfig },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_message_roundtrip_matches_envelope() {
        let json = r#"{"type":"TriggerEmotion","payload":{"emotion":"happy"}}"#;
        let msg: ClientMessage = serde_json::from_str(json).expect("parse");
        match msg {
            ClientMessage::TriggerEmotion { emotion } => {
                assert_eq!(emotion, "happy")
            }
            other => panic!("unexpected variant {other:?}"),
        }
    }

    #[test]
    fn clear_override_has_no_payload() {
        let json = r#"{"type":"ClearOverride"}"#;
        let msg: ClientMessage = serde_json::from_str(json).expect("parse");
        assert!(matches!(msg, ClientMessage::ClearOverride));
    }

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
    fn welcome_catalog_roundtrips() {
        let cat = LayerCatalog {
            base: vec!["base/body.png".into()],
            mouths: MouthLayers {
                closed: Some("mouths/closed.png".into()),
                partial: None,
                medium: None,
                open: Some("mouths/open.png".into()),
            },
            default_eyes: EyeLayers {
                open: Some("eyes/open.png".into()),
                closed: Some("eyes/closed.png".into()),
            },
            emotions: BTreeMap::new(),
        };
        let welcome = ServerMessage::Welcome {
            catalog: cat,
            default_emotion: "default".into(),
            mouth_config: MouthConfig::all_enabled(0.02, 0.08, 0.18),
            envelope: EnvelopeConfig {
                attack_ms: 6.0,
                release_ms: 110.0,
            },
            latency: "low".into(),
            frame_width: 921,
            frame_height: 921,
        };
        let s = serde_json::to_string(&welcome).unwrap();
        let back: ServerMessage = serde_json::from_str(&s).unwrap();
        match back {
            ServerMessage::Welcome {
                catalog,
                default_emotion,
                frame_width,
                ..
            } => {
                assert_eq!(catalog.base, vec!["base/body.png".to_string()]);
                assert_eq!(default_emotion, "default");
                assert_eq!(frame_width, 921);
            }
            _ => panic!("wrong variant"),
        }
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
