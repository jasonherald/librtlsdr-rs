# Changelog

All notable changes to `librtlsdr-rs` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-05-06

Initial release. Carved out of the in-tree `sdr-rtlsdr` crate from the
[`rtl-sdr` SDR application][rtl-sdr] after iterating its public surface
through six rounds of review.

### Added

- `RtlSdrDevice` — device handle. Opens via `RtlSdrDevice::open(index)`
  or the more ergonomic `RtlSdrDeviceBuilder`.
- Five tuner backends: R820T / R820T2 / R828D, E4000, FC0012, FC0013,
  FC2580. All transcribed faithfully from upstream librtlsdr.
- `RtlSdrReader` — streaming-focused handle returned by
  `RtlSdrDevice::reader()`. Internally clones the device's
  `Arc<rusb::DeviceHandle>`; the parent retains control while the
  reader streams samples on its own thread or async task.
- Sync iterator: `RtlSdrReader::iter_samples(buffer_size)` returns a
  `Send + 'static` iterator usable across thread boundaries.
- Async streams (per-runtime, opt-in via cargo feature):
  - `tokio` → `RtlSdrReader::stream_samples_tokio`
  - `smol` → `RtlSdrReader::stream_samples_smol`

  An `async-std` backend was prototyped and dropped before publication —
  async-std itself was marked unmaintained (RUSTSEC-2025-0052) and its
  upstream recommends migrating to smol, which we ship.
- Enumeration helpers: `list_devices` (returns `Vec<DeviceInfo>`),
  plus the upstream-compatible `get_device_count`, `get_device_name`,
  `get_device_usb_strings`, `get_index_by_serial`.
- `RtlSdrError` — unified `thiserror`-derived error type with
  `DeviceLost`, `Usb`, `InvalidParameter`, and per-tuner failure
  variants.
- `TunerType` — IC family identifier returned by
  `RtlSdrDevice::tuner_type()`.
- Live hardware integration tests (`tests/live_streaming.rs`) — gated
  behind `#[ignore]`; run with `cargo test --features tokio --test
  live_streaming -- --ignored`.

[rtl-sdr]: https://github.com/jasonherald/rtl-sdr
