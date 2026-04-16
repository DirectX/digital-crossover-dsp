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
$ git clone https://github.com/mikebrady/nqptp.git
$ cd nqptp
$ autoreconf -fi # about a minute on a Raspberry Pi.
$ ./configure --with-systemd-startup
$ make
# make install
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

```shell
sudo nano /etc/shairport-sync.conf
```

/etc/shairport-sync.conf
```
eneral = {
  name = "My AirPlay";
  ignore_volume_control = "yes";
};

// Отключаем вывод на системное устройство, используем pipe
alsa = {
  // Disabling system audio output
};

pipe = {
  name = "/tmp/shairport-sync-audio";  // UNIX pipe path
};
```

```
systemctl --user enable shairport-sync
systemctl --user start shairport-sync
systemctl --user status shairport-sync
```

#### Testing (possible jitter)

```
ffplay -fflags nobuffer -f s32le -ar 44100 -ch_layout stereo /tmp/shairport-sync-audio
```

```
aplay -f S32_LE -r 44100 -c 2 /tmp/shairport-sync-audio
```

```
# sudo apt install sox
play -t raw --buffer 8192 -r 44100 -e signed -b 32 -c 2 -L /tmp/shairport-sync-audio
```
