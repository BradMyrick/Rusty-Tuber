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
    AvatarSnapshot, ClientMessage, EnvelopeConfig, EyeState, MouthConfig,
    MouthState, ServerMessage,
};
use crate::state::StateCommand;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use rust_embed::RustEmbed;
use std::path::Path as FsPath;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock, Semaphore};
use tokio::task::JoinHandle;
use tower_http::set_header::SetResponseHeaderLayer;
use tracing::{debug, warn};

/// Maximum inbound WebSocket message size (256 KiB). The protocol messages are
/// a few dozen bytes, so this is generous while capping malicious/buggy clients.
const MAX_WS_MESSAGE_SIZE: usize = 256 * 1024;
/// Maximum inbound WebSocket frame size.
const MAX_WS_FRAME_SIZE: usize = 256 * 1024;
/// Concurrent WebSocket clients (panel, OBS source, phones, Stream Deck...).
const MAX_WS_CLIENTS: usize = 16;

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
    /// Latest mouth-level configuration (enablement + thresholds), mirrored
    /// from the state task for `GET /api/mouth-config` and the WS `Welcome`.
    pub mouth_config: Arc<RwLock<MouthConfig>>,
    /// Latest audio envelope (attack/release), mirrored for `GET /api/envelope`
    /// and `Welcome`.
    pub envelope: Arc<RwLock<EnvelopeConfig>>,
    /// Audio latency preset in use ("low" / "stable") — surfaced in `Welcome`.
    pub latency: String,
    /// Composited frame dimensions — surfaced in `Welcome` for info.
    pub frame_width: u32,
    pub frame_height: u32,
    /// Limits concurrent WebSocket clients to [`MAX_WS_CLIENTS`].
    pub ws_permits: Arc<Semaphore>,
}

impl AppState {
    /// Construct shared state with the default WebSocket concurrency cap.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        catalog: Arc<AssetCatalog>,
        default_emotion: String,
        cmd_tx: mpsc::UnboundedSender<StateCommand>,
        bcast_tx: broadcast::Sender<ServerMessage>,
        snapshot: Arc<RwLock<Option<AvatarSnapshot>>>,
        mouth_config: Arc<RwLock<MouthConfig>>,
        envelope: Arc<RwLock<EnvelopeConfig>>,
        latency: String,
        frame_width: u32,
        frame_height: u32,
    ) -> Self {
        Self {
            catalog,
            default_emotion,
            cmd_tx,
            bcast_tx,
            snapshot,
            mouth_config,
            envelope,
            latency,
            frame_width,
            frame_height,
            ws_permits: Arc::new(Semaphore::new(MAX_WS_CLIENTS)),
        }
    }
}

/// Build the full HTTP/WS router.
pub fn build_router(state: Arc<AppState>, asset_root: &FsPath) -> Router {
    // Frames are immutable for the lifetime of the process (assets load once at
    // startup), so let the browser reuse them without revalidating. This keeps
    // the first mouth/blink swap after OBS reconnects flicker-free.
    let frames = Router::new()
        .fallback_service(tower_http::services::ServeDir::new(asset_root))
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=3600"),
        ));

    Router::new()
        .route("/", get(index))
        .route("/:file", get(embedded_file))
        .route("/ws", get(ws_handler))
        .nest("/frames", frames)
        .nest("/api", api_router())
        .with_state(state)
        .layer(tower_http::trace::TraceLayer::new_for_http())
}

/// Spawn a task that mirrors the latest `StateUpdate` into the shared snapshot
/// so `GET /api/state` stays current, and `MouthConfigUpdate` into the shared
/// mouth config so `GET /api/mouth-config` and new WS clients stay current.
pub fn spawn_snapshot_recorder(state: Arc<AppState>) -> JoinHandle<()> {
    let mut rx = state.bcast_tx.subscribe();
    let snapshot = state.snapshot.clone();
    let mouth_config = state.mouth_config.clone();
    let envelope = state.envelope.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ServerMessage::StateUpdate {
                    emotion,
                    mouth,
                    eyes,
                    volume,
                    overridden,
                    mouth_overridden,
                    eyes_overridden,
                    eyes_frame,
                    mouth_frame,
                    default_emotion,
                }) => {
                    *snapshot.write().await = Some(AvatarSnapshot {
                        emotion,
                        mouth,
                        eyes,
                        volume,
                        overridden,
                        mouth_overridden,
                        eyes_overridden,
                        eyes_frame,
                        mouth_frame,
                        default_emotion,
                    });
                }
                Ok(ServerMessage::MouthConfigUpdate { config }) => {
                    *mouth_config.write().await = config;
                }
                Ok(ServerMessage::EnvelopeUpdate { config }) => {
                    *envelope.write().await = config;
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
        .route("/health", get(api_health))
        .route("/catalog", get(api_catalog))
        .route("/state", get(api_state))
        .route(
            "/mouth-config",
            get(api_get_mouth_config).post(api_set_mouth_config),
        )
        .route("/envelope", get(api_get_envelope).post(api_set_envelope))
        .route("/emotion/:name", post(api_trigger_emotion))
        .route("/clear", post(api_clear))
        .route("/default/:name", post(api_set_default))
        .route("/mouth/:mouth", post(api_set_mouth))
        .route("/mouth", post(api_clear_mouth))
        .route("/eyes/:state", post(api_set_eyes))
        .route("/eyes", post(api_clear_eyes))
}

async fn api_health() -> Response {
    Json(serde_json::json!({ "status": "ok" })).into_response()
}

/// JSON error envelope used by every failing REST handler so clients can parse
/// a consistent shape (`{"error": "..."}`) regardless of the status code.
fn api_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        Json(serde_json::json!({ "error": message.into() })),
    )
        .into_response()
}

/// Lower-case + validate an emotion name against the catalog, returning the
/// canonical key on success or an error message on failure.
fn resolve_emotion(
    catalog: &AssetCatalog,
    raw: &str,
) -> Result<String, String> {
    let key = raw.to_ascii_lowercase();
    if catalog.has_emotion(&key) {
        Ok(key)
    } else {
        Err(format!("unknown emotion: {raw}"))
    }
}

async fn api_catalog(State(st): State<Arc<AppState>>) -> Response {
    Json(st.catalog.catalog()).into_response()
}

async fn api_state(State(st): State<Arc<AppState>>) -> Response {
    let snap = st.snapshot.read().await.clone();
    match snap {
        Some(s) => Json(s).into_response(),
        None => {
            // No audio yet: synthesise a resting snapshot.
            let default = default_or_snapshot(&st).await;
            let emotion_opt = if default.is_empty() {
                None
            } else {
                Some(default.as_str())
            };
            let eyes_frame = st
                .catalog
                .eyes_frame(emotion_opt, EyeState::Open)
                .unwrap_or_default();
            let mouth_frame = st
                .catalog
                .mouth_frame(MouthState::Closed)
                .unwrap_or_default();
            Json(AvatarSnapshot {
                emotion: default,
                mouth: MouthState::Closed,
                eyes: EyeState::Open,
                volume: 0.0,
                overridden: false,
                mouth_overridden: false,
                eyes_overridden: false,
                eyes_frame,
                mouth_frame,
                default_emotion: st.default_emotion.clone(),
            })
            .into_response()
        }
    }
}

async fn api_get_mouth_config(State(st): State<Arc<AppState>>) -> Response {
    Json(st.mouth_config.read().await.clone()).into_response()
}

async fn api_set_mouth_config(
    State(st): State<Arc<AppState>>,
    Json(config): Json<MouthConfig>,
) -> Response {
    if let Err(msg) = config.validate() {
        return api_error(StatusCode::BAD_REQUEST, msg);
    }
    let _ = st.cmd_tx.send(StateCommand::SetMouthConfig(config));
    StatusCode::NO_CONTENT.into_response()
}

async fn api_get_envelope(State(st): State<Arc<AppState>>) -> Response {
    Json(st.envelope.read().await.clone()).into_response()
}

async fn api_set_envelope(
    State(st): State<Arc<AppState>>,
    Json(config): Json<EnvelopeConfig>,
) -> Response {
    if let Err(msg) = config.validate() {
        return api_error(StatusCode::BAD_REQUEST, msg);
    }
    let _ = st.cmd_tx.send(StateCommand::SetEnvelope(config));
    StatusCode::NO_CONTENT.into_response()
}

async fn api_trigger_emotion(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    match resolve_emotion(&st.catalog, &name) {
        Ok(key) => {
            let _ = st.cmd_tx.send(StateCommand::TriggerEmotion(key));
            StatusCode::NO_CONTENT.into_response()
        }
        Err(msg) => api_error(StatusCode::NOT_FOUND, msg),
    }
}

async fn api_clear(State(st): State<Arc<AppState>>) -> Response {
    let _ = st.cmd_tx.send(StateCommand::ClearOverride);
    StatusCode::NO_CONTENT.into_response()
}

async fn api_set_default(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    match resolve_emotion(&st.catalog, &name) {
        Ok(key) => {
            let _ = st.cmd_tx.send(StateCommand::SetDefault(key));
            StatusCode::NO_CONTENT.into_response()
        }
        Err(msg) => api_error(StatusCode::NOT_FOUND, msg),
    }
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
        None => api_error(
            StatusCode::BAD_REQUEST,
            format!(
                "invalid mouth: {mouth} (expected closed|partial|medium|open)"
            ),
        ),
    }
}

async fn api_clear_mouth(State(st): State<Arc<AppState>>) -> Response {
    let _ = st.cmd_tx.send(StateCommand::ClearMouthOverride);
    StatusCode::NO_CONTENT.into_response()
}

async fn api_set_eyes(
    State(st): State<Arc<AppState>>,
    Path(state): Path<String>,
) -> Response {
    match EyeState::from_str_ci(&state) {
        Some(e) => {
            let _ = st.cmd_tx.send(StateCommand::SetEyesOverride(e));
            StatusCode::NO_CONTENT.into_response()
        }
        None => api_error(
            StatusCode::BAD_REQUEST,
            format!("invalid eyes: {state} (expected open|closed)"),
        ),
    }
}

async fn api_clear_eyes(State(st): State<Arc<AppState>>) -> Response {
    let _ = st.cmd_tx.send(StateCommand::ClearEyesOverride);
    StatusCode::NO_CONTENT.into_response()
}

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    // Origin guard: non-browser clients (OBS, curl, Stream Deck) send no
    // `Origin` header and are allowed; browser origins are allowed only when
    // they resolve to a loopback or private-LAN host, so a random website can't
    // open the live control channel and drive the avatar.
    if let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
    {
        if !origin_local(&origin) {
            return api_error(
                StatusCode::FORBIDDEN,
                format!("origin not allowed: {origin}"),
            );
        }
    }

    // Bound concurrent clients so a runaway/abusive source can't exhaust FDs.
    let permit = match st.ws_permits.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "too many websocket clients",
            )
        }
    };

    ws.max_message_size(MAX_WS_MESSAGE_SIZE)
        .max_frame_size(MAX_WS_FRAME_SIZE)
        .on_upgrade(move |socket| async move {
            let _permit = permit; // held until the socket task ends
            handle_ws(socket, st).await;
        })
}

/// True if the host portion of an `Origin` URL is loopback or a private LAN
/// address (the panel/OBS/phone use cases). `Origin` is absent for non-browser
/// clients and is checked separately at the call site.
fn origin_local(origin: &str) -> bool {
    let after_scheme = origin.split("://").nth(1).unwrap_or(origin);
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    let host = authority
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(authority);
    is_local_host(host)
}

fn is_local_host(host: &str) -> bool {
    match host {
        "localhost" | "127.0.0.1" | "::1" | "[::1]" => true,
        _ => {
            let mut octets = host.split('.');
            match (octets.next(), octets.next(), octets.next(), octets.next()) {
                (Some("10"), _, _, _) => true,
                (Some("172"), Some(b), _, _) => {
                    b.parse::<u32>().is_ok_and(|n| (16..=31).contains(&n))
                }
                (Some("192"), Some("168"), _, _) => true,
                _ => false,
            }
        }
    }
}

async fn handle_ws(socket: WebSocket, st: Arc<AppState>) {
    let (mut sink, mut stream) = socket.split();
    let mut rx = st.bcast_tx.subscribe();

    // The video is NOT on this socket — the browser reads the virtual webcam
    // directly (getUserMedia). This WS carries only control + meter + config.
    let welcome = ServerMessage::Welcome {
        catalog: st.catalog.catalog().clone(),
        default_emotion: default_or_snapshot(&st).await,
        mouth_config: st.mouth_config.read().await.clone(),
        envelope: st.envelope.read().await.clone(),
        latency: st.latency.clone(),
        frame_width: st.frame_width,
        frame_height: st.frame_height,
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
                        warn!(lag = n, "ws client lagged; resyncing from snapshot");
                        if resync_from_snapshot(&st, &mut sink)
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    debug!("websocket client disconnected");
}

/// Push the latest snapshot as a `StateUpdate` so a lagging client re-syncs
/// immediately instead of freezing on a stale frame.
async fn resync_from_snapshot(
    st: &AppState,
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
) -> Result<(), axum::Error> {
    if let Some(s) = st.snapshot.read().await.clone() {
        let msg = ServerMessage::StateUpdate {
            emotion: s.emotion,
            mouth: s.mouth,
            eyes: s.eyes,
            volume: s.volume,
            overridden: s.overridden,
            mouth_overridden: s.mouth_overridden,
            eyes_overridden: s.eyes_overridden,
            eyes_frame: s.eyes_frame,
            mouth_frame: s.mouth_frame,
            default_emotion: s.default_emotion,
        };
        send_server_message(sink, &msg).await
    } else {
        Ok(())
    }
}

async fn handle_client_text(
    st: &AppState,
    text: &str,
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
) {
    let parsed: Result<ClientMessage, _> = serde_json::from_str(text);
    match parsed {
        Ok(ClientMessage::TriggerEmotion { emotion }) => {
            match resolve_emotion(&st.catalog, &emotion) {
                Ok(key) => {
                    let _ = st.cmd_tx.send(StateCommand::TriggerEmotion(key));
                }
                Err(msg) => reply_error(sink, &msg).await,
            }
        }
        Ok(ClientMessage::SetDefault { emotion }) => {
            match resolve_emotion(&st.catalog, &emotion) {
                Ok(key) => {
                    let _ = st.cmd_tx.send(StateCommand::SetDefault(key));
                }
                Err(msg) => reply_error(sink, &msg).await,
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
        Ok(ClientMessage::SetMouthConfig { config }) => {
            if let Err(msg) = config.validate() {
                reply_error(sink, &msg).await;
            } else {
                let _ = st.cmd_tx.send(StateCommand::SetMouthConfig(config));
            }
        }
        Ok(ClientMessage::SetEnvelope { config }) => {
            if let Err(msg) = config.validate() {
                reply_error(sink, &msg).await;
            } else {
                let _ = st.cmd_tx.send(StateCommand::SetEnvelope(config));
            }
        }
        Ok(ClientMessage::SetEyesOverride { eyes }) => {
            let _ = st.cmd_tx.send(StateCommand::SetEyesOverride(eyes));
        }
        Ok(ClientMessage::ClearEyesOverride) => {
            let _ = st.cmd_tx.send(StateCommand::ClearEyesOverride);
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
