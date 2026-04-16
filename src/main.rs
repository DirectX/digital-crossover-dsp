use cpal::StreamConfig;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::thread;

const SAMPLE_RATE: u32 = 44100;
const CHANNELS: u16 = 2;
const PIPE_PATH: &str = "/tmp/shairport-sync-audio";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    let device = host.default_output_device().expect("Нет устройства вывода");
    println!("Устройство вывода: {}", device.name()?);

    let config = StreamConfig {
        channels: CHANNELS,
        sample_rate: cpal::SampleRate(SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };

    // Разделяемый буфер через Mutex<VecDeque>
    let buffer: Arc<Mutex<VecDeque<i32>>> = Arc::new(Mutex::new(VecDeque::new()));
    let buffer_writer = Arc::clone(&buffer);
    let buffer_reader = Arc::clone(&buffer);

    // Поток чтения из pipe
    thread::spawn(move || {
        loop {
            if std::path::Path::new(PIPE_PATH).exists() {
                break;
            }
            println!("Ожидание pipe {PIPE_PATH}...");
            thread::sleep(std::time::Duration::from_secs(1));
        }

        let mut file = OpenOptions::new()
            .read(true)
            .open(PIPE_PATH)
            .expect("Не удалось открыть pipe");

        println!("Pipe открыт, читаем аудио...");
        let mut buf = [0u8; 4096];

        loop {
            match file.read(&mut buf) {
                Ok(0) => thread::sleep(std::time::Duration::from_millis(10)),
                Ok(n) => {
                    let samples: Vec<i32> = buf[..n]
                        .chunks_exact(4)
                        .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                        .collect();
                    let mut q = buffer_writer.lock().unwrap();
                    // Ограничиваем размер буфера (~2 сек)
                    const MAX: usize = 44100 * 2 * 2;
                    if q.len() < MAX {
                        q.extend(samples);
                    }
                }
                Err(e) => {
                    eprintln!("Ошибка: {e}");
                    break;
                }
            }
        }
    });

    // CPAL output stream
    let stream = device.build_output_stream(
        &config,
        move |output: &mut [i32], _| {
            let mut q = buffer_reader.lock().unwrap();
            for sample in output.iter_mut() {
                *sample = q.pop_front().unwrap_or(0);
            }
        },
        |err| eprintln!("Ошибка CPAL: {err}"),
        None,
    )?;

    stream.play()?;
    println!("Воспроизведение запущено. Ctrl+C для выхода.");

    loop {
        thread::sleep(std::time::Duration::from_secs(60));
    }
}
