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
use crate::config::{BlinkSettings, ThresholdSettings};
use crate::protocol::{EyeState, MouthState, ServerMessage};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
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
    /// Internal: the blink scheduler closed the eyes.
    BlinkClose,
    /// Internal: the blink scheduler opened the eyes.
    BlinkOpen,
    /// Client forced the eyes open/closed (disables the blink scheduler's
    /// effect until cleared).
    SetEyesOverride(EyeState),
    /// Client released a forced eye state; resume blinking.
    ClearEyesOverride,
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
        eyes_override: None,
        mouth: MouthState::Closed,
        eyes: EyeState::Open,
        volume: 0.0,
        active_token: 0,
        cmd_tx,
        bcast,
        last_sent_emotion: default_emotion,
        last_sent_mouth: MouthState::Closed,
        last_sent_eyes: EyeState::Open,
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
    eyes_override: Option<EyeState>,
    /// Mic-derived mouth (ignored while `mouth_override` is `Some`).
    mouth: MouthState,
    /// Blink-derived eyes (ignored while `eyes_override` is `Some`).
    eyes: EyeState,
    volume: f32,
    /// Bumped on every emotion trigger; stale timers carry an older token.
    active_token: u64,
    cmd_tx: mpsc::UnboundedSender<StateCommand>,
    bcast: broadcast::Sender<ServerMessage>,
    last_sent_emotion: String,
    last_sent_mouth: MouthState,
    last_sent_eyes: EyeState,
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

    fn effective_eyes(&self) -> EyeState {
        self.eyes_override.unwrap_or(self.eyes)
    }

    /// The currently-displayed frame URL (for change detection).
    fn current_frame(&self) -> String {
        self.catalog
            .frame_url(
                self.effective_emotion(),
                self.effective_mouth(),
                self.effective_eyes(),
            )
            .unwrap_or_default()
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
                let eyes = self.effective_eyes();
                let changed = emotion != self.last_sent_emotion
                    || mouth != self.last_sent_mouth
                    || eyes != self.last_sent_eyes;
                if changed
                    || now.duration_since(self.last_vol_broadcast)
                        >= VOLUME_BROADCAST_INTERVAL
                {
                    self.broadcast(&emotion, mouth, eyes, now);
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
            StateCommand::BlinkClose => {
                // A manual eyes override pauses the scheduler entirely.
                if self.eyes_override.is_none() {
                    let before = self.current_frame();
                    self.eyes = EyeState::Closed;
                    // Only broadcast when the visible frame actually changes
                    // (e.g. an emotion with no blink art resolves to the same
                    // eyes-open frame, so nothing is sent).
                    if self.current_frame() != before {
                        self.broadcast_now();
                    }
                }
            }
            StateCommand::BlinkOpen => {
                if self.eyes_override.is_none() {
                    let before = self.current_frame();
                    self.eyes = EyeState::Open;
                    if self.current_frame() != before {
                        self.broadcast_now();
                    }
                }
            }
            StateCommand::SetEyesOverride(eyes) => {
                self.eyes_override = Some(eyes);
                self.broadcast_now();
            }
            StateCommand::ClearEyesOverride => {
                self.eyes_override = None;
                self.broadcast_now();
            }
            StateCommand::Shutdown => unreachable!("handled by the run loop"),
        }
    }

    /// Always broadcast the current state (used for discrete client commands).
    fn broadcast_now(&mut self) {
        let emotion = self.effective_emotion().to_string();
        let mouth = self.effective_mouth();
        let eyes = self.effective_eyes();
        self.broadcast(&emotion, mouth, eyes, Instant::now());
    }

    fn broadcast(
        &mut self,
        emotion: &str,
        mouth: MouthState,
        eyes: EyeState,
        now: Instant,
    ) {
        let frame = self
            .catalog
            .frame_url(emotion, mouth, eyes)
            .unwrap_or_default();
        let overridden = self.emotion_override.is_some()
            || self.mouth_override.is_some()
            || self.eyes_override.is_some();
        let msg = ServerMessage::StateUpdate {
            emotion: emotion.to_string(),
            mouth,
            eyes,
            volume: self.volume,
            overridden,
            frame,
            default_emotion: self.default_emotion.clone(),
        };
        // `send` errors only when there are no receivers; that's fine.
        let _ = self.bcast.send(msg);
        self.last_sent_emotion = emotion.to_string();
        self.last_sent_mouth = mouth;
        self.last_sent_eyes = eyes;
        self.last_vol_broadcast = now;
    }
}

/// Spawn the blink scheduler. It posts `BlinkClose`/`BlinkOpen` commands at
/// randomised intervals until the command channel closes (on shutdown), at
/// which point the next send fails and the loop exits. No-op if `cfg.enabled`
/// is false. A manual eyes override (`SetEyesOverride`) masks blinks without
/// stopping the scheduler.
pub fn spawn_blink_scheduler(
    cmd_tx: mpsc::UnboundedSender<StateCommand>,
    cfg: BlinkSettings,
) {
    if !cfg.enabled {
        debug!("blink scheduler disabled by config");
        return;
    }
    tokio::spawn(async move {
        let mut rng = StdRng::from_entropy();
        loop {
            let interval = rng.gen_range(cfg.min_interval..=cfg.max_interval);
            tokio::time::sleep(Duration::from_secs_f32(interval)).await;
            if cmd_tx.send(StateCommand::BlinkClose).is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_secs_f32(cfg.duration)).await;
            if cmd_tx.send(StateCommand::BlinkOpen).is_err() {
                return;
            }
            // Occasional double-blink: short gap then another close/open.
            if rng.gen_bool(f64::from(cfg.double_chance)) {
                tokio::time::sleep(Duration::from_secs_f32(cfg.duration) * 2)
                    .await;
                if cmd_tx.send(StateCommand::BlinkClose).is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_secs_f32(cfg.duration)).await;
                if cmd_tx.send(StateCommand::BlinkOpen).is_err() {
                    return;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::AssetCatalog;
    use crate::protocol::{FrameGrid, MouthSet};
    use std::collections::BTreeMap;

    fn mouth(
        closed: &str,
        slight: Option<&str>,
        medium: Option<&str>,
        open: &str,
    ) -> MouthSet {
        MouthSet {
            closed: Some(closed.into()),
            slight: slight.map(Into::into),
            medium: medium.map(Into::into),
            open: Some(open.into()),
        }
    }

    fn fake_catalog() -> AssetCatalog {
        let mut m = BTreeMap::new();
        // calm: full eyes-open + full blink set
        m.insert(
            "calm".into(),
            FrameGrid {
                eyes_open: mouth(
                    "calm/closed.png",
                    Some("calm/slight.png"),
                    Some("calm/medium.png"),
                    "calm/open.png",
                ),
                eyes_closed: Some(mouth(
                    "calm/closed-blink.png",
                    Some("calm/slight-blink.png"),
                    Some("calm/medium-blink.png"),
                    "calm/open-blink.png",
                )),
            },
        );
        // surprised: only closed/open eyes-open, no blink art
        m.insert(
            "surprised".into(),
            FrameGrid {
                eyes_open: mouth(
                    "surprised/closed.png",
                    None,
                    None,
                    "surprised/open.png",
                ),
                eyes_closed: None,
            },
        );
        // angry: closed/open eyes-open, partial blink (closed+open only)
        m.insert(
            "angry".into(),
            FrameGrid {
                eyes_open: mouth(
                    "angry/closed.png",
                    None,
                    None,
                    "angry/open.png",
                ),
                eyes_closed: Some(mouth(
                    "angry/closed-blink.png",
                    None,
                    None,
                    "angry/open-blink.png",
                )),
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

    #[derive(Debug)]
    struct S {
        emotion: String,
        mouth: MouthState,
        eyes: EyeState,
        volume: f32,
        frame: String,
        default_emotion: String,
    }

    fn unwrap_state(msg: ServerMessage) -> S {
        match msg {
            ServerMessage::StateUpdate {
                emotion,
                mouth,
                eyes,
                volume,
                overridden: _,
                frame,
                default_emotion,
            } => S {
                emotion,
                mouth,
                eyes,
                volume,
                frame,
                default_emotion,
            },
            other => panic!("expected StateUpdate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn volume_maps_to_mouth_and_broadcasts() {
        let (tx, mut rx, _h) = harness().await;
        tx.send(StateCommand::SetVolume(0.5)).unwrap(); // above open(0.18) -> Open
        let s = unwrap_state(rx.recv().await.unwrap());
        assert_eq!(s.emotion, "calm");
        assert_eq!(s.mouth, MouthState::Open);
        assert_eq!(s.eyes, EyeState::Open);
        assert!((s.volume - 0.5).abs() < 1e-9);
        assert_eq!(s.frame, "/frames/calm/open.png");
        assert_eq!(s.default_emotion, "calm");
    }

    #[tokio::test]
    async fn emotion_override_and_timer_revert() {
        let (tx, mut rx, _h) = harness().await;
        tx.send(StateCommand::TriggerEmotion("surprised".into()))
            .unwrap();
        let s = unwrap_state(rx.recv().await.unwrap());
        assert_eq!(s.emotion, "surprised");

        // timer is 0.1s; should revert to calm.
        tokio::time::sleep(Duration::from_millis(250)).await;
        let mut last = String::new();
        while let Ok(m) = rx.try_recv() {
            last = unwrap_state(m).emotion;
        }
        assert_eq!(last, "calm", "should have reverted to calm");
    }

    #[tokio::test]
    async fn stale_timer_does_not_clobber_newer_emotion() {
        let (tx, mut rx, _h) = harness().await;
        // surprised has a 0.1s timer; calm has none.
        tx.send(StateCommand::TriggerEmotion("surprised".into()))
            .unwrap();
        let _ = rx.recv().await.unwrap();
        tx.send(StateCommand::TriggerEmotion("calm".into()))
            .unwrap();
        let s = unwrap_state(rx.recv().await.unwrap());
        assert_eq!(s.emotion, "calm");

        tokio::time::sleep(Duration::from_millis(250)).await;
        let mut still = String::from("calm");
        while let Ok(m) = rx.try_recv() {
            still = unwrap_state(m).emotion;
        }
        assert_eq!(still, "calm", "newer emotion must remain in effect");
    }

    #[tokio::test]
    async fn manual_clear_override_returns_to_default() {
        let (tx, mut rx, _h) = harness().await;
        tx.send(StateCommand::TriggerEmotion("calm".into()))
            .unwrap();
        let _ = rx.recv().await.unwrap();
        tx.send(StateCommand::SetDefault("surprised".into()))
            .unwrap();
        let _ = rx.recv().await.unwrap();
        tx.send(StateCommand::ClearOverride).unwrap();
        let s = unwrap_state(rx.recv().await.unwrap());
        assert_eq!(s.emotion, "surprised");
    }

    #[tokio::test]
    async fn mouth_override_ignores_mic() {
        let (tx, mut rx, _h) = harness().await;
        tx.send(StateCommand::SetVolume(0.5)).unwrap();
        let s = unwrap_state(rx.recv().await.unwrap());
        assert_eq!(s.mouth, MouthState::Open);

        tx.send(StateCommand::SetMouthOverride(MouthState::Closed))
            .unwrap();
        let s = unwrap_state(rx.recv().await.unwrap());
        assert_eq!(s.mouth, MouthState::Closed);

        tokio::time::sleep(
            VOLUME_BROADCAST_INTERVAL + Duration::from_millis(20),
        )
        .await;
        tx.send(StateCommand::SetVolume(0.5)).unwrap();
        let s = unwrap_state(rx.recv().await.unwrap());
        assert_eq!(s.mouth, MouthState::Closed, "override must win over mic");
        assert!((s.volume - 0.5).abs() < 1e-9);
    }

    #[tokio::test]
    async fn blink_swaps_to_closed_eye_frame() {
        let (tx, mut rx, _h) = harness().await;
        // Establish eyes-open baseline on calm.
        tx.send(StateCommand::SetVolume(0.5)).unwrap(); // mouth Open
        let _ = unwrap_state(rx.recv().await.unwrap());

        tx.send(StateCommand::BlinkClose).unwrap();
        let s = unwrap_state(rx.recv().await.unwrap());
        assert_eq!(s.eyes, EyeState::Closed);
        assert_eq!(s.frame, "/frames/calm/open-blink.png");

        tx.send(StateCommand::BlinkOpen).unwrap();
        let s = unwrap_state(rx.recv().await.unwrap());
        assert_eq!(s.eyes, EyeState::Open);
        assert_eq!(s.frame, "/frames/calm/open.png");
    }

    #[tokio::test]
    async fn blink_falls_back_to_eyes_open_without_blink_art() {
        let (tx, mut rx, _h) = harness().await;
        tx.send(StateCommand::TriggerEmotion("surprised".into()))
            .unwrap();
        let _ = unwrap_state(rx.recv().await.unwrap());
        tx.send(StateCommand::SetVolume(0.5)).unwrap(); // mouth Open on surprised
        let _ = unwrap_state(rx.recv().await.unwrap());

        // surprised has no blink art: BlinkClose must NOT change the frame.
        tx.send(StateCommand::BlinkClose).unwrap();
        // No broadcast is expected (effective eyes unchanged) — nothing to read.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            rx.try_recv().is_err(),
            "no broadcast when blink has no effect"
        );
    }

    #[tokio::test]
    async fn blink_snaps_mouth_within_eyes_closed() {
        let (tx, mut rx, _h) = harness().await;
        // angry has blink art only for closed+open. Medium snaps to open-blink.
        tx.send(StateCommand::TriggerEmotion("angry".into()))
            .unwrap();
        let _ = unwrap_state(rx.recv().await.unwrap());
        tx.send(StateCommand::SetVolume(0.1)).unwrap(); // mouth Medium (>=0.08,<0.18)
        let _ = unwrap_state(rx.recv().await.unwrap());
        tx.send(StateCommand::BlinkClose).unwrap();
        let s = unwrap_state(rx.recv().await.unwrap());
        assert_eq!(s.frame, "/frames/angry/open-blink.png");
    }

    #[tokio::test]
    async fn eyes_override_masks_blink() {
        let (tx, mut rx, _h) = harness().await;
        tx.send(StateCommand::SetVolume(0.5)).unwrap();
        let _ = unwrap_state(rx.recv().await.unwrap());

        // Force eyes open: a subsequent BlinkClose must not change them.
        tx.send(StateCommand::SetEyesOverride(EyeState::Open))
            .unwrap();
        let _ = unwrap_state(rx.recv().await.unwrap());
        tx.send(StateCommand::BlinkClose).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err(), "override must mask the blink");

        // Force eyes closed and clear: blinking resumes.
        tx.send(StateCommand::SetEyesOverride(EyeState::Closed))
            .unwrap();
        let s = unwrap_state(rx.recv().await.unwrap());
        assert_eq!(s.eyes, EyeState::Closed);
        tx.send(StateCommand::ClearEyesOverride).unwrap();
        let s = unwrap_state(rx.recv().await.unwrap());
        // After clearing, effective eyes == self.eyes which was last set Open by BlinkOpen? No
        // BlinkOpen never fired; self.eyes is still Open from init. So effective -> Open.
        assert_eq!(s.eyes, EyeState::Open);
    }
}
