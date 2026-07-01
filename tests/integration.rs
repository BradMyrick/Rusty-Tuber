//! End-to-end tests for the HTTP + WebSocket surface (`net.rs`) wired to the
//! real state task and the project's own layered asset catalog.

use futures_util::{SinkExt, StreamExt};
use rusty_tuber::{assets, config, net, state};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio_tungstenite::connect_async;

/// Read the next TEXT JSON message from the socket, skipping the binary RGBA
/// avatar frames the server interleaves with its control messages.
async fn next_json(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> serde_json::Value {
    loop {
        match socket.next().await {
            Some(Ok(m)) => {
                if let Ok(t) = m.into_text() {
                    if let Ok(v) = serde_json::from_str(&t) {
                        return v;
                    }
                }
                // binary frame or non-JSON text — skip
            }
            other => panic!("stream ended unexpectedly: {other:?}"),
        }
    }
}

/// Spawn the router on an ephemeral port (no audio) against the real catalog.
async fn spawn_server() -> String {
    let cfg = config::AppConfig::from_path(std::path::Path::new("config.toml"))
        .unwrap();
    let catalog = Arc::new(
        assets::AssetCatalog::load(std::path::Path::new(
            "./assets/characters/default_macaw",
        ))
        .unwrap(),
    );
    let compositor = Arc::new(
        rusty_tuber::compositor::Compositor::new(
            catalog.clone(),
            std::path::Path::new("./assets/characters/default_macaw"),
        )
        .unwrap(),
    );
    let mouth_config = cfg.thresholds.to_mouth_config().unwrap();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (bcast_tx, _) = broadcast::channel(256);
    let default_emotion = if catalog.has_emotion(&cfg.engine.default_emotion) {
        cfg.engine.default_emotion.clone()
    } else {
        String::new()
    };
    let init = compositor.render(
        if default_emotion.is_empty() {
            None
        } else {
            Some(&default_emotion)
        },
        rusty_tuber::protocol::MouthState::Closed,
        rusty_tuber::protocol::EyeState::Open,
        &[],
    );
    let (frame_tx, _) = tokio::sync::watch::channel(std::sync::Arc::new(init));
    let envelope = rusty_tuber::audio::EnvelopeControl::new(6.0, 110.0);
    let _state = state::spawn(
        catalog.clone(),
        compositor,
        mouth_config.clone(),
        envelope,
        cfg.timers.clone(),
        default_emotion.clone(),
        cmd_tx.clone(),
        cmd_rx,
        bcast_tx.clone(),
        frame_tx.clone(),
    );
    let app_state = Arc::new(net::AppState::new(
        catalog.clone(),
        default_emotion,
        cmd_tx.clone(),
        bcast_tx.clone(),
        Arc::new(RwLock::new(None)),
        Arc::new(RwLock::new(mouth_config)),
        Arc::new(RwLock::new(rusty_tuber::protocol::EnvelopeConfig {
            attack_ms: 6.0,
            release_ms: 110.0,
        })),
        "low".into(),
        frame_tx,
        [0, 255, 0],
    ));
    let _rec = net::spawn_snapshot_recorder(app_state.clone());
    let app = net::build_router(
        app_state,
        std::path::Path::new("./assets/characters/default_macaw"),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().build().expect("reqwest client")
}

#[tokio::test]
async fn rest_catalog_lists_layers() {
    let base = spawn_server().await;
    let body: serde_json::Value = http()
        .get(format!("{base}/api/catalog"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Layered catalog: base body + mouths + default eyes.
    assert!(!body["base"].as_array().unwrap().is_empty());
    assert_eq!(body["mouths"]["closed"].as_str(), Some("mouths/closed.png"));
    assert_eq!(body["mouths"]["open"].as_str(), Some("mouths/open.png"));
    assert_eq!(body["default_eyes"]["open"].as_str(), Some("eyes/open.png"));
    assert_eq!(
        body["default_eyes"]["closed"].as_str(),
        Some("eyes/closed.png")
    );
    // No emotion eye-sets shipped with the placeholder art.
    assert!(body["emotions"].as_object().unwrap().is_empty());
}

#[tokio::test]
async fn rest_health_returns_ok() {
    let base = spawn_server().await;
    let body: serde_json::Value = http()
        .get(format!("{base}/api/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["status"].as_str(), Some("ok"));
}

#[tokio::test]
async fn rest_unknown_emotion_is_404_json() {
    let base = spawn_server().await;
    let resp = http()
        .post(format!("{base}/api/emotion/does-not-exist"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/json"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("unknown emotion"));
}

#[tokio::test]
async fn rest_state_synthesises_resting_layers() {
    let base = spawn_server().await;
    let st: serde_json::Value = http()
        .get(format!("{base}/api/state"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(st["mouth"].as_str(), Some("closed"));
    assert_eq!(st["eyes"].as_str(), Some("open"));
    assert_eq!(st["eyes_frame"].as_str(), Some("/frames/eyes/open.png"));
    assert_eq!(
        st["mouth_frame"].as_str(),
        Some("/frames/mouths/closed.png")
    );
}

#[tokio::test]
async fn rest_mouth_override_swaps_mouth_layer() {
    let base = spawn_server().await;
    let client = http();
    let status = client
        .post(format!("{base}/api/mouth/open"))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status.as_u16(), 204);

    let st: serde_json::Value = client
        .get(format!("{base}/api/state"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(st["mouth"].as_str(), Some("open"));
    assert_eq!(st["mouth_frame"].as_str(), Some("/frames/mouths/open.png"));
    assert_eq!(st["mouth_overridden"].as_bool(), Some(true));

    // Invalid level is rejected.
    let status = client
        .post(format!("{base}/api/mouth/grin"))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status.as_u16(), 400);
}

#[tokio::test]
async fn rest_eyes_override_swaps_eye_layer() {
    let base = spawn_server().await;
    let client = http();

    let status = client
        .post(format!("{base}/api/eyes/closed"))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status.as_u16(), 204);

    let st: serde_json::Value = client
        .get(format!("{base}/api/state"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(st["eyes"].as_str(), Some("closed"));
    assert_eq!(st["eyes_frame"].as_str(), Some("/frames/eyes/closed.png"));

    // Clearing returns to open eyes.
    client
        .post(format!("{base}/api/eyes"))
        .send()
        .await
        .unwrap();
    let st: serde_json::Value = client
        .get(format!("{base}/api/state"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(st["eyes"].as_str(), Some("open"));
    assert_eq!(st["eyes_frame"].as_str(), Some("/frames/eyes/open.png"));
}

#[tokio::test]
async fn ws_welcome_carries_layered_catalog() {
    let base = spawn_server().await;
    let ws_url = base.replacen("http://", "ws://", 1) + "/ws";
    let (mut socket, _) = connect_async(&ws_url).await.unwrap();

    let welcome = next_json(&mut socket).await;
    assert_eq!(welcome["type"].as_str(), Some("Welcome"));
    assert!(
        welcome["payload"]["catalog"]["base"].is_array(),
        "catalog carries a base layer array"
    );
    assert!(welcome["payload"]["catalog"]["mouths"]["closed"].is_string());
}

#[tokio::test]
async fn ws_mouth_override_emits_layered_state_update() {
    let base = spawn_server().await;
    let ws_url = base.replacen("http://", "ws://", 1) + "/ws";
    let (mut socket, _) = connect_async(&ws_url).await.unwrap();
    let _ = socket.next().await.unwrap(); // Welcome

    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"SetMouthOverride","payload":{"mouth":"open"}}"#.into(),
        ))
        .await
        .unwrap();

    let mut saw_open = false;
    for _ in 0..20 {
        let msg = next_json(&mut socket).await;
        if msg["type"].as_str() == Some("StateUpdate")
            && msg["payload"]["mouth"].as_str() == Some("open")
        {
            assert_eq!(
                msg["payload"]["mouth_frame"].as_str(),
                Some("/frames/mouths/open.png")
            );
            saw_open = true;
            break;
        }
    }
    assert!(saw_open, "should have received an open-mouth StateUpdate");
}

#[tokio::test]
async fn ws_eyes_override_emits_layered_state_update() {
    let base = spawn_server().await;
    let ws_url = base.replacen("http://", "ws://", 1) + "/ws";
    let (mut socket, _) = connect_async(&ws_url).await.unwrap();
    let _ = socket.next().await.unwrap(); // Welcome

    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"SetEyesOverride","payload":{"eyes":"closed"}}"#.into(),
        ))
        .await
        .unwrap();

    let mut saw_closed = false;
    for _ in 0..20 {
        let msg = next_json(&mut socket).await;
        if msg["type"].as_str() == Some("StateUpdate")
            && msg["payload"]["eyes"].as_str() == Some("closed")
        {
            assert_eq!(
                msg["payload"]["eyes_frame"].as_str(),
                Some("/frames/eyes/closed.png")
            );
            saw_closed = true;
            break;
        }
    }
    assert!(saw_closed, "should have received a closed-eyes StateUpdate");
}
