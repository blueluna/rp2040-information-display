# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Flash

```sh
# Build (release profile used for all embedded work)
cargo build --release

# Flash and run via probe-rs (configured in .cargo/config.toml)
cargo run --release

# Check only (faster than build, no linking)
cargo check
```

The runner in [.cargo/config.toml](.cargo/config.toml) calls `probe-rs run --probe 2e8a:000c --chip RP2040`. defmt RTT logs appear in the terminal during `cargo run`.

## Wi-Fi Credentials

Set before building — they are `env!()` constants compiled into the binary:

```sh
# .cargo/config.toml already has placeholder entries; override on the command line:
WIFI_SSID="myssid" WIFI_PASSWORD="mypassword" cargo run --release
```

## CYW43 Firmware

The binary embeds firmware via `include_bytes!`. The files must exist before building:

```sh
mkdir cyw43-firmware
curl -L -o cyw43-firmware/43439A0.bin \
  https://github.com/embassy-rs/embassy/raw/main/cyw43-firmware/43439A0.bin
curl -L -o cyw43-firmware/43439A0_clm.bin \
  https://github.com/embassy-rs/embassy/raw/main/cyw43-firmware/43439A0_clm.bin
```

## Architecture

Everything lives in [src/main.rs](src/main.rs). The application runs five concurrent Embassy tasks:

| Task | Responsibility |
|---|---|
| `cyw43_task` | Drives the CYW43439 Wi-Fi chip runner (must never block) |
| `net_task` | Drives the embassy-net network stack runner |
| `wifi_task` | Owns `cyw43::Control`; joins Wi-Fi with retry; publishes `WifiState` on `WIFI_CHANNEL`; monitors for link loss and reconnects |
| `ntp_task` | Subscribes to `WIFI_CHANNEL`; queries `pool.ntp.org` via SNTP once `NetworkUp`; sends Unix timestamp on `NTP_TIME_CHANNEL` |
| `main` (async) | Owns the display and RTC; selects on `WIFI_CHANNEL` (update display) and `NTP_TIME_CHANNEL` (set RTC) |

### Inter-task communication

- **`WIFI_CHANNEL`** — `PubSubChannel<_, WifiState, 4, 4, 1>`: broadcast Wi-Fi state to all subscribers. `wifi_task` is the sole publisher. `main` and `ntp_task` each hold a subscriber slot (4 max).
- **`NTP_TIME_CHANNEL`** — `Channel<_, u32, 1>`: single-producer single-consumer; carries a Unix timestamp (seconds) from `ntp_task` to `main`.

### Hardware peripherals

| Peripheral | Pins | Purpose |
|---|---|---|
| PIO0 + DMA_CH0 | PIN_23/24/25/29 | CYW43 SPI (Wi-Fi) |
| SPI0 + DMA_CH1/CH2 | PIN_16/17/18/19/20/21/26 | UC8151 e-ink display |
| I2C0 | PIN_4 (SDA), PIN_5 (SCL) | PCF85063A RTC |
| GPIO | PIN_10/11/12/13/14/15/22 | Power, buttons A/B/C/Up/Down, LED |

### Key design notes

- `no_std` / `no_main` — bare-metal, no allocator.
- All tasks use `'static` lifetimes; heap-allocated state uses `StaticCell`.
- `jiff` is used for Unix-timestamp → civil-datetime arithmetic; `time` 0.3 is kept as a minimal bridge dep because `pcf85063a::PCF85063::set_datetime` requires `time::PrimitiveDateTime`.
- `EmbassyTimestamp` (implements `sntpc::NtpTimestampGenerator`) uses `embassy_time::Instant::now()` as the originate-timestamp source — relative uptime is sufficient for SNTP.
- Both dev and release profiles use `opt-level = "z"` and LTO to minimise flash usage.
