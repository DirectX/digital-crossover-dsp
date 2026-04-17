# Digital Crossover DSP

## Overview

```mermaid
flowchart TD
    AP["🎵 AirPlay2 Source\nShairPort / NQPTP"]
    PIPE["Unix Pipe — stdin\nPCM Stereo f32"]

    AP -->|"PCM stereo stream"| PIPE

    subgraph DSP["DSP Audio Server (Rust)"]
        IN["Input Stage\nStereo L/R · f32 PCM · ring buffer"]
        FIR["Crossover Engine · FIR Filter Bank\nlinear phase · overlap-save convolution"]

        IN --> FIR

        FIR --> LL["L-Low\n< 200 Hz\nCh 1"]
        FIR --> LM["L-Mid\n200–4k Hz\nCh 2"]
        FIR --> LH["L-High\n> 4k Hz\nCh 3"]
        FIR --> RL["R-Low\n< 200 Hz\nCh 4"]
        FIR --> RM["R-Mid\n200–4k Hz\nCh 5"]
        FIR --> RH["R-High\n> 4k Hz\nCh 6"]

        subgraph LCH["Left Channel"]
            LL
            LM
            LH
        end

        subgraph RCH["Right Channel"]
            RL
            RM
            RH
        end

        OUT["Output Stage\n6-ch interleaved f32 · ALSA / JACK"]
        API["Control API — axum / tokio\nREST · WebSocket · filter params · gain · delay · phase · metering"]

        LL & LM & LH & RL & RM & RH --> OUT
    end

    PIPE --> IN

    OUT -->|"6-ch PCM"| DAC["6-Ch USB Audio Interface\nDAC · analog outputs · Ch 1–6"]

    DAC --> A1["Amp L-Low\nCh 1"] & A2["Amp L-Mid\nCh 2"] & A3["Amp L-High\nCh 3"]
    DAC --> A4["Amp R-Low\nCh 4"] & A5["Amp R-Mid\nCh 5"] & A6["Amp R-High\nCh 6"]

    A1 --> D1["🔊 Woofer L"]
    A2 --> D2["🔊 Mid L"]
    A3 --> D3["🔊 Tweeter L"]
    A4 --> D4["🔊 Woofer R"]
    A5 --> D5["🔊 Mid R"]
    A6 --> D6["🔊 Tweeter R"]

    WEBUI["🖥️ Web UI"] <-->|"REST / WebSocket"| API

    style DSP fill:#dddfed,stroke:#2ec4b6,color:#333
    style LCH fill:#dddaed,stroke:#4a9eff,color:#333
    style RCH fill:#dddaed,stroke:#4a9eff,color:#333
    style API fill:#bdbacd,stroke:#747271,color:#333
    style WEBUI fill:#7d9faa,stroke:#2ec4b6,color:#fff
    style AP fill:#7d9faa,stroke:#4a9eff,color:#fff
    style PIPE fill:#7d9faa,stroke:#4a9eff,color:#fff
    style DAC fill:#7d9faa,stroke:#2ec4b6,color:#fff
```

## Prerequisites

### Shairport-Sync

#### Installation

https://github.com/mikebrady/shairport-sync/blob/master/BUILD.md

##### Prerequisites

```
apt update
apt upgrade # this is optional but recommended
apt install --no-install-recommends build-essential git autoconf automake libtool \
    libpopt-dev libconfig-dev libasound2-dev avahi-daemon libavahi-client-dev libssl-dev libsoxr-dev \
    libplist-dev libsodium-dev uuid-dev libgcrypt-dev xxd libplist-utils \
    libavutil-dev libavcodec-dev libavformat-dev systemd-dev
```

##### NQPTP

```
git clone https://github.com/mikebrady/nqptp.git
cd nqptp
autoreconf -fi # about a minute on a Raspberry Pi.
./configure --with-systemd-startup
make
make install
```

##### ShairPort Sync

```
git clone https://github.com/mikebrady/shairport-sync.git
cd shairport-sync
autoreconf -fi
./configure --sysconfdir=/etc --with-alsa --with-soxr --with-avahi --with-ssl=openssl --with-systemd-startup --with-airplay-2 --with-pipe
make
sudo make install
```

#### Configuration

##### Service Config

```shell
systemctl --user edit shairport-sync
```

```shell
[Service]
PrivateTmp=false
ExecStart=
ExecStart=/usr/local/bin/shairport-sync -o pipe
```

```shell
sudo systemctl daemon-reload
sudo systemctl restart shairport-sync
```

##### ShairPort Sync Config

```shell
sudo nano /etc/shairport-sync.conf
```

/etc/shairport-sync.conf
```
eneral = {
  name = "My AirPlay";
  ignore_volume_control = "yes";
};

alsa = {
  // Disabling system audio output
};

pipe = {
  name = "/tmp/shairport-sync-audio";  // UNIX pipe path
};

metadata =
{
    enabled = "yes"; // Set this to "yes" to get Shairport Sync to solicit metadata from the source and pass it on via a pipe
    include_cover_art = "yes"; // Set to "yes" to get cover art. "no" is the default.
    pipe_name = "/tmp/shairport-sync-metadata"; // The default name of the pipe where metadata is written.
    pipe_timeout = 5000; // Wait for this many milliseconds before giving up trying to write into the pipe.
};
```

```
systemctl --user enable shairport-sync
systemctl --user start shairport-sync
systemctl --user status shairport-sync
```

#### Testing (possible jitter)

```
ffplay -fflags nobuffer -f s32le -ar 48000 -ch_layout stereo /tmp/shairport-sync-audio
```

```
aplay -f S32_LE -r 48000 -c 2 /tmp/shairport-sync-audio
```

```
# sudo apt install sox
play -t raw --buffer 8192 -r 48000 -e signed -b 32 -c 2 -L /tmp/shairport-sync-audio
```

### Architecture

Reader/resampler thread:

Reads S32LE interleaved stereo from pipe at 44.1 kHz
Accumulates exactly RESAMPLE_CHUNK (1024) frames before processing
Converts i32 to f64 and de-interleaves into per-channel buffers
Before each process() call, computes buffer fill level and adjusts the resampling ratio via set_resample_ratio_relative()
Resamples 44.1 kHz to 96 kHz using SincFixedIn with sinc interpolation (256-tap, linear interp, BlackmanHarris2 window)
Pushes resampled i32 samples into the lock-free rtrb ring buffer
CPAL output callback (src/main.rs:135-141):

Configured at 96 kHz stereo
Simply pops samples from ring buffer (lock-free, real-time safe)
Buffer-state-driven speed control (src/main.rs:108-111):

fill = 1.0 - producer.slots() / capacity gives current fill ratio (0.0-1.0)
Proportional controller: rel_ratio = 1.0 + (0.5 - fill) * ADJUST_GAIN
Fill > 50% --> ratio decreases slightly --> fewer output samples --> buffer drains
Fill < 50% --> ratio increases slightly --> more output samples --> buffer fills
Clamped within the resampler's allowed range (1/1.01 to 1.01, i.e. +/-1%)
ADJUST_GAIN (0.0005) controls responsiveness -- increase for faster convergence, decrease for smoother output
