use axum::{Json, Router, routing::post};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::config::AudioRuntimeConfig;

pub async fn spawn(token: CancellationToken, config_tx: watch::Sender<AudioRuntimeConfig>) {
    let app = Router::new().route(
        "/update_config",
        post({
            let tx = config_tx.clone();
            move |Json(payload): Json<AudioRuntimeConfig>| async move {
                let _ = tx.send(payload);
                "Config updated successfully!"
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