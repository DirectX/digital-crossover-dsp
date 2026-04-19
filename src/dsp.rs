use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use rtrb::RingBuffer;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use rustfft::{FftPlanner, num_complex::Complex};
use std::f32::consts::PI;
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

pub fn run(
    token: CancellationToken,
    mut config_rx: watch::Receiver<AudioRuntimeConfig>,
    state: SharedState,
    fft_tx: FftBroadcast,
) {
    let host = cpal::default_host();

    let (device, output_rate) = match select_device(&host) {
        Some(d) => d,
        None => {
            eprintln!("[device] Error: no suitable 6-channel output device found.");
            eprintln!("[device] Set DEVICE_NAME in config.rs to specify a device manually.");
            return;
        }
    };
    println!("Output device: {}", device.name().unwrap_or_default());

    let sample_format = device
        .supported_output_configs()
        .expect("Cannot query device configs")
        .filter(|c| {
            c.channels() as usize == OUTPUT_CHANNELS
                && c.min_sample_rate().0 <= output_rate
                && c.max_sample_rate().0 >= output_rate
        })
        .max_by_key(|c| match c.sample_format() {
            SampleFormat::F32 => 5,
            SampleFormat::I32 => 4,
            SampleFormat::U32 => 3,
            SampleFormat::I16 => 2,
            SampleFormat::U16 => 1,
            _ => 0,
        })
        .map(|c| c.sample_format())
        .unwrap_or(SampleFormat::F32);
    println!("Device sample format: {:?}", sample_format);

    let stream_config = StreamConfig {
        channels: OUTPUT_CHANNELS as u16,
        sample_rate: cpal::SampleRate(output_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    let buffer_capacity: usize = output_rate as usize * OUTPUT_CHANNELS * 2;
    let base_ratio: f64 = output_rate as f64 / INPUT_RATE as f64;

    {
        let mut s = state.lock().unwrap();
        s.output_rate = output_rate;
        s.output_format = format!("{:?}", sample_format);
    }

    macro_rules! build_stream {
        ($T:ty, $consumer:expr) => {{
            let mut consumer = $consumer;
            device
                .build_output_stream(
                    &stream_config,
                    move |output: &mut [$T], _| {
                        for sample in output.iter_mut() {
                            *sample = <$T as cpal::FromSample<f32>>::from_sample_(
                                consumer.pop().unwrap_or(0.0),
                            );
                        }
                    },
                    |err| eprintln!("CPAL error: {err}"),
                    None,
                )
                .expect("Failed to build output stream")
        }};
    }

    while !token.is_cancelled() {
        let initial_cfg = config_rx.borrow_and_update().clone();
        let mut crossover = Crossover::new(&initial_cfg, output_rate as f32);
        println!("DSP starting with config: {:?}", initial_cfg);

        // FFT plan (reused across frames; FFT_SIZE constant so plan is stable).
        let mut fft_planner = FftPlanner::<f32>::new();
        let fft_plan = fft_planner.plan_fft_forward(FFT_SIZE);

        // Pre-compute Hann window coefficients.
        let hann: Vec<f32> = (0..FFT_SIZE)
            .map(|i| 0.5 * (1.0 - (2.0 * PI * i as f32 / FFT_SIZE as f32).cos()))
            .collect();

        // Mono sample accumulator for FFT (50% overlap).
        let mut fft_buf: Vec<f32> = Vec::with_capacity(FFT_SIZE);

        // Reusable scratch buffer for rustfft.
        let mut fft_scratch: Vec<Complex<f32>> =
            vec![Complex::default(); fft_plan.get_outofplace_scratch_len().max(1)];
        let mut fft_out: Vec<Complex<f32>> = vec![Complex::default(); FFT_SIZE];

        let (mut producer, consumer) = RingBuffer::<f32>::new(buffer_capacity);

        let stream = match sample_format {
            SampleFormat::F32 => build_stream!(f32, consumer),
            SampleFormat::I16 => build_stream!(i16, consumer),
            SampleFormat::I32 => build_stream!(i32, consumer),
            SampleFormat::U16 => build_stream!(u16, consumer),
            SampleFormat::U32 => build_stream!(u32, consumer),
            _ => {
                eprintln!(
                    "Unsupported sample format {:?}, falling back to F32",
                    sample_format
                );
                build_stream!(f32, consumer)
            }
        };

        stream.play().expect("Failed to start playback");
        println!(
            "Playback started at {output_rate} Hz, {OUTPUT_CHANNELS}ch, format={sample_format:?}."
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
            SincFixedIn::<f64>::new(base_ratio, 1.05, sinc_params, RESAMPLE_CHUNK, CHANNELS)
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

            let fill = (buffer_capacity - producer.slots()) as f64 / buffer_capacity as f64;
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
                let effective_ratio = base_ratio * rel_ratio;
                {
                    let mut s = state.lock().unwrap();
                    s.buffer_fill = fill * 100.0;
                    s.buffer_fill_avg = fill_avg * 100.0;
                    s.buffer_fill_min = fill_min * 100.0;
                    s.buffer_fill_max = fill_max * 100.0;
                    s.resample_ratio = effective_ratio;
                    s.chunks_processed = chunks_processed;
                    if s.playback == "Unknown" {
                        s.playback = "Playing".to_string();
                    }
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
                let r = if CHANNELS > 1 {
                    output[1][frame] as f32
                } else {
                    l
                };

                let six = crossover.process(l, r);

                // Accumulate mono mix for FFT
                let mono = (l + r) * 0.5;
                fft_buf.push(mono);
                if fft_buf.len() == FFT_SIZE {
                    // Apply Hann window and convert to complex
                    let mut fft_in: Vec<Complex<f32>> = fft_buf
                        .iter()
                        .zip(hann.iter())
                        .map(|(&s, &w)| Complex { re: s * w, im: 0.0 })
                        .collect();

                    fft_plan.process_outofplace_with_scratch(
                        &mut fft_in,
                        &mut fft_out,
                        &mut fft_scratch,
                    );

                    // Build magnitude array (positive frequencies only, dB)
                    let bins = FFT_SIZE / 2;
                    let scale = 2.0 / FFT_SIZE as f32;
                    let magnitudes: Vec<f32> = fft_out[..bins]
                        .iter()
                        .map(|c| {
                            let mag = c.norm() * scale;
                            20.0 * mag.max(1e-9).log10()
                        })
                        .collect();

                    let json = serde_json::json!({
                        "type": "fft",
                        "bins": magnitudes,
                        "sample_rate": output_rate,
                        "fft_size": FFT_SIZE,
                    })
                    .to_string();

                    let _ = fft_tx.send(json);

                    // 50% overlap: keep the second half for the next window
                    let half = FFT_SIZE / 2;
                    fft_buf.drain(..half);
                }

                for &sample in &six {
                    let clamped = sample.clamp(-1.0, 1.0);
                    if producer.push(clamped).is_err() {
                        eprintln!("[buf] overflow, dropping samples");
                        break 'frame_loop;
                    }
                }
            }
        }
    }
}

fn select_device(host: &cpal::Host) -> Option<(cpal::Device, u32)> {
    use cpal::traits::DeviceTrait;

    const PREFERRED_RATES: &[u32] = &[96000, 48000];

    let devices: Vec<cpal::Device> = host
        .output_devices()
        .expect("Cannot enumerate output devices")
        .collect();

    fn max_channels(d: &cpal::Device) -> u16 {
        d.supported_output_configs()
            .map(|cfgs| cfgs.map(|c| c.channels()).max().unwrap_or(0))
            .unwrap_or(0)
    }

    fn supports_6ch_at_rate(d: &cpal::Device, rate: u32) -> bool {
        d.supported_output_configs()
            .map(|mut cfgs| {
                cfgs.any(|c| {
                    c.channels() as usize == OUTPUT_CHANNELS
                        && c.min_sample_rate().0 <= rate
                        && c.max_sample_rate().0 >= rate
                })
            })
            .unwrap_or(false)
    }

    // Devices advertising more than 8 channels are virtual software routers
    // (e.g. PipeWire passthrough, ALSA surround plugins) not real 5.1 hardware.
    fn is_honest_hardware(d: &cpal::Device) -> bool {
        max_channels(d) <= 8
    }

    // "nvidia" is always GPU HDMI/DP audio on Linux — no NVidia analog or USB
    // audio cards exist. "intel" is intentionally excluded: hw:CARD=Intel,DEV=0
    // is typically the analog/headphone output; only DEV=3+ are HDMI, and we
    // cannot distinguish them by card name alone.
    fn is_hdmi_or_dp(name: &str) -> bool {
        let n = name.to_ascii_lowercase();
        n.contains("hdmi") || n.contains("displayport") || n.contains("dp,") || n.contains("nvidia")
    }

    println!("[device] Available output devices:");
    for d in &devices {
        let name = d.name().unwrap_or_else(|_| "<unknown>".into());
        let ch = max_channels(d);
        let rates: Vec<u32> = PREFERRED_RATES
            .iter()
            .filter(|&&r| supports_6ch_at_rate(d, r))
            .copied()
            .collect();
        let honest = is_honest_hardware(d);
        let hdmi = is_hdmi_or_dp(&name);
        println!("[device]   max={ch}ch  honest={honest}  hdmi={hdmi}  rates={rates:?}  {name}");
    }

    // If a specific device is configured, honour it and select the best rate.
    if !DEVICE_NAME.is_empty() {
        if let Some(d) = devices
            .iter()
            .find(|d| d.name().map(|n| n == DEVICE_NAME).unwrap_or(false))
        {
            for &rate in PREFERRED_RATES {
                if supports_6ch_at_rate(d, rate) {
                    let name = d.name().unwrap_or_default();
                    println!("[device] Using configured DEVICE_NAME: {DEVICE_NAME} @ {rate}Hz");
                    return devices
                        .into_iter()
                        .find(|d| d.name().map(|n| n == name).unwrap_or(false))
                        .map(|d| (d, rate));
                }
            }
            eprintln!(
                "[device] Warning: DEVICE_NAME '{DEVICE_NAME}' does not support 6ch, falling back"
            );
        } else {
            eprintln!("[device] Warning: DEVICE_NAME '{DEVICE_NAME}' not found, falling back");
        }
    }

    // Priority 1: non-hw: devices (PipeWire, Pulse, default), non-HDMI, with 6ch.
    // These route to real hardware transparently and are the correct way to reach
    // ALSA devices that PipeWire holds exclusively (e.g. the integrated ALC1220).
    // The is_honest_hardware filter is NOT applied here — PipeWire legitimately
    // reports 32ch max as its virtual routing capability, not because it is fake.
    for &rate in PREFERRED_RATES {
        if let Some(d) = devices.iter().find(|d| {
            d.name()
                .map(|n| {
                    !n.starts_with("hw:") && !is_hdmi_or_dp(&n) && supports_6ch_at_rate(d, rate)
                })
                .unwrap_or(false)
        }) {
            let name = d.name().unwrap_or_default();
            println!("[device] Auto-selected: {name} @ {rate}Hz");
            return devices
                .into_iter()
                .find(|d| d.name().map(|n| n == name).unwrap_or(false))
                .map(|d| (d, rate));
        }
    }

    // Priority 2: honest hardware, direct hw: (may fail on combined rate×channel
    // constraints not captured by supported_output_configs).
    for &rate in PREFERRED_RATES {
        if let Some(d) = devices.iter().find(|d| {
            d.name()
                .map(|n| {
                    is_honest_hardware(d)
                        && !is_hdmi_or_dp(&n)
                        && n.starts_with("hw:")
                        && supports_6ch_at_rate(d, rate)
                })
                .unwrap_or(false)
        }) {
            let name = d.name().unwrap_or_default();
            eprintln!(
                "[device] Warning: using direct hw: device {name} @ {rate}Hz — format negotiation may fail"
            );
            return devices
                .into_iter()
                .find(|d| d.name().map(|n| n == name).unwrap_or(false))
                .map(|d| (d, rate));
        }
    }

    // Priority 3: honest hardware that happens to be HDMI/DP (e.g. AV receiver
    // connected via HDMI — a valid 5.1 sink).
    for &rate in PREFERRED_RATES {
        if let Some(d) = devices
            .iter()
            .find(|d| is_honest_hardware(d) && supports_6ch_at_rate(d, rate))
        {
            let name = d.name().unwrap_or_default();
            eprintln!("[device] Warning: only honest HDMI/DP 6ch device found: {name} @ {rate}Hz");
            return devices
                .into_iter()
                .find(|d| d.name().map(|n| n == name).unwrap_or(false))
                .map(|d| (d, rate));
        }
    }

    eprintln!("[device] Error: no real 6-channel hardware device found.");
    eprintln!("[device] All devices with 6ch support appear to be virtual (>8ch max).");
    eprintln!("[device] Set DEVICE_NAME in config.rs to override.");
    None
}
