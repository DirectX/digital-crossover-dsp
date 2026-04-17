mod config;
mod dsp;
mod metadata;
mod pipe;
mod server;

use std::thread;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use config::AudioRuntimeConfig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = CancellationToken::new();

    let initial_config = AudioRuntimeConfig {
        filter_cutoff: 0.95,
        volume: 1.0,
    };
    let (config_tx, config_rx) = watch::channel(initial_config);

    metadata::spawn_thread(token.clone());

    let dsp_token = token.clone();
    let dsp_handle = thread::spawn(move || {
        dsp::run(dsp_token, config_rx);
    });

    server::spawn(token.clone(), config_tx).await;

    tokio::signal::ctrl_c().await?;
    println!("\nShutdown signal received...");

    token.cancel();

    let _ = dsp_handle.join();

    println!("Application gracefully shut down.");
    Ok(())
}