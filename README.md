# librtlsdr-rs

[![CI](https://github.com/jasonherald/librtlsdr-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/jasonherald/librtlsdr-rs/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/librtlsdr-rs.svg)](https://crates.io/crates/librtlsdr-rs)
[![docs.rs](https://docs.rs/librtlsdr-rs/badge.svg)](https://docs.rs/librtlsdr-rs)
[![License: GPL-2.0-or-later](https://img.shields.io/badge/license-GPL--2.0--or--later-blue.svg)](LICENSE)

Pure-Rust port of [librtlsdr]. Talks to RTL2832U-based DVB-T dongles
directly over USB via [`rusb`] — no C `librtlsdr` library, no headers,
no `pkg-config`. Covers all five tuner families shipped in real-world
dongles: R820T / R820T2 / R828D, E4000, FC0012, FC0013, FC2580.

[librtlsdr]: https://github.com/steve-m/librtlsdr
[`rusb`]: https://docs.rs/rusb

## Quick start

```rust,no_run
use librtlsdr_rs::{RtlSdrDevice, RtlSdrError};

fn main() -> Result<(), RtlSdrError> {
    let mut dev = RtlSdrDevice::open(0)?;

    dev.set_center_freq(100_000_000)?;     // 100 MHz
    dev.set_sample_rate(2_048_000)?;       // 2.048 Msps
    dev.set_tuner_gain_mode(true)?;
    dev.set_tuner_gain(144)?;              // 14.4 dB

    dev.reset_buffer()?;
    let mut buf = vec![0u8; 65_536];
    let n = dev.read_sync(&mut buf)?;
    println!("read {n} bytes of interleaved I/Q");
    Ok(())
}
```

Sample values are interleaved unsigned 8-bit I/Q pairs, the native
RTL-SDR format. Convert to centred `i8` (or `f32` in `[-1, 1]`) at the
consumer if needed.

## Streaming

For long-running capture, the **Reader split** lets one half of the
device stream samples on a worker thread while the other half retains
control of tuning, gain, and bias-T:

```rust,no_run
use librtlsdr_rs::RtlSdrDevice;

# fn main() -> Result<(), librtlsdr_rs::RtlSdrError> {
let mut device = RtlSdrDevice::open(0)?;
device.set_sample_rate(2_400_000)?;
device.set_center_freq(100_000_000)?;
device.reset_buffer()?;

let reader = device.reader();
let worker = std::thread::spawn(move || {
    for chunk in reader.iter_samples(262_144) {
        match chunk {
            Ok(buf) => { /* push to ring / DSP */ let _ = buf; }
            Err(e) => { eprintln!("read error: {e}"); break; }
        }
    }
});

// Parent retains control of the device while the reader streams —
// separate USB endpoints, no rusb-level conflict.
device.set_center_freq(101_000_000)?;
device.set_tuner_gain(150)?;
# let _ = worker;
# Ok(())
# }
```

## Async runtime support

Per-runtime async `Stream` adapters are gated behind cargo features —
no async runtime is pulled in by default.

| Feature   | Method                                | Backend                                            |
| --------- | ------------------------------------- | -------------------------------------------------- |
| `tokio`   | `RtlSdrReader::stream_samples_tokio`  | `tokio::task::spawn_blocking` + `tokio::sync::mpsc` |
| `smol`    | `RtlSdrReader::stream_samples_smol`   | `blocking::unblock` + `async-channel`              |

> async-std users — please migrate to smol (the upstream-recommended
> replacement now that async-std is unmaintained per [RUSTSEC-2025-0052]).
> The smol feature uses the same `blocking::unblock` primitive
> async-std previously offered.

[RUSTSEC-2025-0052]: https://rustsec.org/advisories/RUSTSEC-2025-0052

```toml
[dependencies]
librtlsdr-rs = { version = "0.1", features = ["tokio"] }
```

## Public surface

The committed surface is intentionally narrow:

- `RtlSdrDevice` — the device handle. Open via `RtlSdrDevice::open` or
  `RtlSdrDeviceBuilder`.
- `RtlSdrReader` — streaming-focused handle (`device.reader()`).
- `list_devices` / `get_device_count` / `get_device_name` /
  `get_device_usb_strings` / `get_index_by_serial` — enumeration helpers.
- `RtlSdrError` — unified error type returned by every fallible operation.
- `TunerType` — IC family identifier.

## Linux permissions

On Linux you typically need a udev rule so the dongle is accessible
without root:

```text
# /etc/udev/rules.d/20-rtlsdr.rules
SUBSYSTEM=="usb", ATTRS{idVendor}=="0bda", ATTRS{idProduct}=="2838", MODE="0666"
SUBSYSTEM=="usb", ATTRS{idVendor}=="0bda", ATTRS{idProduct}=="2832", MODE="0666"
```

Reload with `sudo udevadm control --reload-rules && sudo udevadm trigger`,
then unplug + replug the dongle.

If the kernel's DVB driver auto-binds to the dongle, blacklist it:

```text
# /etc/modprobe.d/blacklist-dvb_usb_rtl28xxu.conf
blacklist dvb_usb_rtl28xxu
```

## Why a pure-Rust port?

- **No C build dependency.** Build cleanly on any rustc target that
  supports `rusb` — no `pkg-config`, no `apt install librtlsdr-dev`,
  no Windows DLL-shipping headaches.
- **Cross-platform USB.** `rusb` works on Linux, macOS, and Windows
  with the same API.
- **Rust-native API.** Owned types, real error enums, optional
  per-runtime async streaming.
- **Faithful behavior.** Register addresses, gain tables, and tuner
  initialisation sequences are transcribed directly from upstream
  librtlsdr — same hardware, same numbers, same expectations.

## Live hardware tests

A handful of integration tests exercise real USB I/O. They're
`#[ignore]` by default. Plug in a dongle and run the suite for
whichever async runtime you ship against:

```bash
# tokio Stream variant
cargo test --features tokio --test live_streaming -- --ignored

# smol Stream variant
cargo test --features smol  --test live_streaming_smol -- --ignored
```

Both files mirror the same three scenarios (smoke, parent-retunes-
during-stream, dropping-stream-stops-worker) so the runtime
backends stay at parity. Neither runs in CI (no hardware).

## License

GPL-2.0-or-later. The upstream librtlsdr C source is GPL-2.0-or-later;
since this crate is a faithful port (the [register tables and tuner
init sequences are transcribed directly][NOTICE]), it inherits the
same license. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

[NOTICE]: NOTICE
