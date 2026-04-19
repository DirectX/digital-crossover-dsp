# digital-crossover-dsp

## Overview

Real-time 3-way digital crossover DSP engine for 5.1 surround output. Reads stereo PCM audio from a named pipe (shairport-sync), resamples it, splits it into Low / Mid / High frequency bands using 4th-order Linkwitz–Riley filters, applies per-band gain/mute/solo/bypass, and writes 6-channel interleaved audio to a CPAL output device.

A REST/WebSocket API exposes runtime control and FFT spectrum data. A full-featured Ratatui terminal UI (`tui` subcommand) connects to the API for live monitoring and control.

---

## Build & Run

```bash
# Build
cargo build --release

# Start the DSP engine + API server
./target/release/digital-crossover-dsp serve

# Start the terminal UI (connects to a running server)
./target/release/digital-crossover-dsp tui
./target/release/digital-crossover-dsp tui --url http://192.168.1.x:3000
```

### Tests

```bash
cargo test
```

The `crossover` module has unit tests covering DC routing, HF routing, and all-pass magnitude preservation of the 3-way LR4 splitter.

---

## Architecture

```
shairport-sync pipe (/tmp/shairport-sync-audio)
        │
        ▼
   dsp::run()  [dedicated OS thread]
        │  reads raw S32LE stereo @ 48 kHz
        │  SincFixedIn resampler (rubato) → device sample rate
        │  Crossover::process() → 6-ch interleaved f32
        │  rtrb lock-free ring buffer
        │  FFT accumulator (2048-pt Hann, 50% overlap)
        │         │
        ▼         ▼
  CPAL output   fft_tx (broadcast::Sender<String>)
  stream        │
                ▼
          server::spawn() [tokio async]
                │
        ┌───────┴──────────────┐
        │  REST API :3000      │  WebSocket /ws/fft
        │  GET  /config        │  streams FFT JSON to
        │  POST /update_config │  all connected clients
        │  GET  /status        │
        └──────────────────────┘
                ▲
        metadata::spawn_thread() [OS thread]
        reads /tmp/shairport-sync-metadata XML
        updates track/artist/album/playback state
```

---

## Source Files

### `src/config.rs`
Global constants and shared types:
- `INPUT_RATE` (48000), `CHANNELS` (2), `OUTPUT_CHANNELS` (6)
- `PIPE_PATH` (`/tmp/shairport-sync-audio`), `METADATA_PATH`
- `RESAMPLE_CHUNK` (1024), `FILL_TARGET` (0.5), `P_GAIN`, `I_GAIN`
- `FFT_SIZE` (2048) — window size; positive-frequency bins = 1024
- `DEVICE_NAME` — set to `"hw:N,M"` to pin to a specific ALSA device; empty = auto-detect first 6-channel device
- `FftBroadcast` type alias: `tokio::sync::broadcast::Sender<String>`
- `AudioRuntimeConfig` — serde-serialisable config struct sent via REST (see API section)
- `AppState` — runtime metrics struct served at `GET /status`
- `SharedState` = `Arc<Mutex<AppState>>`

### `src/crossover.rs`
DSP signal path:
- **`Biquad`** — direct-form I biquad filter (RBJ cookbook conventions), single-channel f32
- **`Lr4`** — 4th-order Linkwitz–Riley filter = two cascaded Biquads; constructors: `lpf(fc, fs)` / `hpf(fc, fs)`
- **`LrBandSplitter`** — per-channel 3-way splitter; contains 4 × Lr4 (`lpf_low`, `hpf_low`, `lpf_mid`, `hpf_mid`). Implements `BandSplitter::split(sample) → (lo, mid, hi)` for one channel at a time.
- **`Crossover`** — top-level stereo processor; owns two `LrBandSplitter` instances (`left`, `right`) plus all per-band runtime flags. `new(&cfg, sample_rate)` / `update(&cfg)` / `process(l, r) → [f32; 6]`
- Output channel mapping (canonical 5.1 interleaved order):
  - 0 FL → Mid L, 1 FR → Mid R, 2 FC → Low L, 3 LFE → Low R, 4 RL → High L, 5 RR → High R

Per-band flags applied in `process()`:
- **Solo** — any band soloed makes the other two silent; multiple solos clear each other (enforced in TUI)
- **Mute** — silences the band regardless of solo
- **Bypass** — substitutes the unfiltered raw input for that band's output (filter is still computed but discarded)

### `src/dsp.rs`
The audio thread (`pub fn run(token, config_rx, state, fft_tx)`):
1. Selects the best available 6-channel output device via `select_device()` (prefers 96 kHz then 48 kHz, prefers F32 sample format)
2. Builds a CPAL output stream consuming an `rtrb` ring buffer (capacity = 2 × device_rate × 6)
3. Waits for the pipe at `PIPE_PATH` to appear
4. Reads raw S32LE stereo frames in chunks of `RESAMPLE_CHUNK` (1024 frames)
5. Resamples with `SincFixedIn` (256-tap Blackman–Harris, 256× oversampling) and a PI controller on buffer fill (target 50%)
6. Per frame: runs `Crossover::process()`, accumulates FFT mono mix, pushes 6 samples into the ring buffer
7. **FFT**: 2048-pt Hann-windowed forward FFT, 50% overlap. On each completed window, computes 1024 positive-frequency magnitudes in dBFS (20 × log₁₀), serialises as JSON and sends via `fft_tx`. Approximate rate: ~23 frames/s at 48 kHz.
8. Updates `AppState` metrics once per second; promotes `playback` from `"Unknown"` to `"Playing"` on first stats tick.

### `src/server.rs`
Axum HTTP + WebSocket server on port 3000:
- `POST /update_config` — accepts `AudioRuntimeConfig` JSON body, sends to DSP via `watch::Sender`
- `GET /config` — returns current `AudioRuntimeConfig` as JSON
- `GET /status` — returns current `AppState` as JSON
- `GET /ws/fft` — WebSocket upgrade; each connected client receives one JSON message per FFT frame:
  ```json
  { "type": "fft", "bins": [/* 1024 f32 dBFS values */], "sample_rate": 48000, "fft_size": 2048 }
  ```
  Lagged receivers silently skip frames. Client disconnect is detected on send error.

### `src/tui.rs`
Ratatui terminal UI (`pub async fn run(base_url)`):

**Startup**: fetches initial config from `GET /config`, spawns a background tokio task that connects to `ws://host/ws/fft` (auto-converts `http://` → `ws://`), stores received FFT bins in `Arc<Mutex<Vec<f32>>>`. Reconnects every 2 s on disconnect.

**Main loop** (250 ms poll):
1. Fetches `GET /status`
2. Snapshots FFT bins
3. Renders UI
4. Handles keyboard input; POSTs config changes to `POST /update_config`

**Layout** (top to bottom):
| Pane | Height | Content |
|---|---|---|
| Title / hints | 3 | App name + quick key reference |
| Now Playing | 6 | Track, Artist, Album, playback status (colour-coded) |
| DSP Stats | 8 | Buffer fill gauge + Avg/Min/Max/ratio/output info |
| Gains | 6 | Master + Low + Mid + High gain gauges with [M][S][B] badges |
| Crossover | 4 | Low cut + Mid cut frequency gauges (log scale) |
| Spectrum | Min 8 | Live FFT spectrum (see below) |
| Footer | 3 | Key hints or last error in red |

**Key bindings**:
| Key | Action |
|---|---|
| `1` | Select Master |
| `2` | Select Low |
| `3` | Select Mid |
| `4` | Select High |
| `5` | Select Low cut |
| `6` | Select Mid cut |
| `Tab` / `↓` | Next selection |
| `Shift+Tab` / `↑` | Previous selection |
| `←` / `-` | Decrease value |
| `→` / `+` / `=` | Increase value |
| `Shift+←/→` | Fine adjust |
| `0` | Reset to default |
| `r` | Reset all to defaults |
| `m` | Toggle mute (Low/Mid/High) |
| `s` | Toggle solo (exclusive — clears other bands) |
| `b` | Toggle bypass (Low/Mid/High) |
| `q` / `Esc` | Quit |

**Spectrum display** (`draw_spectrum`):
- Log-index frequency mapping: each terminal column covers `n_bins^(col/cols)` to `n_bins^((col+1)/cols)` bins (approximates log-frequency scale)
- Sub-cell height precision via Unicode block elements `▁▂▃▄▅▆▇█`
- dB range: −80 dBFS (floor) to 0 dBFS (full scale)
- Colours match band boundaries using actual crossover frequencies:
  - **Green**: Low band (0 Hz → `low_cut_hz`)
  - **Yellow**: Mid band (`low_cut_hz` → `mid_cut_hz`)
  - **Magenta**: High band (`mid_cut_hz` → Nyquist)
- Title shows Nyquist frequency from `status.output_rate`

### `src/metadata.rs`
Dedicated OS thread that polls `/tmp/shairport-sync-metadata` (shairport-sync XML pipe) using `libc::poll`. Parses `<item>` XML blobs with minimal hand-rolled helpers (no XML crate dependency). Updates `AppState` fields:
- `track` (`minm`), `artist` (`asar`), `album` (`asal`) — base64-decoded, sanitised
- `playback`: `pbeg` → `"Playing"`, `pend` → `"Stopped"`, `pfls` → `"Flushed"`

### `src/pipe.rs`
Single helper: `poll_readable(fd, timeout_ms) -> bool` — wraps `libc::poll` for non-blocking readable check on a raw file descriptor.

---

## Runtime Configuration (`AudioRuntimeConfig`)

All fields are serde-defaulted (missing keys use defaults):

| Field | Type | Default | Description |
|---|---|---|---|
| `volume` | f32 | 1.0 | Master gain multiplier |
| `low_gain` | f32 | 1.0 | Low band gain |
| `mid_gain` | f32 | 1.0 | Mid band gain |
| `high_gain` | f32 | 1.0 | High band gain |
| `low_cut_hz` | f32 | 1000.0 | Low/Mid crossover frequency (Hz) |
| `mid_cut_hz` | f32 | 10000.0 | Mid/High crossover frequency (Hz) |
| `low_mute` | bool | false | Mute Low band |
| `mid_mute` | bool | false | Mute Mid band |
| `high_mute` | bool | false | Mute High band |
| `low_solo` | bool | false | Solo Low band |
| `mid_solo` | bool | false | Solo Mid band |
| `high_solo` | bool | false | Solo High band |
| `low_bypass` | bool | false | Bypass Low filter (raw input → Low outputs) |
| `mid_bypass` | bool | false | Bypass Mid filter |
| `high_bypass` | bool | false | Bypass High filter |

Gain range: 0.0–2.0 (TUI). Frequency range: 20–20000 Hz (Low cut capped below Mid cut − 100 Hz).

---

## Dependencies

| Crate | Purpose |
|---|---|
| `cpal` | Cross-platform audio I/O |
| `rtrb` | Lock-free single-producer/single-consumer ring buffer |
| `rubato` | High-quality SincFixedIn resampler |
| `rustfft` | FFT (2048-pt forward, Hann window) |
| `tokio` | Async runtime (full features) |
| `axum` + `ws` feature | HTTP + WebSocket server |
| `tokio-tungstenite` | WebSocket client (TUI) |
| `futures-util` | Stream combinators for WS client |
| `serde` + `serde_json` | Config/state serialisation |
| `tokio-util` | `CancellationToken` |
| `clap` | CLI argument parsing |
| `ratatui` + `crossterm` | Terminal UI |
| `reqwest` | HTTP client (TUI polls REST API) |
| `base64` | Decode metadata payloads |
| `libc` | `poll(2)`, `O_NONBLOCK` pipe access |

---

## Notes for Future Work

- `DEVICE_NAME` in `config.rs` — set to a raw `hw:N,M` ALSA node to bypass PipeWire/PulseAudio routing if channel reordering occurs
- The FFT WebSocket can also be consumed by a Next.js frontend; connect to `ws://host:3000/ws/fft` and use `bins` (1024 dBFS floats) with `sample_rate / fft_size` Hz per bin for the frequency axis
- The `PassthroughSplitter` stub in `crossover.rs` exists as a sanity/bring-up fallback and is otherwise unused
