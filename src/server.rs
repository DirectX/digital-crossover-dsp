use axum::{
    Json, Router,
    routing::{get, post},
};
#[cfg(feature = "fft")]
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::config::{AudioRuntimeConfig, SharedState};
#[cfg(feature = "fft")]
use crate::config::FftBroadcast;

pub async fn spawn(
    token: CancellationToken,
    config_tx: watch::Sender<AudioRuntimeConfig>,
    state: SharedState,
    #[cfg(feature = "fft")] fft_tx: FftBroadcast,
) {
    let app = Router::new()
        .route(
            "/update_config",
            post({
                let tx = config_tx.clone();
                move |Json(payload): Json<AudioRuntimeConfig>| async move {
                    let _ = tx.send(payload);
                    "Config updated successfully!"
                }
            }),
        )
        .route(
            "/config",
            get({
                let tx = config_tx.clone();
                move || async move {
                    let cfg = tx.borrow().clone();
                    Json(cfg)
                }
            }),
        )
        .route(
            "/status",
            get({
                let state = state.clone();
                move || async move {
                    let s = state.lock().unwrap().clone();
                    Json(s)
                }
            }),
        );

    #[cfg(feature = "fft")]
    let app = app.route(
        "/ws/fft",
        get({
            let fft_tx = fft_tx.clone();
            move |ws: WebSocketUpgrade| {
                let rx = fft_tx.subscribe();
                async move { ws.on_upgrade(move |socket| handle_fft_ws(socket, rx)) }
            }
        }),
    );

    let listener = TcpListener::bind("0.0.0.0:3000")
        .await
        .expect("Failed to bind port 3000");
    println!("Web API listening on port 3000");

    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(token.cancelled_owned())
            .await
            .ok();
    });
}

#[cfg(feature = "fft")]
async fn handle_fft_ws(mut socket: WebSocket, mut rx: tokio::sync::broadcast::Receiver<String>) {
    loop {
        match rx.recv().await {
            Ok(json) => {
                if socket.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        }
    }
}
