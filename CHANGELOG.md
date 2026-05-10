# Changelog

All notable changes to `librtlsdr-rs` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.1] - 2026-05-10

Audit-driven correctness pass. Seven issues from the May 2026
codebase audit (Tier 1 + Tier 2): four verified critical bugs and
three correctness-trap closures. No public API additions or
removals — strict patch release. The previously-dead
`RtlSdrError::DeviceBusy` variant is now constructible (see
[#7]), so consumers may want to add a match arm if exhaustively
matching `RtlSdrError`.

### Fixed

- **R82XX `set_mux` and `set_bw` range-search loops returned
  the wrong table entry at the upper edge** ([#5]). The Rust port
  of `for (i = 0; i < N-1; i++) { if (...) break; }` ended with
  `range_idx = N-2` after natural completion (where the C left
  `i = N-1`), silently picking the wrong `FREQ_RANGES` /
  `IF_LOW_PASS_BW_TABLE` entry above 650 MHz / below 350 kHz BW.
  Replaced both loops with `partition_point` shape and added 11
  edge-case unit tests on extracted helpers.
- **`get_index_by_serial` returned `DeviceNotFound(0)` when no
  devices were present** ([#6]) — lying about both the failure
  mode (it's a serial mismatch, not a missing index) and the
  input. Now returns `InvalidParameter("no device with serial
  '...'")`. Extracted matching algorithm into a `pub(crate)`
  helper with four unit tests.
- **Concurrent bulk reads on USB endpoint 0x81 silently split the
  IQ stream between callers** ([#7]). `RtlSdrReader: Clone +
  read_sync(&self)` allowed two threads to issue concurrent
  bulk submits — libusb permits the submits, but the responses
  interleave non-deterministically, so each thread sees valid
  bytes for its own libusb transfer with neither holding the
  complete signal. Added an `Arc<AtomicBool>` reader-busy flag
  acquired by an RAII `ReaderBusyGuard` at every bulk-read entry
  point (`RtlSdrDevice::{read_sync, iter_samples,
  read_async_blocking}` + `RtlSdrReader::{read_sync, iter_samples,
  stream_samples_tokio, stream_samples_smol}`). Concurrent
  attempts now return `RtlSdrError::DeviceBusy` (the variant was
  declared in 0.1.0 but had no producer until this fix).
  `RtlSdrDevice::usb_handle()` remains the documented escape
  hatch with an updated concurrency-hazard note.
- **`set_tuner_bandwidth` updated `self.bw` even when
  `tuner.set_bw` failed** ([#9]), so cached state could lie about
  a setting the hardware never accepted. Now propagates the
  primary error and only updates `self.bw` on success.
- **`set_sample_rate`'s inner retune did not reset `self.freq`
  on failure** ([#9]), asymmetric with `set_center_freq`'s
  audit-fix-#11 pattern. Now resets to 0 to match (cached freq
  no longer claims a value the tuner isn't on after a retune
  failure). Same parity fix applied to `set_tuner_bandwidth`.
- **`freq < offs_freq` had three inconsistent shapes across
  `frequency.rs`** ([#10]): `wrapping_sub` (silent wrap) in
  `set_center_freq`, panic-shape `freq - offs_freq` in
  `set_sample_rate` and `set_tuner_bandwidth`. Now all three use
  a `freq_minus_offset` helper returning
  `RtlSdrError::InvalidParameter` with both values named.
- **`set_offset_tuning(true)` had a partial-state hazard** when
  the current freq was at or below the computed floor: the IF
  registers were silently written but the tuner stayed on the
  old freq ([#10]). Now pre-validated before any state mutation;
  rejected calls are true no-ops.
- **`set_sample_rate` had a partial-apply path** when offset
  tuning was active: the trailing `set_offset_tuning(true)?`
  could fail after `self.rate` and several device registers had
  already been updated ([#10] round 2, Code Rabbit). Now
  preflighted at the top of `set_sample_rate`.
- **R82XX `set_pll` returned `Ok(())` on PLL-lock failure** with
  `self.has_lock = false`, requiring callers to remember to
  check the field — a footgun matching no other tuner ([#11]).
  Now returns `Err(RtlSdrError::Tuner("PLL not locked for X
  Hz"))` matching the E4K backend's shape. The vestigial
  `has_lock` field is removed.
- **R82XX `set_pll` could divide by zero** if `xtal` was ever
  set to 0 (e.g., a PPM-correction overflow) ([#11]). Now
  guards at function entry with a typed `Tuner` error.

### Added

- `tracing::warn!` on all swallowed best-effort errors in
  `frequency.rs` (`set_sample_rate`, `set_tuner_bandwidth`,
  `set_offset_tuning`); `tracing::debug!` on `RtlSdrDevice::drop`'s
  cleanup paths ([#9]).
- `freq_minus_offset` and `offset_tuning_floor` pure helpers in
  `frequency.rs` with seven unit tests pinning the math ([#10]).
- `ReaderBusyGuard` RAII type (`pub(crate)`) in `device::reader`
  with two unit tests ([#7]).
- `lookup_serial`, `find_freq_range_idx`, `find_if_lpf_idx`
  internal helpers extracted for testability ([#6], [#5]).
- `tests/live_streaming.rs::second_reader_returns_device_busy`
  ignored hardware test ([#7]).
- A `pub(crate) bulk_read(handle, buf)` helper in
  `device::streaming` deduplicating the USB bulk-IN +
  `NoDevice → DeviceLost` translation across the device and
  reader paths (incidental win addressing part of audit #12).

### Documentation

- Documented the byte-order asymmetry quirk in
  `usb::read_reg` / `usb::demod_read_reg` (faithful to C
  upstream; latent because no in-tree caller uses `len == 2`)
  and dropped the `len: u8` parameter so the latent bug cannot
  fire ([#8]).
- Documented the offset-tuning floor (≈ 0.85 × sample_rate) and
  full `# Errors` enumeration on `set_offset_tuning` ([#10]).
- Documented the `# Errors` shape and best-effort behavior of
  `set_tuner_bandwidth` ([#9]).
- Documented the single-active-streaming-session invariant and
  `usb_handle()` escape-hatch hazard on `RtlSdrReader` ([#7]).

[#5]: https://github.com/jasonherald/librtlsdr-rs/issues/5
[#6]: https://github.com/jasonherald/librtlsdr-rs/issues/6
[#7]: https://github.com/jasonherald/librtlsdr-rs/issues/7
[#8]: https://github.com/jasonherald/librtlsdr-rs/issues/8
[#9]: https://github.com/jasonherald/librtlsdr-rs/issues/9
[#10]: https://github.com/jasonherald/librtlsdr-rs/issues/10
[#11]: https://github.com/jasonherald/librtlsdr-rs/issues/11

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
