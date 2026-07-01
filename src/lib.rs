//! Rusty-Tuber library crate: configuration, asset catalog, audio analysis,
//! avatar state machine, and the HTTP/WebSocket server. The `rusty-tuber`
//! binary in [`src/main.rs`](../main.rs) is a thin CLI wrapper over these
//! modules.

pub mod assets;
pub mod audio;
pub mod config;
pub mod net;
pub mod protocol;
pub mod state;

use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::{error, info, warn};

/// Bring up the full server for a validated [`config::AppConfig`].
///
/// Loads the asset catalog, spawns the state task, starts audio capture on a
/// dedicated OS thread, builds the HTTP/WS router, and serves until a graceful
/// shutdown signal arrives.
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
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<state::StateCommand>();
    let (bcast_tx, _) = broadcast::channel::<protocol::ServerMessage>(256);

    let state_handle = state::spawn(
        catalog.clone(),
        mouth_config.clone(),
        cfg.timers.clone(),
        default_emotion.clone(),
        cmd_tx.clone(),
        cmd_rx,
        bcast_tx.clone(),
    );

    // Eye-blink scheduler: posts BlinkClose/BlinkOpen at randomised intervals
    // until the command channel closes on shutdown.
    state::spawn_blink_scheduler(cmd_tx.clone(), cfg.blink.clone());

    // --- Shared HTTP/WS state ----------------------------------------------
    let app_state = Arc::new(net::AppState::new(
        catalog.clone(),
        default_emotion,
        cmd_tx.clone(),
        bcast_tx.clone(),
        Arc::new(RwLock::new(None)),
        Arc::new(RwLock::new(mouth_config)),
    ));
    let _recorder = net::spawn_snapshot_recorder(app_state.clone());

    // --- Audio capture (dedicated OS thread; cpal Stream lives there) -------
    let audio_cfg = cfg.audio.clone();
    let audio_cmd_tx = cmd_tx.clone();
    std::thread::spawn(move || match audio::start(&audio_cfg, audio_cmd_tx) {
        Ok(_stream) => {
            info!("audio capture running");
            // Hold the stream alive for the lifetime of the process.
            std::thread::park();
        }
        Err(e) => error!(
            error = %format!("{e:#}"),
            "audio capture failed; server will continue (use the web app to drive the avatar manually)"
        ),
    });

    // --- HTTP / WS server --------------------------------------------------
    let app = net::build_router(app_state.clone(), &cfg.engine.asset_root);
    let listener = tokio::net::TcpListener::bind(&cfg.engine.bind)
        .await
        .with_context(|| format!("binding {}", cfg.engine.bind))?;
    let bound_addr = listener.local_addr()?;
    info!(
        bind = %bound_addr,
        panel = %format!("http://{bound_addr}/"),
        stage = %format!("http://{bound_addr}/stage.html"),
        "server listening; add the stage URL as an OBS Browser Source"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // --- Tear down ---------------------------------------------------------
    let _ = cmd_tx.send(state::StateCommand::Shutdown);
    let _ = state_handle.await;
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
