mod config;
mod crossover;
mod dsp;
mod metadata;
mod pipe;
mod server;
mod tui;

use std::sync::{Arc, Mutex};
use std::thread;
use clap::{Parser, Subcommand};
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;

use config::{AppState, AudioRuntimeConfig};

#[derive(Parser)]
#[command(name = "digital-crossover-dsp")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Serve,
    Tui {
        #[arg(short, long, default_value = "http://127.0.0.1:3000")]
        url: String,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Serve => {
            let token = CancellationToken::new();
            let state = Arc::new(Mutex::new(AppState::default()));

            let initial_config = AudioRuntimeConfig::default();
            let (config_tx, config_rx) = watch::channel(initial_config);

            let (fft_tx, _fft_rx) = broadcast::channel::<String>(16);

            metadata::spawn_thread(token.clone(), state.clone());

            let dsp_token = token.clone();
            let dsp_state = state.clone();
            let dsp_fft_tx = fft_tx.clone();
            let dsp_handle = thread::spawn(move || {
                dsp::run(dsp_token, config_rx, dsp_state, dsp_fft_tx);
            });

            server::spawn(token.clone(), config_tx, state.clone(), fft_tx).await;

            tokio::signal::ctrl_c().await?;
            println!("\nShutdown signal received...");

            token.cancel();

            let _ = dsp_handle.join();

            println!("Application gracefully shut down.");
        }
        Commands::Tui { url } => {
            tui::run(&url).await?;
        }
    }

    Ok(())
}