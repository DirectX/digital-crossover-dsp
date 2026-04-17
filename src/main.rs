use axum::{Json, Router, routing::post};
use base64::prelude::*;
use cpal::StreamConfig;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rtrb::RingBuffer;
use serde::Deserialize;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::{BufRead, BufReader};
use std::thread;
use std::time::Instant;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

// Fix: Shairport-sync is providing a 48kHz pipe stream in your system!
const INPUT_RATE: u32 = 48000;
const OUTPUT_RATE: u32 = 96000;
const CHANNELS: usize = 2;
const PIPE_PATH: &str = "/tmp/shairport-sync-audio";
const METADATA_PATH: &str = "/tmp/shairport-sync-metadata";
const RESAMPLE_CHUNK: usize = 1024;
const BUFFER_CAPACITY: usize = OUTPUT_RATE as usize * CHANNELS * 2;
const BASE_RATIO: f64 = OUTPUT_RATE as f64 / INPUT_RATE as f64;
const FILL_TARGET: f64 = 0.5;

// PI terms
const P_GAIN: f64 = 0.002;
const I_GAIN: f64 = 0.00005;

// --- Your Audio Config Data ---
#[derive(Clone, Debug, Deserialize)]
struct AudioRuntimeConfig {
    pub filter_cutoff: f32, // Example coefficient
    pub volume: f32,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Setup graceful shutdown token
    let token = CancellationToken::new();
    let dsp_token = token.clone();

    // 2. Setup the "Bridge" between Axum (Async) and DSP (Sync)
    // watch::channel holds the LATEST value. Axum updates it, DSP reads it lock-free.
    let initial_config = AudioRuntimeConfig {
        filter_cutoff: 0.95,
        volume: 1.0,
    };
    let (config_tx, mut config_rx) = watch::channel(initial_config);

    // Start scanning and printing metadata in the background independently
    spawn_metadata_thread(token.clone());

    let host = cpal::default_host();
    let device = host.default_output_device().expect("No output device");
    println!("Output device: {}", device.name()?);

    let config = StreamConfig {
        channels: CHANNELS as u16,
        sample_rate: cpal::SampleRate(OUTPUT_RATE),
        buffer_size: cpal::BufferSize::Default,
    };

    // 3. Spawn the heavy DSP loop in a DEDICATED OS thread (Not Tokio)
    let dsp_handle = thread::spawn(move || {
        // [ Setup CPAL and lock-free rtrb RingBuffer here ]

        // Inside your file-reading/resampling loop:
        while !dsp_token.is_cancelled() {
            // Non-blocking check for updated configuration from Axum!
            if config_rx.has_changed().unwrap_or(false) {
                let latest_config = config_rx.borrow_and_update().clone();
                println!("DSP applying new config: {:?}", latest_config);
                // Apply latest_config.filter_cutoff to your DSP chain here
            }

            // Create RingBuffer before starting playback
            let (mut producer, mut consumer) = RingBuffer::<i32>::new(BUFFER_CAPACITY);

            // Build and start the output stream before pipe-reading loop
            let stream = device.build_output_stream(
                &config,
                move |output: &mut [i32], _| {
                    for sample in output.iter_mut() {
                        *sample = consumer.pop().unwrap_or(0);
                    }
                },
                |err| eprintln!("CPAL error: {err}"),
                None,
            ).expect("Failed to build output stream");

            stream.play().expect("Failed to start playback");
            println!("Playback started at {OUTPUT_RATE} Hz. Ctrl+C to exit.");

            // [ Read chunk from pipe... ]
            // [ Run rubato resampler... ]
            // [ Push to RingBuffer... ]

            loop {
                if dsp_token.is_cancelled() {
                    return;
                }
                if std::path::Path::new(PIPE_PATH).exists() {
                    break;
                }
                println!("Waiting for pipe {PIPE_PATH}...");
                thread::sleep(std::time::Duration::from_secs(1));
            }

            let mut file = OpenOptions::new()
                .read(true)
                .open(PIPE_PATH)
                .expect("Failed to open pipe");

            println!("Pipe opened, reading audio...");

            let sinc_params = SincInterpolationParameters {
                sinc_len: 256,
                f_cutoff: 0.95,
                interpolation: SincInterpolationType::Linear,
                oversampling_factor: 256,
                window: WindowFunction::BlackmanHarris2,
            };

            let mut resampler =
                SincFixedIn::<f64>::new(BASE_RATIO, 1.05, sinc_params, RESAMPLE_CHUNK, CHANNELS)
                    .expect("Failed to create resampler");

            let mut input_buf = vec![vec![0.0f64; RESAMPLE_CHUNK]; CHANNELS];
            let mut raw_buf = [0u8; RESAMPLE_CHUNK * CHANNELS * 4];
            let frame_bytes = CHANNELS * 4;
            let chunk_bytes = RESAMPLE_CHUNK * frame_bytes;
            let mut leftover = 0usize;

            let mut last_status = Instant::now();
            let mut chunks_processed: u64 = 0;
            let mut integral_error: f64 = 0.0;
            let mut fill_sum: f64 = 0.0;
            let mut fill_min: f64 = 1.0;
            let mut fill_max: f64 = 0.0;

            loop {
                if dsp_token.is_cancelled() {
                    break;
                }
                let target = chunk_bytes - leftover;
                match file.read(&mut raw_buf[leftover..leftover + target]) {
                    Ok(0) => {
                        thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Ok(n) => {
                        leftover += n;
                        if leftover < chunk_bytes {
                            continue;
                        }
                    }
                    Err(e) => {
                        eprintln!("Read error: {e}");
                        break;
                    }
                }

                for frame in 0..RESAMPLE_CHUNK {
                    for ch in 0..CHANNELS {
                        let offset = (frame * CHANNELS + ch) * 4;
                        let sample = i32::from_le_bytes([
                            raw_buf[offset],
                            raw_buf[offset + 1],
                            raw_buf[offset + 2],
                            raw_buf[offset + 3],
                        ]);
                        input_buf[ch][frame] = sample as f64 / i32::MAX as f64;
                    }
                }
                leftover = 0;

                let fill = (BUFFER_CAPACITY - producer.slots()) as f64 / BUFFER_CAPACITY as f64;
                let error = FILL_TARGET - fill;

                integral_error = (integral_error + error).clamp(-50.0, 50.0);
                let p_term = error * P_GAIN;
                let i_term = integral_error * I_GAIN;
                let rel_ratio = (1.0 + p_term + i_term).clamp(0.95, 1.05);

                let _ = resampler.set_resample_ratio_relative(rel_ratio, false);

                chunks_processed += 1;
                fill_sum += fill;
                if fill < fill_min {
                    fill_min = fill;
                }
                if fill > fill_max {
                    fill_max = fill;
                }

                if last_status.elapsed().as_secs() >= 1 {
                    let fill_avg = fill_sum / chunks_processed as f64;
                    let effective_ratio = BASE_RATIO * rel_ratio;
                    eprintln!(
                        "[buf] fill: {:.1}% (avg {:.1}%, min {:.1}%, max {:.1}%) | ratio: {:.6} | chunks: {}",
                        fill * 100.0,
                        fill_avg * 100.0,
                        fill_min * 100.0,
                        fill_max * 100.0,
                        effective_ratio,
                        chunks_processed,
                    );
                    fill_sum = 0.0;
                    fill_min = 1.0;
                    fill_max = 0.0;
                    chunks_processed = 0;
                    last_status = Instant::now();
                }

                let output = match resampler.process(&input_buf, None) {
                    Ok(out) => out,
                    Err(e) => {
                        eprintln!("Resample error: {e}");
                        continue;
                    }
                };

                let out_frames = output[0].len();
                'frame_loop: for frame in 0..out_frames {
                    for ch in 0..CHANNELS {
                        let sample = (output[ch][frame] * i32::MAX as f64) as i32;
                        if producer.push(sample).is_err() {
                            eprintln!("[buf] overflow, dropping samples");
                            break 'frame_loop;
                        }
                    }
                }
            }
        }
    });

    // 4. Setup Axum Server
    // We pass config_tx via closure so the web routes can update the state
    let app = Router::new().route(
        "/update_config",
        post({
            let tx = config_tx.clone();
            move |Json(payload): Json<AudioRuntimeConfig>| async move {
                // Update the state instantly. The DSP thread will see it on its next loop.
                let _ = tx.send(payload);
                "Config updated successfully!"
            }
        }),
    );

    // 5. Run Axum server in a background Tokio task
    let listener = TcpListener::bind("0.0.0.0:3000").await?;
    println!("Web API listening on port 3000");

    let server_token = token.clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(server_token.cancelled_owned())
            .await
            .ok();
    });

    // 6. Wait for Graceful Shutdown (Ctrl+C)
    tokio::signal::ctrl_c().await?;
    println!("\nShutdown signal received...");

    // 7. Cleanup — cancel all tasks
    token.cancel();

    // Wait for the DSP thread to finish its last loop cleanly
    let _ = dsp_handle.join();

    println!("Application gracefully shut down.");
    Ok(())
}

pub fn spawn_metadata_thread(token: CancellationToken) {
    thread::spawn(move || {
        while !token.is_cancelled() {
            // Wait for the metadata pipe to be created by shairport-sync
            if !std::path::Path::new(METADATA_PATH).exists() {
                thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }

            let file = match File::open(METADATA_PATH) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("Failed to open metadata pipe: {}", e);
                    thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            };

            let mut reader = BufReader::new(file);
            let mut buffer = String::new();

            loop {
                if token.is_cancelled() {
                    return;
                }
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        // EOF reached, pipe was closed (shairport-sync restarted)
                        break;
                    }
                    Ok(_) => {
                        buffer.push_str(&line);

                        // Process complete <item> blocks
                        while let Some(end_idx) = buffer.find("<​/item>") {
                            let item_len = end_idx + 7;
                            let item_str = &buffer[..item_len];

                            process_metadata_item(item_str);

                            // Remove processed item from buffer
                            buffer = buffer[item_len..].to_string();
                        }
                    }
                    Err(e) => {
                        eprintln!("Metadata reader error: {}", e);
                        break;
                    }
                }
            }

            // If we break out of the inner loop, wait and try to reconnect
            thread::sleep(std::time::Duration::from_secs(1));
        }
    });
}

fn process_metadata_item(item: &str) {
    let type_hex = extract_tag_content(item, "type");
    let code_hex = extract_tag_content(item, "code");
    let data_b64 = extract_data_content(item);

    if let (Some(t_hex), Some(c_hex), Some(b64)) = (type_hex, code_hex, data_b64) {
        let t = hex_to_ascii(&t_hex);
        let c = hex_to_ascii(&c_hex);

        // Clean up base64 whitespace for proper decoding
        let b64_clean = b64.replace("\n", "").replace("\r", "").replace(" ", "");

        if t == "core" {
            if let Ok(decoded_bytes) = BASE64_STANDARD.decode(&b64_clean) {
                let val = String::from_utf8_lossy(&decoded_bytes).trim().to_string();
                if !val.is_empty() {
                    match c.as_str() {
                        "minm" => println!("🎵 Track:  {}", val),
                        "asar" => println!("🎤 Artist: {}", val),
                        "asal" => println!("💿 Album:  {}", val),
                        _ => {} // Ignore other core tags
                    }
                }
            }
        } else if t == "ssnc" {
            match c.as_str() {
                "pbeg" => println!("▶️ Playback started"),
                "pend" => println!("⏸ Playback stopped"),
                "pfls" => println!("🔁 Stream flushed (Seek/Skip)"),
                _ => {} // Ignore volume/artwork tags for now
            }
        }
    }
}

// Minimal helpers to avoid bringing in a massive XML parsing crate
fn extract_tag_content(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<​{}>", tag);
    let close = format!("<​/{}>", tag);
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)?;
    Some(xml[start..start + end].trim().to_string())
}

fn extract_data_content(xml: &str) -> Option<String> {
    let start = xml.find("<data")?;
    let close_bracket = xml[start..].find('>')?;
    let data_start = start + close_bracket + 1;
    let end_idx = xml[data_start..].find("<​/data>")?;
    Some(xml[data_start..data_start + end_idx].trim().to_string())
}

fn hex_to_ascii(hex: &str) -> String {
    let mut ascii = String::new();
    for i in 0..(hex.len() / 2) {
        if let Ok(b) = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16) {
            ascii.push(b as char);
        }
    }
    ascii
}
