//! Wire protocol shared between the Rust server and the web app.
//!
//! Messages use the adjacent-tagged envelope from the SDD:
//! `{"type":"TriggerEmotion","payload":{"emotion":"surprised"}}` — implemented
//! via `#[serde(tag = "type", content = "payload")]`.
//!
//! `MouthState` is the shared 4-level domain enum used by the audio analyser,
//! the state manager, the asset resolver, and the protocol.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The four mouth-aperture levels, ordered from resting to fully open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouthState {
    Closed,
    Slight,
    Medium,
    Open,
}

impl MouthState {
    /// Numeric rank in `[0, 3]`; higher means more open.
    pub const fn level(self) -> u8 {
        match self {
            MouthState::Closed => 0,
            MouthState::Slight => 1,
            MouthState::Medium => 2,
            MouthState::Open => 3,
        }
    }

    /// All variants, in order from closed to open.
    pub const ALL: [MouthState; 4] = [
        MouthState::Closed,
        MouthState::Slight,
        MouthState::Medium,
        MouthState::Open,
    ];

    /// Case-insensitive parse, tolerant of the JSON-serialized form.
    pub fn from_str_ci(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "closed" => Some(MouthState::Closed),
            "slight" => Some(MouthState::Slight),
            "medium" => Some(MouthState::Medium),
            "open" => Some(MouthState::Open),
            _ => None,
        }
    }
}

/// Resolved on-disk frames for one emotion, expressed as paths **relative to
/// the asset root** using forward slashes (URL-ready: prepend `/frames/`).
///
/// `closed` and `open` are mandatory; `slight`/`medium` are optional and the
/// resolver snaps to the nearest available frame when they are absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameSet {
    pub closed: String,
    pub slight: Option<String>,
    pub medium: Option<String>,
    pub open: String,
}

/// Catalog snapshot pushed to clients on connect: emotion name -> frames.
pub type Catalog = BTreeMap<String, FrameSet>;

/// Serializable avatar snapshot, used for the REST `GET /api/state` endpoint.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AvatarSnapshot {
    pub emotion: String,
    pub mouth: MouthState,
    pub volume: f32,
    pub overridden: bool,
    pub frame: String,
    pub default_emotion: String,
}

// ---------------------------------------------------------------------------
// Client -> Server
// ---------------------------------------------------------------------------

/// Messages accepted from web-app / API clients.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", content = "payload", rename_all = "PascalCase")]
pub enum ClientMessage {
    /// Switch to an emotion; auto-reverts after its configured timer (if any).
    TriggerEmotion { emotion: String },
    /// Drop any active emotion override and return to the default.
    ClearOverride,
    /// Change the resting emotion (applied when no override is active).
    SetDefault { emotion: String },
    /// Force a specific mouth shape, ignoring mic input.
    SetMouthOverride { mouth: MouthState },
    /// Release a forced mouth shape; resume mic-driven motion.
    ClearMouthOverride,
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
    /// Sent once on (re)connect: the full asset catalog (for preloading +
    /// button generation) and the current resting emotion.
    Welcome {
        catalog: Catalog,
        default_emotion: String,
    },
    /// Authoritative avatar state. Sent on every change and throttled for
    /// volume-only drift. `frame` is the resolved URL to display;
    /// `default_emotion` is the current resting emotion (so the UI can mark it).
    StateUpdate {
        emotion: String,
        mouth: MouthState,
        volume: f32,
        overridden: bool,
        frame: String,
        default_emotion: String,
    },
    /// Human-readable error (e.g. unknown emotion).
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_message_roundtrip_matches_sdd_envelope() {
        let json =
            r#"{"type":"TriggerEmotion","payload":{"emotion":"surprised"}}"#;
        let msg: ClientMessage = serde_json::from_str(json).expect("parse");
        match msg {
            ClientMessage::TriggerEmotion { emotion } => {
                assert_eq!(emotion, "surprised")
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
            emotion: "angry".into(),
            mouth: MouthState::Open,
            volume: 0.42,
            overridden: true,
            frame: "/frames/angry/open.png".into(),
            default_emotion: "calm".into(),
        };
        let s = serde_json::to_string(&msg).unwrap();
        let back: ServerMessage = serde_json::from_str(&s).unwrap();
        match back {
            ServerMessage::StateUpdate {
                emotion,
                mouth,
                volume,
                default_emotion,
                ..
            } => {
                assert_eq!(emotion, "angry");
                assert_eq!(mouth, MouthState::Open);
                assert!((volume - 0.42).abs() < 1e-9);
                assert_eq!(default_emotion, "calm");
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
        assert_eq!(MouthState::from_str_ci("OPEN"), Some(MouthState::Open));
    }
}
