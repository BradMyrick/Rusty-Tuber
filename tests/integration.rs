//! End-to-end tests for the HTTP + WebSocket surface (`net.rs`) wired to the
//! real state task and the project's own asset catalog.

use futures_util::{SinkExt, StreamExt};
use rusty_tuber::{assets, config, net, state};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio_tungstenite::connect_async;

/// Spawn the router on an ephemeral port (no audio) against the real catalog.
async fn spawn_server() -> String {
    let cfg = config::AppConfig::from_path(std::path::Path::new("config.toml"))
        .unwrap();
    let catalog =
        Arc::new(assets::AssetCatalog::load(&cfg.engine.asset_root).unwrap());
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (bcast_tx, _) = broadcast::channel(256);
    let _state = state::spawn(
        catalog.clone(),
        cfg.thresholds,
        cfg.timers.clone(),
        cfg.engine.default_emotion.clone(),
        cmd_tx.clone(),
        cmd_rx,
        bcast_tx.clone(),
    );
    let app_state = Arc::new(net::AppState {
        catalog: catalog.clone(),
        default_emotion: cfg.engine.default_emotion.clone(),
        cmd_tx: cmd_tx.clone(),
        bcast_tx: bcast_tx.clone(),
        snapshot: Arc::new(RwLock::new(None)),
    });
    let _rec = net::spawn_snapshot_recorder(app_state.clone());
    let app = net::build_router(app_state, &cfg.engine.asset_root);
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
async fn rest_catalog_lists_all_emotions() {
    let base = spawn_server().await;
    let body: serde_json::Value = http()
        .get(format!("{base}/api/catalog"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let emotions = body["emotions"].as_array().unwrap();
    let names: Vec<&str> =
        emotions.iter().map(|v| v.as_str().unwrap()).collect();
    assert!(names.contains(&"calm"));
    assert!(names.contains(&"surprised"));
    // full frame set for calm
    assert_eq!(
        body["frames"]["calm"]["slight"].as_str(),
        Some("calm/slight.png")
    );
    // partial set for surprised (no slight/medium)
    assert!(body["frames"]["surprised"]["slight"].is_null());
}

#[tokio::test]
async fn rest_trigger_then_revert_via_timer() {
    let base = spawn_server().await;
    let client = http();

    let status = client
        .post(format!("{base}/api/emotion/surprised"))
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
    assert_eq!(st["emotion"].as_str(), Some("surprised"));
    assert_eq!(st["overridden"].as_bool(), Some(true));

    // Timer is 2.5s; wait for the auto-revert.
    tokio::time::sleep(std::time::Duration::from_millis(2800)).await;
    let st: serde_json::Value = client
        .get(format!("{base}/api/state"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        st["emotion"].as_str(),
        Some("calm"),
        "should have reverted to the default"
    );
}

#[tokio::test]
async fn rest_unknown_emotion_is_404() {
    let base = spawn_server().await;
    let status = client_post_404(&base, "does-not-exist").await;
    assert_eq!(status.as_u16(), 404);
}

async fn client_post_404(base: &str, emotion: &str) -> reqwest::StatusCode {
    http()
        .post(format!("{base}/api/emotion/{emotion}"))
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn ws_welcome_and_state_update() {
    let base = spawn_server().await;
    let ws_url = base.replacen("http://", "ws://", 1) + "/ws";
    let (mut socket, _) = connect_async(&ws_url).await.unwrap();

    // First message must be Welcome with the catalog.
    let welcome = serde_json::from_str::<serde_json::Value>(
        &socket.next().await.unwrap().unwrap().into_text().unwrap(),
    )
    .unwrap();
    assert_eq!(welcome["type"].as_str(), Some("Welcome"));
    assert!(welcome["payload"]["catalog"]["calm"].is_object());

    // Trigger an emotion and wait for a matching StateUpdate.
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"TriggerEmotion","payload":{"emotion":"laughing"}}"#
                .into(),
        ))
        .await
        .unwrap();

    let mut saw_laughing = false;
    for _ in 0..20 {
        let msg = serde_json::from_str::<serde_json::Value>(
            &socket.next().await.unwrap().unwrap().into_text().unwrap(),
        )
        .unwrap();
        if msg["type"].as_str() == Some("StateUpdate")
            && msg["payload"]["emotion"].as_str() == Some("laughing")
        {
            assert_eq!(
                msg["payload"]["frame"].as_str(),
                Some("/frames/laughing/closed.png")
            );
            saw_laughing = true;
            break;
        }
    }
    assert!(saw_laughing, "should have received a laughing StateUpdate");
}

#[tokio::test]
async fn ws_unknown_emotion_replies_error() {
    let base = spawn_server().await;
    let ws_url = base.replacen("http://", "ws://", 1) + "/ws";
    let (mut socket, _) = connect_async(&ws_url).await.unwrap();
    // consume Welcome
    let _ = socket.next().await.unwrap().unwrap();

    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"TriggerEmotion","payload":{"emotion":"nope"}}"#.into(),
        ))
        .await
        .unwrap();

    let mut saw_error = false;
    for _ in 0..20 {
        let msg = serde_json::from_str::<serde_json::Value>(
            &socket.next().await.unwrap().unwrap().into_text().unwrap(),
        )
        .unwrap();
        if msg["type"].as_str() == Some("Error") {
            assert!(msg["payload"]["message"]
                .as_str()
                .unwrap()
                .contains("unknown emotion"));
            saw_error = true;
            break;
        }
    }
    assert!(
        saw_error,
        "server should reject unknown emotions with an Error"
    );
}
