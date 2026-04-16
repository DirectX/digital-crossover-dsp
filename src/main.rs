use cpal::StreamConfig;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rtrb::RingBuffer;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::fs::OpenOptions;
use std::io::Read;
use std::thread;
use std::time::Instant;

const INPUT_RATE: u32 = 44100;
const OUTPUT_RATE: u32 = 96000;
const CHANNELS: usize = 2;
const PIPE_PATH: &str = "/tmp/shairport-sync-audio";
const RESAMPLE_CHUNK: usize = 1024;
const BUFFER_CAPACITY: usize = OUTPUT_RATE as usize * CHANNELS * 2 * 15;
const BASE_RATIO: f64 = OUTPUT_RATE as f64 / INPUT_RATE as f64;
const FILL_TARGET: f64 = 0.5;
const ADJUST_GAIN: f64 = 0.0005;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    let device = host.default_output_device().expect("No output device");
    println!("Output device: {}", device.name()?);

    let config = StreamConfig {
        channels: CHANNELS as u16,
        sample_rate: cpal::SampleRate(OUTPUT_RATE),
        buffer_size: cpal::BufferSize::Default,
    };

    let (mut producer, mut consumer) = RingBuffer::<i32>::new(BUFFER_CAPACITY);

    thread::spawn(move || {
        loop {
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

        let mut resampler = SincFixedIn::<f64>::new(
            BASE_RATIO,
            1.01,
            sinc_params,
            RESAMPLE_CHUNK,
            CHANNELS,
        )
        .expect("Failed to create resampler");

        let mut input_buf = vec![vec![0.0f64; RESAMPLE_CHUNK]; CHANNELS];
        let mut raw_buf = [0u8; RESAMPLE_CHUNK * CHANNELS * 4];
        let frame_bytes = CHANNELS * 4;
        let chunk_bytes = RESAMPLE_CHUNK * frame_bytes;
        let mut leftover = 0usize;
        let mut last_status = Instant::now();
        let mut chunks_processed: u64 = 0;
        let mut fill_sum: f64 = 0.0;
        let mut fill_min: f64 = 1.0;
        let mut fill_max: f64 = 0.0;

        loop {
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

            let fill = 1.0 - producer.slots() as f64 / BUFFER_CAPACITY as f64;
            let error = FILL_TARGET - fill;
            let rel_ratio = (1.0 + error * ADJUST_GAIN).clamp(1.0 / 1.01, 1.01);
            let _ = resampler.set_resample_ratio_relative(rel_ratio, false);

            chunks_processed += 1;
            fill_sum += fill;
            if fill < fill_min { fill_min = fill; }
            if fill > fill_max { fill_max = fill; }

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
            for frame in 0..out_frames {
                for ch in 0..CHANNELS {
                    let sample = (output[ch][frame] * i32::MAX as f64) as i32;
                    if producer.push(sample).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let stream = device.build_output_stream(
        &config,
        move |output: &mut [i32], _| {
            for sample in output.iter_mut() {
                *sample = consumer.pop().unwrap_or(0);
            }
        },
        |err| eprintln!("CPAL error: {err}"),
        None,
    )?;

    stream.play()?;
    println!("Playback started at {OUTPUT_RATE} Hz. Ctrl+C to exit.");

    loop {
        thread::sleep(std::time::Duration::from_secs(60));
    }
}
