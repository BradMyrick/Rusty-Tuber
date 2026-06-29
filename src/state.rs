//! Avatar state machine: the single source of truth for what the avatar shows.
//!
//! One async task owns all mutable state and mutates it only in response to
//! [`StateCommand`]s received on an mpsc channel. Emotion auto-revert timers
//! are implemented as spawned sleeps that post a [`StateCommand::TimerClear`]
//! carrying the token that was current when they were armed; the handler only
//! honours the clear if the token still matches, so a newer emotion trigger
//! cannot be clobbered by a stale timer (the bug in the original SDD design).
//!
//! State changes are broadcast to all subscribers (the WebSocket layer) on a
//! `tokio::sync::broadcast` channel. Volume-only drift is throttled to ~20 Hz
//! so the web-app meter stays lively without flooding slow clients.

use crate::assets::AssetCatalog;
use crate::config::ThresholdSettings;
use crate::protocol::{MouthState, ServerMessage};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, trace};

/// Minimum interval between volume-only broadcasts (meter refresh cap).
const VOLUME_BROADCAST_INTERVAL: Duration = Duration::from_millis(50);

/// Commands accepted by the state task.
#[derive(Debug, Clone)]
pub enum StateCommand {
    /// Smoothed RMS volume in roughly `[0.0, 1.0]`, from the audio analyser.
    SetVolume(f32),
    /// Client requested an emotion. The caller (net layer) must have validated
    /// that the emotion exists in the catalog.
    TriggerEmotion(String),
    /// Client dropped the emotion override (manual clear).
    ClearOverride,
    /// Client changed the resting emotion.
    SetDefault(String),
    /// Client forced a mouth shape (disables mic-driven mouth until cleared).
    SetMouthOverride(MouthState),
    /// Client released a forced mouth shape.
    ClearMouthOverride,
    /// Internal: a revert-timer fired. Only applied if `token` equals the
    /// currently active token.
    TimerClear(u64),
    /// Stop the state task.
    Shutdown,
}

/// Map smoothed RMS volume to a mouth level using the configured thresholds.
pub fn volume_to_mouth(v: f32, t: &ThresholdSettings) -> MouthState {
    if v >= t.open {
        MouthState::Open
    } else if v >= t.medium {
        MouthState::Medium
    } else if v >= t.slight {
        MouthState::Slight
    } else {
        MouthState::Closed
    }
}

/// Spawn the state task. Returns its `JoinHandle`. The caller retains the
/// `mpsc` sender (to feed commands) and the `broadcast` receiver handle.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    catalog: Arc<AssetCatalog>,
    thresholds: ThresholdSettings,
    timers: HashMap<String, f32>,
    default_emotion: String,
    cmd_tx: mpsc::UnboundedSender<StateCommand>,
    mut cmd_rx: mpsc::UnboundedReceiver<StateCommand>,
    bcast: broadcast::Sender<ServerMessage>,
) -> JoinHandle<()> {
    let mut machine = StateMachine {
        catalog,
        thresholds,
        timers,
        default_emotion: default_emotion.clone(),
        emotion_override: None,
        mouth_override: None,
        mouth: MouthState::Closed,
        volume: 0.0,
        active_token: 0,
        cmd_tx,
        bcast,
        last_sent_emotion: default_emotion,
        last_sent_mouth: MouthState::Closed,
        last_vol_broadcast: Instant::now(),
    };

    tokio::spawn(async move {
        debug!("state task started");
        while let Some(cmd) = cmd_rx.recv().await {
            if matches!(cmd, StateCommand::Shutdown) {
                debug!("state task shutting down");
                break;
            }
            machine.handle(cmd);
        }
    })
}

struct StateMachine {
    catalog: Arc<AssetCatalog>,
    thresholds: ThresholdSettings,
    timers: HashMap<String, f32>,
    default_emotion: String,
    emotion_override: Option<String>,
    mouth_override: Option<MouthState>,
    /// Mic-derived mouth (ignored while `mouth_override` is `Some`).
    mouth: MouthState,
    volume: f32,
    /// Bumped on every emotion trigger; stale timers carry an older token.
    active_token: u64,
    cmd_tx: mpsc::UnboundedSender<StateCommand>,
    bcast: broadcast::Sender<ServerMessage>,
    last_sent_emotion: String,
    last_sent_mouth: MouthState,
    last_vol_broadcast: Instant,
}

impl StateMachine {
    fn effective_emotion(&self) -> &str {
        self.emotion_override
            .as_deref()
            .unwrap_or(&self.default_emotion)
    }

    fn effective_mouth(&self) -> MouthState {
        self.mouth_override.unwrap_or(self.mouth)
    }

    fn handle(&mut self, cmd: StateCommand) {
        match cmd {
            StateCommand::SetVolume(v) => {
                self.volume = v.clamp(0.0, 1.0);
                self.mouth = volume_to_mouth(self.volume, &self.thresholds);
                // High-frequency path: dedupe identical visible state and
                // throttle volume-only drift to keep the meter fresh.
                let now = Instant::now();
                let emotion = self.effective_emotion().to_string();
                let mouth = self.effective_mouth();
                let changed = emotion != self.last_sent_emotion
                    || mouth != self.last_sent_mouth;
                if changed
                    || now.duration_since(self.last_vol_broadcast)
                        >= VOLUME_BROADCAST_INTERVAL
                {
                    self.broadcast(&emotion, mouth, now);
                }
            }
            StateCommand::TriggerEmotion(emotion) => {
                self.emotion_override = Some(emotion.clone());
                self.active_token = self.active_token.wrapping_add(1);
                let token = self.active_token;
                if let Some(&secs) = self.timers.get(&emotion) {
                    let tx = self.cmd_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs_f32(secs)).await;
                        // Best-effort: if the channel closed, the task is done.
                        let _ = tx.send(StateCommand::TimerClear(token));
                    });
                }
                self.broadcast_now();
            }
            StateCommand::ClearOverride => {
                self.emotion_override = None;
                self.broadcast_now();
            }
            StateCommand::TimerClear(token) => {
                if token == self.active_token {
                    trace!(token, "honouring timer clear");
                    self.emotion_override = None;
                    self.broadcast_now();
                } else {
                    trace!(
                        token,
                        active = self.active_token,
                        "ignoring stale timer"
                    );
                }
            }
            StateCommand::SetDefault(emotion) => {
                self.default_emotion = emotion;
                self.broadcast_now();
            }
            StateCommand::SetMouthOverride(mouth) => {
                self.mouth_override = Some(mouth);
                self.broadcast_now();
            }
            StateCommand::ClearMouthOverride => {
                self.mouth_override = None;
                self.broadcast_now();
            }
            StateCommand::Shutdown => unreachable!("handled by the run loop"),
        }
    }

    /// Always broadcast the current state (used for discrete client commands).
    fn broadcast_now(&mut self) {
        let emotion = self.effective_emotion().to_string();
        let mouth = self.effective_mouth();
        self.broadcast(&emotion, mouth, Instant::now());
    }

    fn broadcast(&mut self, emotion: &str, mouth: MouthState, now: Instant) {
        let frame = self.catalog.frame_url(emotion, mouth).unwrap_or_default();
        let overridden =
            self.emotion_override.is_some() || self.mouth_override.is_some();
        let msg = ServerMessage::StateUpdate {
            emotion: emotion.to_string(),
            mouth,
            volume: self.volume,
            overridden,
            frame,
            default_emotion: self.default_emotion.clone(),
        };
        // `send` errors only when there are no receivers; that's fine.
        let _ = self.bcast.send(msg);
        self.last_sent_emotion = emotion.to_string();
        self.last_sent_mouth = mouth;
        self.last_vol_broadcast = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::AssetCatalog;
    use crate::protocol::FrameSet;
    use std::collections::BTreeMap;

    fn fake_catalog() -> AssetCatalog {
        let mut m = BTreeMap::new();
        m.insert(
            "calm".into(),
            FrameSet {
                closed: "calm/closed.png".into(),
                slight: Some("calm/slight.png".into()),
                medium: Some("calm/medium.png".into()),
                open: "calm/open.png".into(),
            },
        );
        m.insert(
            "surprised".into(),
            FrameSet {
                closed: "surprised/closed.png".into(),
                slight: None,
                medium: None,
                open: "surprised/open.png".into(),
            },
        );
        AssetCatalog(m)
    }

    fn thresholds() -> ThresholdSettings {
        ThresholdSettings {
            slight: 0.02,
            medium: 0.08,
            open: 0.18,
        }
    }

    async fn harness() -> (
        mpsc::UnboundedSender<StateCommand>,
        broadcast::Receiver<ServerMessage>,
        JoinHandle<()>,
    ) {
        let catalog = Arc::new(fake_catalog());
        let mut timers = HashMap::new();
        timers.insert("surprised".into(), 0.1_f32); // short for tests
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (bcast_tx, bcast_rx) = broadcast::channel(64);
        let handle = spawn(
            catalog,
            thresholds(),
            timers,
            "calm".into(),
            cmd_tx.clone(),
            cmd_rx,
            bcast_tx,
        );
        (cmd_tx, bcast_rx, handle)
    }

    fn unwrap_state(
        msg: ServerMessage,
    ) -> (String, MouthState, f32, bool, String, String) {
        match msg {
            ServerMessage::StateUpdate {
                emotion,
                mouth,
                volume,
                overridden,
                frame,
                default_emotion,
            } => (emotion, mouth, volume, overridden, frame, default_emotion),
            other => panic!("expected StateUpdate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn volume_maps_to_mouth_and_broadcasts() {
        let (tx, mut rx, _h) = harness().await;
        tx.send(StateCommand::SetVolume(0.5)).unwrap(); // above open(0.18) -> Open
        let msg = rx.recv().await.unwrap();
        let (emotion, mouth, volume, _ov, frame, default) = unwrap_state(msg);
        assert_eq!(emotion, "calm");
        assert_eq!(mouth, MouthState::Open);
        assert!((volume - 0.5).abs() < 1e-9);
        assert_eq!(frame, "/frames/calm/open.png");
        assert_eq!(default, "calm");
    }

    #[tokio::test]
    async fn emotion_override_and_timer_revert() {
        let (tx, mut rx, _h) = harness().await;
        tx.send(StateCommand::TriggerEmotion("surprised".into()))
            .unwrap();
        let m1 = rx.recv().await.unwrap();
        assert_eq!(unwrap_state(m1).0, "surprised");

        // timer is 0.1s; should revert to calm.
        tokio::time::sleep(Duration::from_millis(250)).await;
        // drain any messages
        let mut last = String::new();
        while let Ok(m) = rx.try_recv() {
            last = unwrap_state(m).0;
        }
        assert_eq!(last, "calm", "should have reverted to calm");
    }

    #[tokio::test]
    async fn stale_timer_does_not_clobber_newer_emotion() {
        let (tx, mut rx, _h) = harness().await;
        // surprised has a 0.1s timer; pleased has none.
        tx.send(StateCommand::TriggerEmotion("surprised".into()))
            .unwrap();
        let _ = rx.recv().await.unwrap();
        // Immediately override with a different emotion before the timer fires.
        tx.send(StateCommand::TriggerEmotion("calm".into()))
            .unwrap();
        let m = rx.recv().await.unwrap();
        assert_eq!(unwrap_state(m).0, "calm");

        // Wait past the surprised timer; the stale clear must NOT take effect.
        tokio::time::sleep(Duration::from_millis(250)).await;
        let mut still = String::from("calm");
        while let Ok(m) = rx.try_recv() {
            still = unwrap_state(m).0;
        }
        assert_eq!(still, "calm", "newer emotion must remain in effect");
    }

    #[tokio::test]
    async fn manual_clear_override_returns_to_default() {
        let (tx, mut rx, _h) = harness().await;
        // Trigger an emotion with no timer (calm has none) so it sticks.
        tx.send(StateCommand::TriggerEmotion("calm".into()))
            .unwrap();
        let _ = rx.recv().await.unwrap();
        tx.send(StateCommand::SetDefault("surprised".into()))
            .unwrap();
        let _ = rx.recv().await.unwrap();
        tx.send(StateCommand::ClearOverride).unwrap();
        let m = rx.recv().await.unwrap();
        // After clear, effective emotion == default == surprised
        assert_eq!(unwrap_state(m).0, "surprised");
    }

    #[tokio::test]
    async fn mouth_override_ignores_mic() {
        let (tx, mut rx, _h) = harness().await;
        // Establish a loud baseline so the mic-driven mouth is Open.
        tx.send(StateCommand::SetVolume(0.5)).unwrap();
        let m = rx.recv().await.unwrap();
        assert_eq!(unwrap_state(m).1, MouthState::Open);

        // Pin the mouth to Closed regardless of input.
        tx.send(StateCommand::SetMouthOverride(MouthState::Closed))
            .unwrap();
        let m = rx.recv().await.unwrap();
        assert_eq!(unwrap_state(m).1, MouthState::Closed);

        // Wait past the volume throttle, then push another loud sample. The
        // override must win, so the effective mouth stays Closed.
        tokio::time::sleep(
            VOLUME_BROADCAST_INTERVAL + Duration::from_millis(20),
        )
        .await;
        tx.send(StateCommand::SetVolume(0.5)).unwrap();
        let m = rx.recv().await.unwrap();
        let (_, mouth, volume, _, _, _) = unwrap_state(m);
        assert_eq!(mouth, MouthState::Closed, "override must win over mic");
        assert!((volume - 0.5).abs() < 1e-9);
    }
}
