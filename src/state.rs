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
//!
//! In the layered art model the avatar is a static body plus independent eye
//! and mouth layers. This task resolves the current eye/mouth layer URLs from
//! the catalog and sends them in every `StateUpdate`; the body never changes.

use crate::assets::AssetCatalog;
use crate::audio::EnvelopeControl;
use crate::compositor::{Compositor, Frame};
use crate::config::BlinkSettings;
use crate::protocol::{
    EnvelopeConfig, EyeState, MouthConfig, MouthState, ServerMessage,
};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, trace};

/// Minimum interval between volume-only broadcasts (meter refresh cap).
const VOLUME_BROADCAST_INTERVAL: Duration = Duration::from_millis(50);

/// Commands accepted by the state task.
#[derive(Debug, Clone)]
pub enum StateCommand {
    /// Smoothed RMS volume in roughly `[0.0, 1.0]`, from the audio analyser.
    SetVolume(f32),
    /// Client requested an emotion (eye-expression set). The caller (net layer)
    /// must have validated that the emotion exists in the catalog.
    TriggerEmotion(String),
    /// Client dropped the emotion override (manual clear).
    ClearOverride,
    /// Client changed the resting emotion.
    SetDefault(String),
    /// Client forced a mouth shape (disables mic-driven mouth until cleared).
    SetMouthOverride(MouthState),
    /// Client released a forced mouth shape.
    ClearMouthOverride,
    /// Client updated the mouth-level configuration (enabled + thresholds).
    SetMouthConfig(MouthConfig),
    /// Client updated the audio envelope (attack/release). Writes the shared
    /// atomics the realtime callback reads — applies to the live stream without
    /// a restart.
    SetEnvelope(EnvelopeConfig),
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

/// Map smoothed RMS volume to a mouth level using the active configuration.
///
/// The lowest enabled level is the resting mouth (returned at low volume); each
/// higher enabled level engages when volume reaches its threshold. Disabled
/// levels are skipped entirely, so turning off `closed` makes `partial` the
/// resting mouth (3-position mode).
pub fn volume_to_mouth(v: f32, cfg: &MouthConfig) -> MouthState {
    const ASCENDING: [MouthState; 4] = [
        MouthState::Closed,
        MouthState::Partial,
        MouthState::Medium,
        MouthState::Open,
    ];
    let resting = ASCENDING
        .iter()
        .find(|l| cfg.is_enabled(**l))
        .copied()
        .unwrap_or(MouthState::Closed);
    // Highest enabled level (above resting) whose threshold <= v.
    for &l in ASCENDING.iter().rev() {
        if l.level() <= resting.level() {
            break;
        }
        if !cfg.is_enabled(l) {
            continue;
        }
        if let Some(th) = cfg.threshold(l) {
            if v >= th {
                return l;
            }
        }
    }
    resting
}

/// Spawn the state task. Returns its `JoinHandle`. The caller retains the
/// `mpsc` sender (to feed commands) and the `broadcast` receiver handle.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    catalog: Arc<AssetCatalog>,
    compositor: Arc<Compositor>,
    mouth_config: MouthConfig,
    envelope: EnvelopeControl,
    timers: HashMap<String, f32>,
    default_emotion: String,
    cmd_tx: mpsc::UnboundedSender<StateCommand>,
    mut cmd_rx: mpsc::UnboundedReceiver<StateCommand>,
    bcast: broadcast::Sender<ServerMessage>,
    frame_tx: watch::Sender<Arc<Frame>>,
) -> JoinHandle<()> {
    let mut machine = StateMachine {
        catalog,
        compositor,
        mouth_config,
        envelope,
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
        frame_tx,
        last_sent_emotion: default_emotion,
        last_sent_mouth: MouthState::Closed,
        last_sent_eyes: EyeState::Open,
        last_frame_keys: (String::new(), String::new()),
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
    compositor: Arc<Compositor>,
    mouth_config: MouthConfig,
    envelope: EnvelopeControl,
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
    /// Pushes a freshly composited RGBA frame to the output sinks (browser +
    /// webcam) whenever the visible layers change.
    frame_tx: watch::Sender<Arc<Frame>>,
    last_sent_emotion: String,
    last_sent_mouth: MouthState,
    last_sent_eyes: EyeState,
    /// Last (eyes_frame, mouth_frame) we rendered — skips re-compositing when
    /// only the volume (meter) drifted but the visible frame didn't change.
    last_frame_keys: (String, String),
    last_vol_broadcast: Instant,
}

impl StateMachine {
    fn effective_emotion(&self) -> &str {
        self.emotion_override
            .as_deref()
            .unwrap_or(&self.default_emotion)
    }

    /// The emotion name to look up in the catalog for the eye layer, or `None`
    /// when no emotion is active (resting on the default/base eyes).
    fn emotion_for_eyes(&self) -> Option<&str> {
        let e = self.effective_emotion();
        if e.is_empty() {
            None
        } else {
            Some(e)
        }
    }

    fn effective_mouth(&self) -> MouthState {
        self.mouth_override.unwrap_or(self.mouth)
    }

    fn effective_eyes(&self) -> EyeState {
        self.eyes_override.unwrap_or(self.eyes)
    }

    /// The currently-displayed eye layer URL (for change detection).
    fn current_eyes_frame(&self) -> String {
        self.catalog
            .eyes_frame(self.emotion_for_eyes(), self.effective_eyes())
            .unwrap_or_default()
    }

    fn handle(&mut self, cmd: StateCommand) {
        match cmd {
            StateCommand::SetVolume(v) => {
                self.volume = v.clamp(0.0, 1.0);
                self.mouth = volume_to_mouth(self.volume, &self.mouth_config);
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
            StateCommand::SetMouthConfig(config) => {
                // A change to enablement can move the resting mouth (e.g.
                // disabling `closed` makes `partial` the resting level), so
                // recompute the mic-driven mouth and broadcast state too.
                self.mouth_config = config.clone();
                self.mouth = volume_to_mouth(self.volume, &self.mouth_config);
                let _ = self
                    .bcast
                    .send(ServerMessage::MouthConfigUpdate { config });
                self.broadcast_now();
            }
            StateCommand::SetEnvelope(config) => {
                // Write the shared atomics the realtime callback reads — applies
                // immediately to the live audio stream — then tell the panels.
                self.envelope.set(config.attack_ms, config.release_ms);
                let _ =
                    self.bcast.send(ServerMessage::EnvelopeUpdate { config });
            }
            StateCommand::BlinkClose => {
                // A manual eyes override pauses the scheduler entirely.
                if self.eyes_override.is_none() {
                    let before = self.current_eyes_frame();
                    self.eyes = EyeState::Closed;
                    // Only broadcast when the visible eye layer actually changes
                    // (e.g. an emotion with no `closed.png` resolves to the same
                    // `open` frame, so nothing is sent).
                    if self.current_eyes_frame() != before {
                        self.broadcast_now();
                    }
                }
            }
            StateCommand::BlinkOpen => {
                if self.eyes_override.is_none() {
                    let before = self.current_eyes_frame();
                    self.eyes = EyeState::Open;
                    if self.current_eyes_frame() != before {
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
        let eyes_frame = self
            .catalog
            .eyes_frame(
                if emotion.is_empty() {
                    None
                } else {
                    Some(emotion)
                },
                eyes,
            )
            .unwrap_or_default();
        let mouth_frame = self.catalog.mouth_frame(mouth).unwrap_or_default();
        let emotion_overridden = self.emotion_override.is_some();
        let mouth_overridden = self.mouth_override.is_some();
        let eyes_overridden = self.eyes_override.is_some();
        let overridden =
            emotion_overridden || mouth_overridden || eyes_overridden;
        let msg = ServerMessage::StateUpdate {
            emotion: emotion.to_string(),
            mouth,
            eyes,
            volume: self.volume,
            overridden,
            mouth_overridden,
            eyes_overridden,
            eyes_frame: eyes_frame.clone(),
            mouth_frame: mouth_frame.clone(),
            default_emotion: self.default_emotion.clone(),
        };
        // `send` errors only when there are no receivers; that's fine.
        let _ = self.bcast.send(msg);
        // Re-composite the avatar only when the visible layers actually
        // changed (skips volume-only meter drift), then push to sinks.
        let key = (eyes_frame, mouth_frame);
        if key != self.last_frame_keys {
            self.last_frame_keys = key;
            let emotion_opt = if emotion.is_empty() {
                None
            } else {
                Some(emotion)
            };
            let frame = self.compositor.render(emotion_opt, mouth, eyes);
            let _ = self.frame_tx.send(Arc::new(frame));
        }
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
    use crate::protocol::{EyeLayers, LayerCatalog, MouthLayers};
    use image::RgbaImage;
    use std::collections::BTreeMap;
    use std::fs;

    // Build a real on-disk layered catalog + compositor (the compositor decodes
    // actual PNGs), so the state task can render frames in tests.
    fn setup() -> (AssetCatalog, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "rt-state-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let touch = |rel: &str, rgba: &[u8]| {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            let img: RgbaImage =
                RgbaImage::from_raw(2, 2, rgba.to_vec()).unwrap();
            img.save(&p).unwrap();
        };
        touch("base/body.png", &[200, 0, 0, 255].repeat(4));
        for m in ["closed", "partial", "medium", "open"] {
            touch(&format!("mouths/{m}.png"), &[0, 255, 0, 255].repeat(4));
        }
        touch("eyes/open.png", &[255, 255, 255, 255].repeat(4));
        touch("eyes/closed.png", &[0, 0, 0, 255].repeat(4));
        touch("eyes/surprised/open.png", &[255, 0, 255, 255].repeat(4));
        touch("eyes/surprised/closed.png", &[0, 255, 255, 255].repeat(4));
        touch("eyes/flat/open.png", &[120, 120, 120, 255].repeat(4));

        let cat = AssetCatalog(LayerCatalog {
            base: vec!["base/body.png".into()],
            mouths: MouthLayers {
                closed: Some("mouths/closed.png".into()),
                partial: Some("mouths/partial.png".into()),
                medium: Some("mouths/medium.png".into()),
                open: Some("mouths/open.png".into()),
            },
            default_eyes: EyeLayers {
                open: Some("eyes/open.png".into()),
                closed: Some("eyes/closed.png".into()),
            },
            emotions: {
                let mut m = BTreeMap::new();
                m.insert(
                    "surprised".into(),
                    EyeLayers {
                        open: Some("eyes/surprised/open.png".into()),
                        closed: Some("eyes/surprised/closed.png".into()),
                    },
                );
                m.insert(
                    "flat".into(),
                    EyeLayers {
                        open: Some("eyes/flat/open.png".into()),
                        closed: None,
                    },
                );
                m
            },
        });
        (cat, root)
    }

    fn mouth_config() -> MouthConfig {
        MouthConfig::all_enabled(0.02, 0.08, 0.18)
    }

    async fn harness() -> (
        mpsc::UnboundedSender<StateCommand>,
        broadcast::Receiver<ServerMessage>,
        JoinHandle<()>,
    ) {
        let (cat, root) = setup();
        let catalog = Arc::new(cat);
        let compositor =
            Arc::new(Compositor::new(catalog.clone(), &root).unwrap());
        let mut timers = HashMap::new();
        timers.insert("surprised".into(), 0.1_f32); // short for tests
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (bcast_tx, bcast_rx) = broadcast::channel(64);
        let init = compositor.render(None, MouthState::Closed, EyeState::Open);
        let (frame_tx, _frame_rx) = watch::channel(Arc::new(init));
        let handle = spawn(
            catalog,
            compositor,
            mouth_config(),
            crate::audio::EnvelopeControl::new(6.0, 110.0),
            timers,
            String::new(), // no resting emotion -> default eyes
            cmd_tx.clone(),
            cmd_rx,
            bcast_tx,
            frame_tx,
        );
        (cmd_tx, bcast_rx, handle)
    }

    #[derive(Debug)]
    struct S {
        emotion: String,
        mouth: MouthState,
        eyes: EyeState,
        volume: f32,
        eyes_frame: String,
        mouth_frame: String,
    }

    fn unwrap_state(msg: ServerMessage) -> S {
        match msg {
            ServerMessage::StateUpdate {
                emotion,
                mouth,
                eyes,
                volume,
                overridden: _,
                mouth_overridden: _,
                eyes_overridden: _,
                eyes_frame,
                mouth_frame,
                default_emotion: _,
            } => S {
                emotion,
                mouth,
                eyes,
                volume,
                eyes_frame,
                mouth_frame,
            },
            other => panic!("expected StateUpdate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn volume_maps_to_mouth_and_broadcasts_layers() {
        let (tx, mut rx, _h) = harness().await;
        tx.send(StateCommand::SetVolume(0.5)).unwrap(); // above open(0.18) -> Open
        let s = unwrap_state(rx.recv().await.unwrap());
        assert_eq!(s.mouth, MouthState::Open);
        assert_eq!(s.eyes, EyeState::Open);
        // Resting state uses default eyes + the resolved mouth layer.
        assert_eq!(s.eyes_frame, "/frames/eyes/open.png");
        assert_eq!(s.mouth_frame, "/frames/mouths/open.png");
    }

    #[tokio::test]
    async fn disabling_closed_makes_partial_the_resting_mouth() {
        // 3-position mode: closed off → partial is resting, medium/open engage
        // at their thresholds. Validates the A/B-test + optimization path.
        let cfg = MouthConfig {
            enabled: [false, true, true, true],
            partial: 0.02,
            medium: 0.08,
            open: 0.18,
        };
        // silence → partial (resting), never closed
        assert_eq!(volume_to_mouth(0.0, &cfg), MouthState::Partial);
        assert_eq!(volume_to_mouth(0.01, &cfg), MouthState::Partial);
        // crosses medium
        assert_eq!(volume_to_mouth(0.09, &cfg), MouthState::Medium);
        // crosses open
        assert_eq!(volume_to_mouth(0.5, &cfg), MouthState::Open);
    }

    #[tokio::test]
    async fn emotion_swaps_eye_layer_and_timer_reverts() {
        let (tx, mut rx, _h) = harness().await;
        tx.send(StateCommand::TriggerEmotion("surprised".into()))
            .unwrap();
        let s = unwrap_state(rx.recv().await.unwrap());
        assert_eq!(s.emotion, "surprised");
        assert_eq!(s.eyes_frame, "/frames/eyes/surprised/open.png");
        // Mouth layer is shared across emotions.
        assert_eq!(s.mouth_frame, "/frames/mouths/closed.png");

        // timer is 0.1s; should revert to default eyes.
        tokio::time::sleep(Duration::from_millis(250)).await;
        let mut eyes_frame = s.eyes_frame.clone();
        while let Ok(m) = rx.try_recv() {
            eyes_frame = unwrap_state(m).eyes_frame;
        }
        assert_eq!(eyes_frame, "/frames/eyes/open.png");
    }

    #[tokio::test]
    async fn stale_timer_does_not_clobber_newer_emotion() {
        let (tx, mut rx, _h) = harness().await;
        tx.send(StateCommand::TriggerEmotion("surprised".into()))
            .unwrap();
        let _ = rx.recv().await.unwrap();
        tx.send(StateCommand::TriggerEmotion("flat".into()))
            .unwrap();
        let s = unwrap_state(rx.recv().await.unwrap());
        assert_eq!(s.emotion, "flat");

        tokio::time::sleep(Duration::from_millis(250)).await;
        let mut still = "flat".to_string();
        while let Ok(m) = rx.try_recv() {
            still = unwrap_state(m).emotion;
        }
        assert_eq!(still, "flat", "newer emotion must remain in effect");
    }

    #[tokio::test]
    async fn manual_clear_override_returns_to_default() {
        let (tx, mut rx, _h) = harness().await;
        tx.send(StateCommand::TriggerEmotion("surprised".into()))
            .unwrap();
        let _ = rx.recv().await.unwrap();
        tx.send(StateCommand::SetDefault("flat".into())).unwrap();
        let _ = rx.recv().await.unwrap();
        tx.send(StateCommand::ClearOverride).unwrap();
        let s = unwrap_state(rx.recv().await.unwrap());
        assert_eq!(s.emotion, "flat");
        assert_eq!(s.eyes_frame, "/frames/eyes/flat/open.png");
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
        assert_eq!(s.mouth_frame, "/frames/mouths/closed.png");

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
    async fn blink_swaps_eye_layer_to_closed() {
        let (tx, mut rx, _h) = harness().await;
        tx.send(StateCommand::SetVolume(0.5)).unwrap(); // mouth Open
        let _ = unwrap_state(rx.recv().await.unwrap());

        tx.send(StateCommand::BlinkClose).unwrap();
        let s = unwrap_state(rx.recv().await.unwrap());
        assert_eq!(s.eyes, EyeState::Closed);
        assert_eq!(s.eyes_frame, "/frames/eyes/closed.png");

        tx.send(StateCommand::BlinkOpen).unwrap();
        let s = unwrap_state(rx.recv().await.unwrap());
        assert_eq!(s.eyes, EyeState::Open);
        assert_eq!(s.eyes_frame, "/frames/eyes/open.png");
    }

    #[tokio::test]
    async fn emotion_without_blink_art_does_not_change_frame() {
        let (tx, mut rx, _h) = harness().await;
        // "flat" has only open.png; a blink must not change the visible frame.
        tx.send(StateCommand::TriggerEmotion("flat".into()))
            .unwrap();
        let _ = unwrap_state(rx.recv().await.unwrap());
        tx.send(StateCommand::BlinkClose).unwrap();
        // No broadcast expected (eye frame unchanged) — nothing to read.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            rx.try_recv().is_err(),
            "no broadcast when blink has no effect"
        );
    }

    #[tokio::test]
    async fn eyes_override_masks_blink() {
        let (tx, mut rx, _h) = harness().await;
        tx.send(StateCommand::SetVolume(0.5)).unwrap();
        let _ = unwrap_state(rx.recv().await.unwrap());

        tx.send(StateCommand::SetEyesOverride(EyeState::Open))
            .unwrap();
        let _ = unwrap_state(rx.recv().await.unwrap());
        tx.send(StateCommand::BlinkClose).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err(), "override must mask the blink");

        tx.send(StateCommand::SetEyesOverride(EyeState::Closed))
            .unwrap();
        let s = unwrap_state(rx.recv().await.unwrap());
        assert_eq!(s.eyes, EyeState::Closed);
        tx.send(StateCommand::ClearEyesOverride).unwrap();
        let s = unwrap_state(rx.recv().await.unwrap());
        assert_eq!(s.eyes, EyeState::Open);
    }

    #[tokio::test]
    async fn override_flags_are_broadcast_per_channel() {
        let (tx, mut rx, _h) = harness().await;

        tx.send(StateCommand::SetVolume(0.5)).unwrap();
        let (emotion_ovr, mouth_ovr, eyes_ovr) =
            unwrap_override_flags(&rx.recv().await.unwrap());
        assert!(!emotion_ovr && !mouth_ovr && !eyes_ovr);

        tx.send(StateCommand::SetMouthOverride(MouthState::Open))
            .unwrap();
        let (_, mouth_ovr, eyes_ovr) =
            unwrap_override_flags(&rx.recv().await.unwrap());
        assert!(mouth_ovr);
        assert!(!eyes_ovr);

        tx.send(StateCommand::TriggerEmotion("surprised".into()))
            .unwrap();
        let (emotion_ovr, mouth_ovr, _) =
            unwrap_override_flags(&rx.recv().await.unwrap());
        assert!(emotion_ovr);
        assert!(mouth_ovr);

        tx.send(StateCommand::SetEyesOverride(EyeState::Closed))
            .unwrap();
        let (_, _, eyes_ovr) = unwrap_override_flags(&rx.recv().await.unwrap());
        assert!(eyes_ovr);

        tx.send(StateCommand::ClearMouthOverride).unwrap();
        let (_, mouth_ovr, eyes_ovr) =
            unwrap_override_flags(&rx.recv().await.unwrap());
        assert!(!mouth_ovr);
        assert!(eyes_ovr);
    }

    fn unwrap_override_flags(msg: &ServerMessage) -> (bool, bool, bool) {
        match msg {
            ServerMessage::StateUpdate {
                overridden,
                mouth_overridden,
                eyes_overridden,
                ..
            } => (*overridden, *mouth_overridden, *eyes_overridden),
            other => panic!("expected StateUpdate, got {other:?}"),
        }
    }
}
