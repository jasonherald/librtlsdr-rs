# Changelog

All notable changes to `librtlsdr-rs` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-05-10

First semver-major release. Closes the deferred error-type bundle
([#16]) — the only audit finding intentionally held back from the
0.1.x patch wave because it required breaking changes. Two
companion enums are now `#[non_exhaustive]` so future variant
additions stay non-breaking.

### Migration guide

**1. `RtlSdrError::Tuner` now carries `TunerError`, not `String`.**

```rust
// 0.1.x
match err {
    RtlSdrError::Tuner(msg) if msg.contains("PLL not locked") => retry(),
    RtlSdrError::Tuner(msg) => log::warn!("tuner: {msg}"),
    _ => bail!(err),
}

// 0.2
use librtlsdr_rs::{RtlSdrError, TunerError};
match err {
    RtlSdrError::Tuner(TunerError::PllNotLocked { freq_hz }) => retry(),
    RtlSdrError::Tuner(inner) => log::warn!("tuner: {inner}"),
    _ => bail!(err),
}
```

**2. `RtlSdrError` and `TunerError` are `#[non_exhaustive]`.** Add
a catch-all arm to any exhaustive match. Future variant additions
ship as patch releases.

```rust
match err {
    RtlSdrError::Usb(_) => ...,
    RtlSdrError::DeviceLost => ...,
    _ => ...,                     // required, even if all variants are listed
}
```

**3. `DeviceNotFound`, `InvalidSampleRate`, `RegisterAccess` are
struct variants now.**

```rust
// 0.1.x
RtlSdrError::DeviceNotFound(idx)
RtlSdrError::InvalidSampleRate(rate)
RtlSdrError::RegisterAccess

// 0.2
RtlSdrError::DeviceNotFound { index: idx }
RtlSdrError::InvalidSampleRate { rate_hz: rate }
RtlSdrError::RegisterAccess { block, address }   // now also names the failing register
```

**4. `closest_gain` returns `Option<i32>`; `try_closest_gain` is
removed.**

```rust
// 0.1.x — `0` was ambiguous between "no gain table" and "0 was the closest step"
dev.set_tuner_gain(dev.closest_gain(150))?;

// 0.2
if let Some(g) = dev.closest_gain(150) {
    dev.set_tuner_gain(g)?;
}
```

### Changed (breaking)

- **`RtlSdrError` is `#[non_exhaustive]`** ([#16]). Adds a
  required catch-all arm to consumer matches; in exchange, future
  additions ship as patch releases. Same treatment for the new
  `TunerError`.
- **`RtlSdrError::Tuner(String)` → `Tuner(TunerError)`** ([#16]).
  New `TunerError` enum (`PllNotLocked { freq_hz }`,
  `XtalIsZero`, `PllProgrammingFailed { backend, freq_hz, reason }`,
  `I2cTransferFailed { operation, got, expected }`,
  `ShadowCacheMiss { reg }`, `UnsupportedFilterBandwidth { mode }`,
  `InvalidGain { what, detail }`, `Context { context, source }`)
  lets consumers programmatically discriminate tuner failures
  without parsing message strings. `#[from]` keeps `?` ergonomic
  inside the crate.
- **`RtlSdrError::DeviceNotFound(u32)` → `DeviceNotFound { index }`**
  ([#16]) — struct variant for forward-compatibility with
  diagnostic context fields.
- **`RtlSdrError::InvalidSampleRate(u32)` → `InvalidSampleRate { rate_hz }`**
  ([#16]) — struct variant.
- **`RtlSdrError::RegisterAccess` (no payload) → `RegisterAccess { block, address }`**
  ([#16]) — names the failing register block + address. The
  `Block` enum (`Demod`, `Iic`, `Sys`, …) is re-exported at the
  crate root.
- **`RtlSdrDevice::closest_gain` returns `Option<i32>`** ([#16]).
  Removes `try_closest_gain`; the two-method 0.1.x stopgap from
  audit #15 collapses into one.

[#16]: https://github.com/jasonherald/librtlsdr-rs/issues/16

## [0.1.2] - 2026-05-10

Second wave of audit follow-up — closes Tier 3 through Tier 6
of the May 2026 codebase audit (#12 through #21) plus a new UX
issue (#31) discovered during the live-test runs. Strict patch
release: pure additive surface, no public API removed or
reshaped. **15 of 17 audit issues now closed; only #16
(deferred semver-major error-type bundle) remains by design.**

### Added

- **`RtlSdrError` is now `Clone + PartialEq + Eq`** ([#15]) —
  consumers can stash the last error in
  `Arc<Mutex<Option<RtlSdrError>>>`, snapshot it across UI
  re-render cycles, or assert equality in tests without
  resorting to `format!("{e}")` substring matching. Verified
  against `rusb::Error` 0.9.4 (already `Copy + Clone + Eq +
  PartialEq`).
- **`RtlSdrError::is_disconnected()` / `is_timeout()`** helper
  methods ([#15]) — common SDR retry/reconnect pattern shouldn't
  require pulling `rusb` into the consumer's `Cargo.toml` just
  to pattern-match transport variants.
- **`pub use rusb`** at the crate root ([#15]) — consumers
  pattern-matching on less-common `rusb::Error` variants (`Io`,
  `Pipe`, `Overflow`, etc.) can now `use librtlsdr_rs::rusb;`
  without risking a Cargo resolver dep-version mismatch.
- **`RtlSdrDevice::try_closest_gain`** returning `Option<i32>`
  ([#15]) — disambiguates "no gain table available" from "the
  closest step happens to be 0" that `closest_gain` overloads.
- **Manual `Debug` impl for `RtlSdrDevice`** ([#19]) — consumers
  can `dbg!(&device)` or include the device in
  `#[derive(Debug)]` parent structs. Skips the non-Debug
  `handle` (substituted with `Arc::as_ptr`) and `tuner` fields
  (substituted with `tuner_present: bool` alongside the existing
  `tuner_type`).
- **`#[must_use]` on 12 public pure-getter methods** ([#19]):
  `tuner_type`, `tuner_gains`, `manufacturer`, `product`,
  `serial`, `center_freq`, `sample_rate`, `freq_correction`,
  `tuner_gain`, `direct_sampling`, `offset_tuning`, `xtal_freq`.
- **`tuner_gains` returns `&'static [i32]`** ([#19]) instead of
  borrowing self — strictly more permissive; callers can stash
  the slice across the device's lifetime.
- **Three new live-hardware tokio tests** ([#21]):
  `tokio_stream_drains_30_seconds` (sustained-throughput
  smoke), `tokio_stream_drop_while_blocking_send` (drop while
  worker is parked in `blocking_send` because the channel is
  full), and a strengthened
  `dropping_stream_stops_worker` (now drops device + re-opens
  with bounded retry to verify the worker fully released the
  USB interface claim).
- **Three new live-hardware smol tests** ([#13]) — mirror of the
  three primary tokio Stream scenarios (smoke, parent-retunes-
  during-stream, dropping-stream-stops-worker). `smol = "2"`
  added to `[dev-dependencies]` for `smol::block_on` /
  `smol::Timer`. Production `smol` feature unchanged.
- **`MAX_CONSECUTIVE_ZERO_READS` fuse** in
  `RtlSdrDevice::read_async_blocking` ([#12]) — after 100
  consecutive `Ok(0)` reads (~100 s at the 1 s
  `ASYNC_POLL_TIMEOUT`), log a `tracing::warn!` and return
  `Ok(())` cleanly. Brings the callback path's "stuck device"
  semantics into rough parity with `iter_samples`'s defensive
  `Ok(0)` fuse (was inherited from upstream C's spin-forever).
- **CI matrix: macOS + Windows + MSRV jobs** ([#17]) —
  `cross-platform` matrix job builds + lib-tests + doctests on
  `macos-latest` (`brew install libusb pkg-config`) and
  `windows-latest` (`vcpkg install libusb:x64-windows-static-md`,
  `VCPKGRS_DYNAMIC=0`). `msrv` job pins `dtolnay/rust-toolchain@1.95.0`
  so a 1.96-only feature can't silently land green and break
  consumers on the declared MSRV. README's cross-platform
  claim now actually validated.
- **Static-assertion tightening** ([#20]) — `static_assertions`
  added as a dev-dep. `assert_not_impl_any!(SampleIter<'static>:
  Send)` pins the !Send contract of the borrowed iterator;
  `<TokioSampleStream as Stream>::Item: Send` and same for Smol
  pin a future non-Send `RtlSdrError` variant addition (#16) as
  a compile-time failure rather than consumer-code surprise.

### Fixed

- **R82xx `sysfreq_sel` predetect dead-flag** ([#14]) — the
  `use_predetect` config flag's conditional set is followed
  unconditionally by the digital-TV "PRE_DECT off" clear, so
  the flag has no observable effect today (verified faithful to
  C upstream `tuner_r82xx.c`). Documented with a comment naming
  the dead flag and the gate that would have to be removed for
  it to actually matter.
- **FC0012 / FC0013 PLL `pm` underflow** ([#14]) — the
  `if am < 2 { pm -= 1 }` block could underflow `pm` to 65535
  on pathological `xdiv` (debug panic, release wrap matching C's
  `uint8_t` wrap which then silently passes downstream
  validation). Now returns `RtlSdrError::Tuner` with the bad
  `xdiv` and freq named. Backends are latent (not wired up in
  `probe_tuner`); fix lands before any future wire-up.
- **E4K `set_lna_gain` exact-match fragility** ([#14]) — was
  `-EINVAL` on no exact match against `LNA_GAIN`; now snaps to
  nearest like `closest_arr_idx` does for filters. Deliberate
  divergence from C upstream; latent because E4K isn't wired
  up.
- **`STREAM_BACKPRESSURE_DEPTH` was duplicated** in
  `streaming_tokio.rs` and `streaming_smol.rs` ([#12]) — hoisted
  to a single `pub(crate) const` in `constants.rs`. The
  back-pressure-math comment was also rewritten (the original
  chained "4 × 256 KB ≈ 1 MB ≈ 250 ms ... = 4 MB/s"
  resolved to 1 s, not 250 ms).

### Documentation

- **Tier 5 faithful-port foot-gun docs** ([#18]) — five sites
  the Rust port faithfully copies from C upstream that aren't
  obvious to a Rust reader: `set_testmode` / `set_agc_mode`
  shared-register interaction (sequence
  `set_agc_mode(true) → set_testmode(true) → set_testmode(false)`
  silently turns AGC off), `set_xtal_freq` `0`-sentinel,
  `init_baseband` redundant 0x16/0x17 clear, `tracing::warn!`
  on `enumerate.rs::get_device_count` /
  `get_device_name` failures (was silently treated as "no
  devices"), `tracing::info!` on one-shot state-change methods
  (`set_freq_correction`, `set_tuner_gain_mode`,
  `set_offset_tuning`, `set_agc_mode`, `set_testmode`,
  `set_bias_tee_gpio`, `set_xtal_freq`).
- **Tuner trait clarification** ([#14]) — `Tuner::set_bw`
  return-value convention documented (only R82xx returns a
  meaningful IF; `0` from non-R82xx means "no IF change
  required"). `RtlSdrDevice` doc said `Box<dyn Tuner + Send>`;
  type is `Box<dyn Tuner>` (the `+ Send` is implicit via the
  trait's `Send` supertrait).
- **README `--test-threads=1` requirement for live tests**
  ([#31], discovered during PR #30's hardware run) — `cargo
  test`'s default parallel runner has multiple threads each call
  `RtlSdrDevice::open(0)` and only one wins; the rest silently
  skip via "Resource busy." Suite reports `5 passed` with only
  1 actually exercising hardware. README updated; `open_or_skip`
  prints a louder diagnostic naming `--test-threads=1` when it
  detects `rusb::Error::Busy`.
- **Streaming layer docs** ([#20]) — `iter_samples` allocation
  cost note now mentions small-buffer scaling (~7800 allocs/sec
  at 512 B vs ~15 allocs/sec at 256 KB); new
  `# Cancellation latency` section on `read_async_blocking`;
  inline rationale for `Ordering::Relaxed` on the cancel-flag
  load. `usb_handle()` concurrency-hazard note was already
  added in 0.1.1's #7 work.

### Internal

- **CI: dropped duplicate `cargo-deny` job** ([#17]) — the inline
  job in `ci.yml` was running identically to `deny.yml`'s.
- **CI: dropped unneeded `apt install libusb` in `audit.yml`**
  ([#17]) — `cargo audit` only reads `Cargo.lock`.
- **CI: pinned `cargo-deny`'s `command: check all`** ([#17]) so
  a future upstream default change can't silently shrink
  supply-chain coverage.
- **Streaming layer dedup** ([#12]) — single `pub(crate) fn
  bulk_read(handle, buf)` was already extracted in 0.1.1's #7
  work (busy-flag PR); this release just hoists the
  `STREAM_BACKPRESSURE_DEPTH` constant alongside.

[#12]: https://github.com/jasonherald/librtlsdr-rs/issues/12
[#13]: https://github.com/jasonherald/librtlsdr-rs/issues/13
[#14]: https://github.com/jasonherald/librtlsdr-rs/issues/14
[#15]: https://github.com/jasonherald/librtlsdr-rs/issues/15
[#17]: https://github.com/jasonherald/librtlsdr-rs/issues/17
[#18]: https://github.com/jasonherald/librtlsdr-rs/issues/18
[#19]: https://github.com/jasonherald/librtlsdr-rs/issues/19
[#20]: https://github.com/jasonherald/librtlsdr-rs/issues/20
[#21]: https://github.com/jasonherald/librtlsdr-rs/issues/21
[#31]: https://github.com/jasonherald/librtlsdr-rs/issues/31

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
