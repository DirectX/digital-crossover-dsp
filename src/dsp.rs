use cpal::StreamConfig;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rtrb::RingBuffer;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::thread;
use std::time::Instant;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::config::*;
use crate::crossover::Crossover;
use crate::pipe::poll_readable;

pub fn run(token: CancellationToken, mut config_rx: watch::Receiver<AudioRuntimeConfig>, state: SharedState) {
    let host = cpal::default_host();

    let device = select_device(&host);
    println!("Output device: {}", device.name().unwrap_or_default());

    let config = StreamConfig {
        channels: OUTPUT_CHANNELS as u16,
        sample_rate: cpal::SampleRate(OUTPUT_RATE),
        buffer_size: cpal::BufferSize::Default,
    };

    while !token.is_cancelled() {
        let initial_cfg = config_rx.borrow_and_update().clone();
        let mut crossover = Crossover::new(&initial_cfg);
        println!("DSP starting with config: {:?}", initial_cfg);

        let (mut producer, mut consumer) = RingBuffer::<i32>::new(BUFFER_CAPACITY);

        let stream = device
            .build_output_stream(
                &config,
                move |output: &mut [i32], _| {
                    for sample in output.iter_mut() {
                        *sample = consumer.pop().unwrap_or(0);
                    }
                },
                |err| eprintln!("CPAL error: {err}"),
                None,
            )
            .expect("Failed to build output stream");

        stream.play().expect("Failed to start playback");
        println!(
            "Playback started at {OUTPUT_RATE} Hz, {OUTPUT_CHANNELS}ch. Ctrl+C to exit."
        );

        loop {
            if token.is_cancelled() {
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
            .custom_flags(libc::O_NONBLOCK)
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
            if token.is_cancelled() {
                break;
            }

            if config_rx.has_changed().unwrap_or(false) {
                let latest = config_rx.borrow_and_update().clone();
                println!("DSP applying new config: {:?}", latest);
                crossover.update(&latest);
            }

            if !poll_readable(file.as_raw_fd(), POLL_TIMEOUT_MS) {
                continue;
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
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    continue;
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
                {
                    let mut s = state.lock().unwrap();
                    s.buffer_fill = fill * 100.0;
                    s.buffer_fill_avg = fill_avg * 100.0;
                    s.buffer_fill_min = fill_min * 100.0;
                    s.buffer_fill_max = fill_max * 100.0;
                    s.resample_ratio = effective_ratio;
                    s.chunks_processed = chunks_processed;
                }
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
                let l = output[0][frame] as f32;
                let r = if CHANNELS > 1 { output[1][frame] as f32 } else { l };

                let six = crossover.process(l, r);

                for &sample in &six {
                    let clamped = sample.clamp(-1.0, 1.0);
                    let out_i32 = (clamped as f64 * i32::MAX as f64) as i32;
                    if producer.push(out_i32).is_err() {
                        eprintln!("[buf] overflow, dropping samples");
                        break 'frame_loop;
                    }
                }
            }
        }
    }
}

/// Select the best available 6-channel output device.
///
/// Priority order:
///   1. Exact name match against DEVICE_NAME constant (if non-empty)
///   2. Any raw `hw:` device supporting OUTPUT_CHANNELS ch at OUTPUT_RATE
///   3. Any device supporting OUTPUT_CHANNELS ch at OUTPUT_RATE (plugin fallback)
///   4. Default output device (last resort — may not support 6ch)
fn select_device(host: &cpal::Host) -> cpal::Device {
    use cpal::traits::DeviceTrait;

    let devices: Vec<cpal::Device> = host
        .output_devices()
        .expect("Cannot enumerate output devices")
        .collect();

    fn supports_6ch(d: &cpal::Device) -> bool {
        d.supported_output_configs()
            .map(|mut cfgs| {
                cfgs.any(|c| {
                    c.channels() as usize == OUTPUT_CHANNELS
                        && c.min_sample_rate().0 <= OUTPUT_RATE
                        && c.max_sample_rate().0 >= OUTPUT_RATE
                })
            })
            .unwrap_or(false)
    }

    // 1. Explicit name override
    if !DEVICE_NAME.is_empty() {
        if devices.iter().any(|d| d.name().map(|n| n == DEVICE_NAME).unwrap_or(false)) {
            println!("[device] Using configured device: {}", DEVICE_NAME);
            return devices.into_iter().find(|d| {
                d.name().map(|n| n == DEVICE_NAME).unwrap_or(false)
            }).unwrap();
        }
        eprintln!("[device] Warning: DEVICE_NAME '{}' not found, falling back", DEVICE_NAME);
    }

    // 2. Raw hw: device with 6ch
    if let Some(d) = devices.iter().find(|d| {
        d.name()
            .map(|n| n.starts_with("hw:") && supports_6ch(d))
            .unwrap_or(false)
    }) {
        let name = d.name().unwrap_or_default();
        println!("[device] Auto-selected raw hw: device: {name}");
        return devices.into_iter().find(|d| {
            d.name().map(|n| n == name).unwrap_or(false)
        }).unwrap();
    }

    // 3. Any 6ch capable device (plugin, plughw, etc.)
    if let Some(d) = devices.iter().find(|d| supports_6ch(d)) {
        let name = d.name().unwrap_or_default();
        eprintln!("[device] Warning: no raw hw: device found, using plugin device: {name}");
        return devices.into_iter().find(|d| {
            d.name().map(|n| n == name).unwrap_or(false)
        }).unwrap();
    }

    // 4. Default fallback
    eprintln!("[device] Warning: no 6-channel device found, using system default");
    host.default_output_device().expect("No output device available")
}
