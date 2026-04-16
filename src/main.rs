use cpal::StreamConfig;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rtrb::RingBuffer;
use std::fs::OpenOptions;
use std::io::Read;
use std::thread;

const SAMPLE_RATE: u32 = 44100;
const CHANNELS: u16 = 2;
const PIPE_PATH: &str = "/tmp/shairport-sync-audio";
const BUFFER_CAPACITY: usize = SAMPLE_RATE as usize * CHANNELS as usize * 2;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    let device = host.default_output_device().expect("Нет устройства вывода");
    println!("Устройство вывода: {}", device.name()?);

    let config = StreamConfig {
        channels: CHANNELS,
        sample_rate: cpal::SampleRate(SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };

    let (mut producer, mut consumer) = RingBuffer::<i32>::new(BUFFER_CAPACITY);

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
                    let samples = buf[..n]
                        .chunks_exact(4)
                        .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                    for sample in samples {
                        if producer.push(sample).is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Ошибка: {e}");
                    break;
                }
            }
        }
    });

    let stream = device.build_output_stream(
        &config,
        move |output: &mut [i32], _| {
            let fill = consumer.slots() as f32 / BUFFER_CAPACITY as f32;
            if fill < 0.1 {
                eprintln!("Буфер почти пуст: {:.1}%", fill * 100.0);
            }
            for sample in output.iter_mut() {
                *sample = consumer.pop().unwrap_or(0);
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
