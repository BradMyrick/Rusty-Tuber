//! HTTP + WebSocket server (axum).
//!
//! Routes:
//! - `GET /` and `GET /:file`  -> embedded web UI (HTML/JS from `web/`).
//! - `GET /frames/...`         -> character PNGs served from the asset root.
//! - `GET /ws`                 -> real-time control channel (JSON protocol).
//! - `GET/POST /api/...`       -> REST mirror of the common commands.
//!
//! State updates produced by the [`crate::state`] task fan out to every
//! connected WebSocket client via a `broadcast` channel. The latest snapshot is
//! also recorded so `GET /api/state` can answer without IPC with the state task.

use crate::assets::AssetCatalog;
use crate::protocol::{
    AvatarSnapshot, ClientMessage, MouthState, ServerMessage,
};
use crate::state::StateCommand;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use rust_embed::RustEmbed;
use std::path::Path as FsPath;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

#[derive(RustEmbed)]
#[folder = "web/"]
struct WebAsset;

/// Shared application state handed to every handler.
pub struct AppState {
    pub catalog: Arc<AssetCatalog>,
    pub default_emotion: String,
    pub cmd_tx: mpsc::UnboundedSender<StateCommand>,
    pub bcast_tx: broadcast::Sender<ServerMessage>,
    pub snapshot: Arc<RwLock<Option<AvatarSnapshot>>>,
}

/// Build the full HTTP/WS router.
pub fn build_router(state: Arc<AppState>, asset_root: &FsPath) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/:file", get(embedded_file))
        .route("/ws", get(ws_handler))
        .nest_service(
            "/frames",
            tower_http::services::ServeDir::new(asset_root),
        )
        .nest("/api", api_router())
        .with_state(state)
        .layer(tower_http::cors::CorsLayer::very_permissive())
        .layer(tower_http::trace::TraceLayer::new_for_http())
}

/// Spawn a task that mirrors the latest `StateUpdate` into the shared snapshot
/// so `GET /api/state` stays current.
pub fn spawn_snapshot_recorder(state: Arc<AppState>) -> JoinHandle<()> {
    let mut rx = state.bcast_tx.subscribe();
    let snapshot = state.snapshot.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ServerMessage::StateUpdate {
                    emotion,
                    mouth,
                    volume,
                    overridden,
                    frame,
                    default_emotion,
                }) => {
                    *snapshot.write().await = Some(AvatarSnapshot {
                        emotion,
                        mouth,
                        volume,
                        overridden,
                        frame,
                        default_emotion,
                    });
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lag = n, "snapshot recorder lagged")
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Embedded web UI
// ---------------------------------------------------------------------------

async fn index() -> Response {
    serve_embedded("index.html")
}

async fn embedded_file(Path(file): Path<String>) -> Response {
    // Reject path traversal; `rust-embed` keys are flat filenames anyway.
    if file.contains("..") || file.contains('/') {
        return StatusCode::NOT_FOUND.into_response();
    }
    serve_embedded(&file)
}

fn serve_embedded(path: &str) -> Response {
    match WebAsset::get(path) {
        Some(asset) => {
            let mime = mime_guess::from_path(path)
                .first_or_octet_stream()
                .essence_str()
                .to_owned();
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, mime)],
                asset.data.into_owned(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------------------
// REST API
// ---------------------------------------------------------------------------

fn api_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/catalog", get(api_catalog))
        .route("/state", get(api_state))
        .route("/emotion/:name", post(api_trigger_emotion))
        .route("/clear", post(api_clear))
        .route("/default/:name", post(api_set_default))
        .route("/mouth/:mouth", post(api_set_mouth))
        .route("/mouth", post(api_clear_mouth))
}

async fn api_catalog(State(st): State<Arc<AppState>>) -> Response {
    Json(serde_json::json!({
        "default_emotion": default_or_snapshot(&st).await,
        "emotions": st.catalog.emotions().collect::<Vec<_>>(),
        "frames": st.catalog.catalog(),
    }))
    .into_response()
}

async fn api_state(State(st): State<Arc<AppState>>) -> Response {
    let snap = st.snapshot.read().await.clone();
    match snap {
        Some(s) => Json(s).into_response(),
        None => {
            // No audio yet: synthesise a resting snapshot.
            let default = default_or_snapshot(&st).await;
            let frame = st
                .catalog
                .frame_url(&default, MouthState::Closed)
                .unwrap_or_default();
            Json(AvatarSnapshot {
                emotion: default,
                mouth: MouthState::Closed,
                volume: 0.0,
                overridden: false,
                frame,
                default_emotion: st.default_emotion.clone(),
            })
            .into_response()
        }
    }
}

async fn api_trigger_emotion(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let key = name.to_ascii_lowercase();
    if st.catalog.get(&key).is_none() {
        return (StatusCode::NOT_FOUND, format!("unknown emotion: {name}"))
            .into_response();
    }
    let _ = st.cmd_tx.send(StateCommand::TriggerEmotion(key));
    StatusCode::NO_CONTENT.into_response()
}

async fn api_clear(State(st): State<Arc<AppState>>) -> Response {
    let _ = st.cmd_tx.send(StateCommand::ClearOverride);
    StatusCode::NO_CONTENT.into_response()
}

async fn api_set_default(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let key = name.to_ascii_lowercase();
    if st.catalog.get(&key).is_none() {
        return (StatusCode::NOT_FOUND, format!("unknown emotion: {name}"))
            .into_response();
    }
    let _ = st.cmd_tx.send(StateCommand::SetDefault(key));
    StatusCode::NO_CONTENT.into_response()
}

async fn api_set_mouth(
    State(st): State<Arc<AppState>>,
    Path(mouth): Path<String>,
) -> Response {
    match MouthState::from_str_ci(&mouth) {
        Some(m) => {
            let _ = st.cmd_tx.send(StateCommand::SetMouthOverride(m));
            StatusCode::NO_CONTENT.into_response()
        }
        None => (
            StatusCode::BAD_REQUEST,
            format!(
                "invalid mouth: {mouth} (expected closed|slight|medium|open)"
            ),
        )
            .into_response(),
    }
}

async fn api_clear_mouth(State(st): State<Arc<AppState>>) -> Response {
    let _ = st.cmd_tx.send(StateCommand::ClearMouthOverride);
    StatusCode::NO_CONTENT.into_response()
}

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(st): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_ws(socket, st))
}

async fn handle_ws(socket: WebSocket, st: Arc<AppState>) {
    let (mut sink, mut stream) = socket.split();
    let mut rx = st.bcast_tx.subscribe();

    let welcome = ServerMessage::Welcome {
        catalog: st.catalog.catalog().clone(),
        default_emotion: default_or_snapshot(&st).await,
    };
    if send_server_message(&mut sink, &welcome).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => handle_client_text(&st, &text, &mut sink).await,
                    Some(Ok(Message::Binary(b))) => {
                        // Tolerate clients that send binary-encoded JSON.
                        if let Ok(text) = std::str::from_utf8(&b) {
                            handle_client_text(&st, text, &mut sink).await;
                        }
                    }
                    Some(Ok(Message::Ping(p))) => { let _ = sink.send(Message::Pong(p)).await; }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
            outgoing = rx.recv() => {
                match outgoing {
                    Ok(msg) => {
                        if send_server_message(&mut sink, &msg).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(lag = n, "ws client lagged; will resync on next update");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    debug!("websocket client disconnected");
}

async fn handle_client_text(
    st: &AppState,
    text: &str,
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
) {
    let parsed: Result<ClientMessage, _> = serde_json::from_str(text);
    match parsed {
        Ok(ClientMessage::TriggerEmotion { emotion }) => {
            let key = emotion.to_ascii_lowercase();
            if st.catalog.get(&key).is_none() {
                reply_error(sink, &format!("unknown emotion: {emotion}")).await;
            } else {
                let _ = st.cmd_tx.send(StateCommand::TriggerEmotion(key));
            }
        }
        Ok(ClientMessage::SetDefault { emotion }) => {
            let key = emotion.to_ascii_lowercase();
            if st.catalog.get(&key).is_none() {
                reply_error(sink, &format!("unknown emotion: {emotion}")).await;
            } else {
                let _ = st.cmd_tx.send(StateCommand::SetDefault(key));
            }
        }
        Ok(ClientMessage::ClearOverride) => {
            let _ = st.cmd_tx.send(StateCommand::ClearOverride);
        }
        Ok(ClientMessage::SetMouthOverride { mouth }) => {
            let _ = st.cmd_tx.send(StateCommand::SetMouthOverride(mouth));
        }
        Ok(ClientMessage::ClearMouthOverride) => {
            let _ = st.cmd_tx.send(StateCommand::ClearMouthOverride);
        }
        Ok(ClientMessage::Hello) => {}
        Err(e) => {
            reply_error(sink, &format!("invalid message: {e}")).await;
        }
    }
}

async fn send_server_message(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    msg: &ServerMessage,
) -> Result<(), axum::Error> {
    match serde_json::to_string(msg) {
        Ok(json) => sink.send(Message::Text(json)).await,
        Err(e) => {
            warn!(error = %e, "failed to serialize server message");
            Ok(())
        }
    }
}

async fn reply_error(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: &str,
) {
    let msg = ServerMessage::Error {
        message: message.to_string(),
    };
    let _ = send_server_message(sink, &msg).await;
}

/// Prefer the snapshot's `default_emotion` (which reflects runtime
/// `SetDefault` calls); fall back to the configured default.
async fn default_or_snapshot(st: &AppState) -> String {
    st.snapshot
        .read()
        .await
        .as_ref()
        .map(|s| s.default_emotion.clone())
        .unwrap_or_else(|| st.default_emotion.clone())
}
