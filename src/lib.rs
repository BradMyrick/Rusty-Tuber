//! Rusty-Tuber library crate: configuration, asset catalog, audio analysis,
//! avatar state machine, the virtual-webcam compositor, and a dependency-free
//! stdin control interface. The `rusty-tuber` binary in
//! [`src/main.rs`](../main.rs) is a thin CLI wrapper over these modules.

pub mod assets;
pub mod audio;
pub mod compositor;
pub mod config;
pub mod control;
pub mod protocol;
pub mod state;
#[cfg(target_os = "linux")]
pub mod webcam;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};

/// Bring up the headless avatar pipeline for a validated [`config::AppConfig`].
///
/// Loads the asset catalog, spawns the state machine + render thread + blink /
/// animation schedulers, starts the virtual-webcam sink and audio capture, then
/// runs a stdin command reader (the control seam — see [`control`]) until a
/// graceful shutdown signal arrives (`Ctrl-C` / `SIGTERM` / the `quit` command).
pub async fn run(cfg: config::AppConfig) -> Result<()> {
    // --- Asset catalog -----------------------------------------------------
    let catalog = Arc::new(assets::AssetCatalog::load(&cfg.engine.asset_root)?);
    info!(
        emotions = ?catalog.emotions().collect::<Vec<_>>(),
        base = ?catalog.catalog().base,
        "loaded asset catalog"
    );
    // Resolve the resting emotion: an empty value or a known emotion is used
    // as-is (empty means the base/default eyes); an unknown name falls back to
    // the default eyes with a warning.
    let default_emotion = if cfg.engine.default_emotion.is_empty()
        || catalog.has_emotion(&cfg.engine.default_emotion)
    {
        cfg.engine.default_emotion.clone()
    } else {
        warn!(
            configured = %cfg.engine.default_emotion,
            "configured default emotion is not in the catalog; using the default eyes"
        );
        String::new()
    };

    // --- Channels & state task ---------------------------------------------
    let mouth_config = cfg.thresholds.to_mouth_config()?;
    let envelope =
        audio::EnvelopeControl::new(cfg.audio.attack_ms, cfg.audio.release_ms);
    let compositor = Arc::new(compositor::Compositor::new(
        catalog.clone(),
        &cfg.engine.asset_root,
        Some(cfg.webcam.output_size),
    )?);
    let anim_count: usize =
        compositor.anim_config().iter().map(|c| c.instances).sum();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<state::StateCommand>();
    // Broadcast channel is retained as the observation seam: a future control
    // server subscribes to it to mirror avatar state. With no subscribers the
    // sends are effectively free (immediate `Err`), so it costs nothing to keep.
    let (bcast_tx, _) = broadcast::channel::<protocol::ServerMessage>(256);

    // Render pipeline: state task → RenderRequest (watch) → render thread →
    // Frame (watch) → webcam. The state task never blocks on compositing; the
    // render thread always works from the latest state.
    let (render_tx, render_rx) =
        tokio::sync::watch::channel(state::RenderRequest {
            emotion: None,
            mouth: protocol::MouthState::Closed,
            eyes: protocol::EyeState::Open,
            anim_frames: vec![0; anim_count],
            version: 0,
        });
    let init_frame = compositor.render(
        if default_emotion.is_empty() {
            None
        } else {
            Some(&default_emotion)
        },
        protocol::MouthState::Closed,
        protocol::EyeState::Open,
        &[],
    );
    let (frame_tx, _) =
        tokio::sync::watch::channel(std::sync::Arc::new(init_frame));
    #[cfg(target_os = "linux")]
    let webcam_rx = frame_tx.subscribe();

    // Producer cadence matches the device's advertised frame rate so renders
    // never outpace what the webcam can sink (and what it advertises via S_PARM).
    let render_frame_min =
        std::time::Duration::from_secs_f32(1.0 / cfg.webcam.fps as f32);
    state::spawn_renderer(
        compositor.clone(),
        render_rx,
        frame_tx.clone(),
        render_frame_min,
    );

    let state_handle = state::spawn(
        catalog.clone(),
        mouth_config.clone(),
        envelope.clone(),
        cfg.timers.clone(),
        default_emotion.clone(),
        anim_count,
        cmd_tx.clone(),
        cmd_rx,
        bcast_tx.clone(),
        render_tx,
    );

    state::spawn_blink_scheduler(cmd_tx.clone(), cfg.blink.clone());
    state::spawn_anim_scheduler(cmd_tx.clone(), compositor.anim_config());

    // --- Virtual webcam sink (Linux v4l2loopback) --------------------------
    #[cfg(target_os = "linux")]
    if cfg.webcam.enabled {
        match (webcam::find_device(&cfg.webcam.device), cfg.webcam.background_rgb()) {
            (Some(dev), Ok(solid)) => {
                match webcam::Background::load(
                    &cfg.webcam.background_image,
                    solid,
                    cfg.webcam.output_size,
                ) {
                    Ok(bg) => {
                        if let Err(e) = webcam::spawn_webcam(
                            webcam_rx,
                            dev.clone(),
                            bg,
                            cfg.webcam.format,
                            cfg.webcam.fps,
                            cfg.webcam.steady,
                        ) {
                            warn!(error = %format!("{e:#}"), "webcam output disabled");
                        }
                    }
                    Err(e) => warn!(
                        error = %format!("{e:#}"),
                        "webcam disabled (could not load [webcam].background_image)",
                    ),
                }
            }
            (None, _) => warn!(
                "no v4l2loopback device found; webcam disabled. \
                 install/enable with: sudo apt install v4l2loopback-dkms && sudo modprobe v4l2loopback"
            ),
            (_, Err(e)) => warn!(error = %format!("{e:#}"), "webcam disabled (bad config)"),
        }
    }
    #[cfg(not(target_os = "linux"))]
    if cfg.webcam.enabled {
        warn!("webcam output is only supported on Linux; ignoring [webcam].enabled");
    }

    // --- Audio capture (dedicated OS thread; cpal Stream lives there) -------
    let audio_cfg = cfg.audio.clone();
    let audio_cmd_tx = cmd_tx.clone();
    let audio_env = envelope.clone();
    std::thread::spawn(move || {
        match audio::start(&audio_cfg, audio_env, audio_cmd_tx) {
            Ok(_stream) => {
                info!("audio capture running");
                // Hold the stream alive for the lifetime of the process.
                std::thread::park();
            }
            Err(e) => error!(
                error = %format!("{e:#}"),
                "audio capture failed; avatar will rest with a closed mouth \
                 (pipe commands to stdin to drive it manually)"
            ),
        }
    });

    // --- Control interface (stdin) -----------------------------------------
    // The seam for hotkeys / a future server: simple text commands on stdin,
    // parsed into [`state::StateCommand`]s. See the [`control`] module.
    control::spawn_stdin(cmd_tx.clone(), catalog.clone());

    info!(
        "rusty-tuber running headless; type `help` on stdin for commands, \
         Ctrl-C to quit"
    );

    // --- Run until shutdown ------------------------------------------------
    // Shutdown is either an OS signal or the state task exiting (which happens
    // when the `quit` command sends `Shutdown`). Polling a JoinHandle after it
    // completed panics, so we track which branch fired and only await when the
    // state task is still running.
    let mut state_handle = state_handle;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let mut state_done = false;
    tokio::select! {
        _ = &mut shutdown => info!("received shutdown signal"),
        _ = &mut state_handle => {
            info!("control interface requested shutdown");
            state_done = true;
        }
    }

    // --- Tear down ---------------------------------------------------------
    if !state_done {
        let _ = cmd_tx.send(state::StateCommand::Shutdown);
        let _ = state_handle.await;
    }
    info!("shutdown complete");
    Ok(())
}

/// Wait for Ctrl-C / SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("installing Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )
        .expect("installing SIGTERM handler")
        .recv()
        .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("received Ctrl-C, shutting down"),
        _ = terminate => info!("received SIGTERM, shutting down"),
    }
}
