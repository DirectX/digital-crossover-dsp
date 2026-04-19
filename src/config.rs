use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

pub const INPUT_RATE: u32 = 48000;
pub const CHANNELS: usize = 2;
pub const OUTPUT_CHANNELS: usize = 6;
pub const PIPE_PATH: &str = "/tmp/shairport-sync-audio";
pub const METADATA_PATH: &str = "/tmp/shairport-sync-metadata";
pub const RESAMPLE_CHUNK: usize = 1024;
pub const FILL_TARGET: f64 = 0.5;

pub const P_GAIN: f64 = 0.002;
pub const I_GAIN: f64 = 0.00005;
pub const POLL_TIMEOUT_MS: i32 = 200;

/// Number of samples per FFT window. Positive-frequency bins = FFT_SIZE / 2.
pub const FFT_SIZE: usize = 2048;

/// Pre-serialised JSON payload broadcast to WebSocket clients each FFT frame.
/// Using a `String` avoids re-serialising per-client.
pub type FftBroadcast = tokio::sync::broadcast::Sender<String>;

/// ALSA device name to open for 6-channel output.
/// Set to a raw hw: name (e.g. "hw:1,0") to bypass all ALSA plugins and
/// avoid LFE/surround resampling. Empty string = auto-detect first suitable device.
pub const DEVICE_NAME: &str = "";

fn default_master_volume() -> f32 {
    1.0
}
fn default_band_gain() -> f32 {
    1.0
}
fn default_low_cut() -> f32 {
    1000.0
}
fn default_mid_cut() -> f32 {
    10000.0
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AudioRuntimeConfig {
    #[serde(default = "default_master_volume")]
    pub volume: f32,
    #[serde(default = "default_band_gain")]
    pub low_gain: f32,
    #[serde(default = "default_band_gain")]
    pub mid_gain: f32,
    #[serde(default = "default_band_gain")]
    pub high_gain: f32,
    #[serde(default = "default_low_cut")]
    pub low_cut_hz: f32,
    #[serde(default = "default_mid_cut")]
    pub mid_cut_hz: f32,
    #[serde(default)]
    pub low_mute: bool,
    #[serde(default)]
    pub mid_mute: bool,
    #[serde(default)]
    pub high_mute: bool,
    #[serde(default)]
    pub low_solo: bool,
    #[serde(default)]
    pub mid_solo: bool,
    #[serde(default)]
    pub high_solo: bool,
    #[serde(default)]
    pub low_bypass: bool,
    #[serde(default)]
    pub mid_bypass: bool,
    #[serde(default)]
    pub high_bypass: bool,
}

impl Default for AudioRuntimeConfig {
    fn default() -> Self {
        Self {
            volume: default_master_volume(),
            low_gain: default_band_gain(),
            mid_gain: default_band_gain(),
            high_gain: default_band_gain(),
            low_cut_hz: default_low_cut(),
            mid_cut_hz: default_mid_cut(),
            low_mute: false,
            mid_mute: false,
            high_mute: false,
            low_solo: false,
            mid_solo: false,
            high_solo: false,
            low_bypass: false,
            mid_bypass: false,
            high_bypass: false,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AppState {
    pub track: String,
    pub artist: String,
    pub album: String,
    pub playback: String,
    pub buffer_fill: f64,
    pub buffer_fill_avg: f64,
    pub buffer_fill_min: f64,
    pub buffer_fill_max: f64,
    pub resample_ratio: f64,
    pub chunks_processed: u64,
    pub output_rate: u32,
    pub output_format: String,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            track: String::new(),
            artist: String::new(),
            album: String::new(),
            playback: "Unknown".to_string(),
            buffer_fill: 0.0,
            buffer_fill_avg: 0.0,
            buffer_fill_min: 0.0,
            buffer_fill_max: 0.0,
            resample_ratio: 0.0,
            chunks_processed: 0,
            output_rate: 0,
            output_format: String::new(),
        }
    }
}

pub type SharedState = Arc<Mutex<AppState>>;
