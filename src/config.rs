use serde::Deserialize;

pub const INPUT_RATE: u32 = 48000;
pub const OUTPUT_RATE: u32 = 96000;
pub const CHANNELS: usize = 2;
pub const PIPE_PATH: &str = "/tmp/shairport-sync-audio";
pub const METADATA_PATH: &str = "/tmp/shairport-sync-metadata";
pub const RESAMPLE_CHUNK: usize = 1024;
pub const BUFFER_CAPACITY: usize = OUTPUT_RATE as usize * CHANNELS * 2;
pub const BASE_RATIO: f64 = OUTPUT_RATE as f64 / INPUT_RATE as f64;
pub const FILL_TARGET: f64 = 0.5;

pub const P_GAIN: f64 = 0.002;
pub const I_GAIN: f64 = 0.00005;
pub const POLL_TIMEOUT_MS: i32 = 200;

#[derive(Clone, Debug, Deserialize)]
pub struct AudioRuntimeConfig {
    pub filter_cutoff: f32,
    pub volume: f32,
}