//! Data streaming — buffer reset, sync read, async read.
//!
//! Ports `rtlsdr_reset_buffer`, `rtlsdr_read_sync`,
//! `rtlsdr_read_async`, `rtlsdr_cancel_async`.
//!
//! Note: The C implementation uses libusb's async transfer API with multiple
//! pre-submitted bulk transfers. The Rust implementation uses a blocking
//! read loop that checks a shared cancellation flag. True async support
//! will be added when the pipeline is wired up with worker threads.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::constants::{BULK_TIMEOUT, DEFAULT_BUF_LENGTH};
use crate::error::RtlSdrError;
use crate::reg::Block;
use crate::usb;

use super::RtlSdrDevice;
use super::reader::ReaderBusyGuard;

/// Callback type for async reading.
/// Called with a byte slice of IQ data for each completed bulk transfer.
pub type ReadAsyncCb = Box<dyn FnMut(&[u8]) + Send>;

/// Maximum allowed buffer length for async reads (16 MB).
const MAX_BUF_LENGTH: u32 = 16 * 1024 * 1024;

/// USB bulk transfer alignment requirement (bytes).
const BULK_ALIGNMENT: u32 = 512;

/// Async read loop timeout for cancel flag polling.
const ASYNC_POLL_TIMEOUT: Duration = Duration::from_secs(1);

/// Maximum consecutive `Ok(0)` reads tolerated by
/// [`RtlSdrDevice::read_async_blocking`] before the loop fuses.
///
/// A healthy RTL-SDR returns either `Ok(n > 0)` bytes or
/// `Err(Timeout)` — `Ok(0)` is a sentinel meaning the USB transfer
/// completed with zero payload (typically a ZLP from a
/// misbehaving / stalling device). The pre-#12 loop retried
/// `Ok(0)` indefinitely, matching the C upstream — but a
/// degenerate device producing ZLPs forever would lock the
/// callback consumer in a tight retry loop with no diagnostic.
///
/// 100 reads × the `ASYNC_POLL_TIMEOUT` upper bound (1 s) =
/// ~100 s worst-case before the loop fuses. Healthy devices
/// reset the counter on the first `Ok(n > 0)`. Per audit
/// issue #12 / "Reconcile Ok(0) semantics."
const MAX_CONSECUTIVE_ZERO_READS: u32 = 100;

/// Internal bulk-read helper shared by [`RtlSdrDevice::read_sync`]
/// and [`SampleIter::next`] (and, in `reader.rs`, also by
/// [`super::reader::RtlSdrReader::read_sync`] / `ReaderIter::next`).
/// Does NOT acquire the reader-busy guard — callers are responsible
/// for that contract; this function only does the actual USB
/// bulk-IN transfer + `NoDevice` translation.
///
/// `dev_lost` is set to `true` on the `NoDevice → DeviceLost`
/// translation path. The flag is shared with the parent
/// [`RtlSdrDevice`] so its [`Drop`] impl can skip cleanup against
/// a vanished handle (avoids a stream of cryptic
/// "register access failed" lines from cleanup writes that would
/// every return `NoDevice`). Per audit pass-2 #40.
pub(crate) fn bulk_read(
    handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    dev_lost: &AtomicBool,
    buf: &mut [u8],
) -> Result<usize, RtlSdrError> {
    // BULK_TIMEOUT = 0 selects the streaming-friendly 5 s default
    // (NOT libusb's "no timeout" convention) so drop-cancellation
    // is observable within at most one bulk-read cycle. See the
    // constant's docs for the full rationale. Per audit pass-2 #47.
    let timeout = if BULK_TIMEOUT == 0 {
        Duration::from_secs(5)
    } else {
        Duration::from_millis(BULK_TIMEOUT)
    };
    translate_bulk_result(
        handle.read_bulk(crate::constants::BULK_ENDPOINT, buf, timeout),
        dev_lost,
    )
}

/// Translate a rusb bulk-read result into the crate's typed shape,
/// side-effecting the `dev_lost` flag on disconnect.
///
/// Three rusb variants count as disconnect:
/// - [`rusb::Error::NoDevice`] — libusb's authoritative signal.
/// - [`rusb::Error::Pipe`] — endpoint stall; on Linux this is
///   the common mid-flight-disconnect surrogate before libusb
///   downgrades subsequent calls to `NoDevice`.
/// - [`rusb::Error::Io`] — generic transport I/O failure; same
///   Linux mid-flight surrogate.
///
/// All three set `dev_lost` AND normalize to [`RtlSdrError::DeviceLost`]
/// so the bulk-read surface and [`RtlSdrError::is_disconnected`]
/// agree (CodeRabbit on PR #80 caught the earlier asymmetry —
/// pre-fix, `Pipe`/`Io` looked disconnected to callers via
/// `is_disconnected()` but didn't trigger the `dev_lost` flag,
/// so `Drop` ran cleanup against a dead handle).
///
/// Pulled out of [`bulk_read`] so the disconnect-detection
/// behavior can be unit-tested without a real USB handle.
/// Per audit pass-2 #40 + #43.
fn translate_bulk_result(
    result: rusb::Result<usize>,
    dev_lost: &AtomicBool,
) -> Result<usize, RtlSdrError> {
    match result {
        Ok(n) => Ok(n),
        Err(rusb::Error::NoDevice | rusb::Error::Pipe | rusb::Error::Io) => {
            // `Release` pairs with the `Drop` impl's `Acquire`
            // load — any cleanup-skipping decision must observe
            // a flag set by a happens-before bulk-read failure.
            dev_lost.store(true, Ordering::Release);
            Err(RtlSdrError::DeviceLost)
        }
        Err(e) => Err(e.into()),
    }
}

impl RtlSdrDevice {
    /// Reset the USB endpoint buffer.
    ///
    /// Ports `rtlsdr_reset_buffer`.
    pub fn reset_buffer(&self) -> Result<(), RtlSdrError> {
        usb::write_reg(
            &self.handle,
            Block::Usb,
            crate::reg::usb_reg::USB_EPA_CTL,
            0x1002,
            2,
        )?;
        usb::write_reg(
            &self.handle,
            Block::Usb,
            crate::reg::usb_reg::USB_EPA_CTL,
            0x0000,
            2,
        )
    }

    /// Get a shared reference to the USB handle for spawning a reader thread.
    ///
    /// The returned Arc can be sent to another thread for concurrent bulk reads
    /// while the main thread retains access for control transfers.
    ///
    /// # Concurrency hazard
    ///
    /// This is the *escape hatch* around the single-active-stream
    /// guard that [`Self::read_sync`], [`Self::iter_samples`],
    /// [`Self::read_async_blocking`], and the
    /// [`super::RtlSdrReader`] streaming methods enforce via
    /// [`RtlSdrError::DeviceBusy`]. Doing your own
    /// `read_bulk(BULK_ENDPOINT, ...)` on this handle bypasses
    /// that guard entirely.
    ///
    /// libusb permits concurrent bulk submits on the same
    /// endpoint, but the responses interleave non-deterministically
    /// — each thread sees valid bytes for its own libusb transfer,
    /// but neither has the complete IQ stream. Only use this
    /// escape hatch if you serialize bulk reads yourself (one
    /// worker thread at a time on endpoint 0x81). Per #7.
    pub fn usb_handle(&self) -> std::sync::Arc<rusb::DeviceHandle<rusb::GlobalContext>> {
        std::sync::Arc::clone(&self.handle)
    }

    /// Synchronous (blocking) read of IQ samples.
    ///
    /// Ports `rtlsdr_read_sync`. Returns the number of bytes read.
    ///
    /// # Errors
    ///
    /// Returns [`RtlSdrError::DeviceBusy`] if another bulk-read
    /// activity (sync read, blocking iterator, async stream) is
    /// already in flight on this device. Per #7.
    pub fn read_sync(&self, buf: &mut [u8]) -> Result<usize, RtlSdrError> {
        let _guard = ReaderBusyGuard::try_acquire(std::sync::Arc::clone(&self.reader_busy))?;
        bulk_read(&self.handle, &self.dev_lost, buf)
    }

    /// Iterate IQ samples as a sequence of owned byte buffers.
    ///
    /// Returns an `Iterator` whose [`Iterator::next`] blocks the
    /// calling thread until one buffer's worth of samples is ready
    /// (a single `read_sync` underneath), then yields a freshly-
    /// allocated `Vec<u8>` of the actual byte count read. Each
    /// item is `Result<Vec<u8>, RtlSdrError>` so transport errors
    /// surface in-band; the iterator fuses (returns `None` from
    /// then on) after the first error or a zero-length read.
    ///
    /// This is the foundation for both sync streaming (use
    /// directly) and async streaming wrappers (the per-runtime
    /// `stream_samples_*` methods drive this iterator inside a
    /// blocking task).
    ///
    /// # Buffer size
    ///
    /// `buffer_size` is the bytes-per-yield target. The librtlsdr
    /// default is 256 KB (16 × 32 × 512). Smaller buffers give
    /// lower per-item latency but more allocator traffic; larger
    /// buffers amortise USB overhead but increase per-buffer
    /// latency. The size doesn't have to be a multiple of the USB
    /// 512-byte packet — `read_sync` returns the actual byte count
    /// — but multiples of 512 avoid short final transfers.
    ///
    /// Passing `0` selects the librtlsdr-equivalent default
    /// (256 KB) rather than requesting a zero-length buffer —
    /// matches the upstream "pass 0 for the default" ergonomic
    /// and prevents a typo from silently fusing the iterator on
    /// the first call (which would look like EOF).
    ///
    /// # Allocation
    ///
    /// Each yielded `Vec<u8>` is a fresh allocation. At the
    /// 256 KB / 65 ms cadence of typical RTL-SDR rates this is
    /// negligible (~15 allocs/sec). Smaller buffers scale
    /// linearly: a 4 KB buffer at 2 Msps is ~1000 allocs/sec
    /// (still acceptable on desktop), but at 512 bytes you're at
    /// ~7800 allocs/sec and an arena/pool starts to matter.
    /// For tight loops or embedded use prefer [`Self::read_sync`]
    /// directly with a reused caller-owned buffer. Per #20.
    ///
    /// ```no_run
    /// # use librtlsdr_rs::{RtlSdrDevice, RtlSdrError};
    /// # fn main() -> Result<(), RtlSdrError> {
    /// let dev = RtlSdrDevice::open(0)?;
    /// dev.reset_buffer()?;
    /// // Take the first 10 buffers — each ~65 ms at 2 Msps.
    /// for chunk in dev.iter_samples(262_144).take(10) {
    ///     let bytes = chunk?;
    ///     // process `bytes`...
    ///     # let _ = bytes;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn iter_samples(&self, buffer_size: usize) -> SampleIter<'_> {
        // Normalise zero to the librtlsdr-equivalent default
        // (256 KB). A `buffer_size == 0` typo would otherwise
        // hand `read_sync` an empty slice, which the USB
        // backend treats as an immediate zero-length read —
        // the iterator's zero-fuse path triggers, and the
        // caller sees an empty `for chunk in iter { … }` that
        // looks like EOF rather than a configuration mistake.
        // Per #632 CR round 1.
        let buffer_size = if buffer_size == 0 {
            DEFAULT_BUF_LENGTH as usize
        } else {
            buffer_size
        };
        // Acquire the reader-busy guard for the iterator's lifetime.
        // On contention, store the error in `pending_error` and yield
        // it on first `next()` (then fuse) — matches the existing
        // fuse-on-error contract documented on `SampleIter`. Per #7.
        let (guard, pending_error) =
            match ReaderBusyGuard::try_acquire(std::sync::Arc::clone(&self.reader_busy)) {
                Ok(g) => (Some(g), None),
                Err(e) => (None, Some(e)),
            };
        SampleIter {
            device: Some(self),
            buffer_size,
            _guard: guard,
            pending_error,
        }
    }

    /// Read IQ samples in a blocking loop, calling the callback for each buffer.
    ///
    /// This is a simplified port of `rtlsdr_read_async`. It blocks the calling
    /// thread and reads bulk data, calling `cb` for each completed buffer.
    /// Use `cancel_flag` to signal cancellation from another thread.
    ///
    /// - `cb`: callback called with each buffer of IQ data
    /// - `cancel_flag`: set to `true` from another thread to stop reading
    /// - `buf_len`: buffer length in bytes (0 = default, must be multiple of 512)
    ///
    /// # Termination
    ///
    /// Returns when any of the following:
    /// - `cancel_flag` becomes `true` (caller-initiated; returns `Ok(())`)
    /// - The underlying USB read returns `NoDevice` (returns
    ///   `Err(DeviceLost)`) or any other transport error
    /// - 100 consecutive `Ok(0)` (zero-length) reads have been
    ///   observed (returns `Ok(())` with a `tracing::warn!`). A
    ///   healthy device returns either `Ok(n > 0)` or
    ///   `Err(Timeout)`; sustained `Ok(0)` indicates a degenerate
    ///   device that the C upstream would loop on forever.
    ///   Brings this path into rough parity with `iter_samples`'s
    ///   defensive `Ok(0)` fuse. Per audit issue #12.
    ///
    /// # Cancellation latency
    ///
    /// The cancel flag is checked between bulk reads. Each bulk
    /// read uses a 1-second timeout (the polling cadence), so
    /// worst-case observation latency from
    /// `cancel_flag.store(true, …)` to the function returning is
    /// ~1 second on an idle device, plus up to one bulk-read time
    /// (~65 ms typical at 2 Msps) on an actively-streaming device.
    /// True in-flight cancellation needs libusb's async-submit +
    /// cancel API and is tracked as #633. Per audit #20.
    pub fn read_async_blocking(
        &self,
        mut cb: ReadAsyncCb,
        cancel_flag: &AtomicBool,
        buf_len: u32,
    ) -> Result<(), RtlSdrError> {
        // Acquire the reader-busy flag for the entire callback loop.
        // Released on Drop when this function returns. Per #7.
        let _guard = ReaderBusyGuard::try_acquire(std::sync::Arc::clone(&self.reader_busy))?;

        let actual_buf_len = if buf_len == 0 {
            DEFAULT_BUF_LENGTH as usize
        } else if !buf_len.is_multiple_of(BULK_ALIGNMENT) || buf_len > MAX_BUF_LENGTH {
            return Err(RtlSdrError::InvalidParameter(format!(
                "buf_len must be a multiple of {BULK_ALIGNMENT} and <= {MAX_BUF_LENGTH}, got {buf_len}"
            )));
        } else {
            buf_len as usize
        };

        let timeout = ASYNC_POLL_TIMEOUT;
        let mut buf = vec![0u8; actual_buf_len];
        let mut consecutive_zero_reads: u32 = 0;

        // Relaxed ordering is sufficient: there's no other state
        // being synchronized through this flag — the worst-case
        // visibility latency is one extra bulk-read iteration
        // (one ASYNC_POLL_TIMEOUT). Don't "upgrade" to SeqCst on
        // a hot loop without a concrete invariant requiring it.
        // Per audit issue #20.
        while !cancel_flag.load(Ordering::Relaxed) {
            match self
                .handle
                .read_bulk(crate::constants::BULK_ENDPOINT, &mut buf, timeout)
            {
                Ok(n) if n > 0 => {
                    consecutive_zero_reads = 0;
                    cb(&buf[..n]);
                }
                Ok(_) => {
                    // Zero-length read. A healthy device shouldn't
                    // produce these; a degenerate device producing
                    // ZLPs forever would lock the consumer in a
                    // tight retry loop. Fuse after a documented
                    // bound. Per audit issue #12.
                    consecutive_zero_reads += 1;
                    if consecutive_zero_reads >= MAX_CONSECUTIVE_ZERO_READS {
                        tracing::warn!(
                            "read_async_blocking: {MAX_CONSECUTIVE_ZERO_READS} consecutive \
                             zero-length reads — fusing the loop (degenerate device?)"
                        );
                        return Ok(());
                    }
                }
                Err(rusb::Error::Timeout) => {
                    // Timeout doesn't reset the counter (it carries
                    // no signal about whether the device is producing
                    // ZLPs) but doesn't increment it either —
                    // distinct from Ok(0).
                }
                Err(rusb::Error::NoDevice | rusb::Error::Pipe | rusb::Error::Io) => {
                    // Mirror `bulk_read`'s `dev_lost` side effect
                    // so `Drop` can skip cleanup against a
                    // vanished handle. Treats Linux hot-unplug
                    // surrogates (`Pipe`/`Io`) same as
                    // `NoDevice` — see `translate_bulk_result`
                    // doc for why. Per audit pass-2 #40 + #43.
                    self.dev_lost.store(true, Ordering::Release);
                    return Err(RtlSdrError::DeviceLost);
                }
                Err(e) => {
                    tracing::error!("bulk read error: {e}");
                    return Err(RtlSdrError::Usb(e));
                }
            }
        }

        Ok(())
    }
}

/// Blocking iterator over IQ-sample buffers, returned by
/// [`RtlSdrDevice::iter_samples`].
///
/// Each [`Iterator::next`] call performs one [`RtlSdrDevice::read_sync`]
/// into a freshly-allocated `Vec<u8>` and yields it. The iterator
/// fuses on the first error or zero-length read — once `next`
/// returns `Some(Err(_))` (or `None` from a zero read), all
/// subsequent calls return `None` so callers can use the standard
/// `for chunk in iter { let chunk = chunk?; ... }` shape without
/// worrying about post-error state.
pub struct SampleIter<'a> {
    /// `None` once the iterator has fused (error or zero read).
    /// Borrows the device shared (`&`) because [`RtlSdrDevice::read_sync`]
    /// is `&self` — the underlying USB bulk transfer doesn't need
    /// mutable access.
    device: Option<&'a RtlSdrDevice>,
    buffer_size: usize,
    /// Reader-busy guard held for the iterator's lifetime. Acquired
    /// at construction (`iter_samples`); released on Drop. `None` if
    /// construction failed to acquire (in which case
    /// `pending_error` carries the `DeviceBusy` to yield on first
    /// `next()`) — also `None` after the iterator drops itself.
    /// Per #7.
    _guard: Option<ReaderBusyGuard>,
    /// Construction-time guard-acquire failure to yield on the next
    /// (= first) `next()` call. Cleared after yielding; the
    /// iterator fuses normally afterward via `device = None`.
    pending_error: Option<RtlSdrError>,
}

impl Iterator for SampleIter<'_> {
    type Item = Result<Vec<u8>, RtlSdrError>;

    fn next(&mut self) -> Option<Self::Item> {
        // Yield any deferred construction error first, then fuse.
        if let Some(e) = self.pending_error.take() {
            self.device = None;
            return Some(Err(e));
        }
        let device = self.device?;
        let mut buf = vec![0u8; self.buffer_size];
        // Bypass `device.read_sync` (which would re-acquire its own
        // guard per call) — the iterator already holds the guard
        // for its lifetime via `_guard`. Per #7.
        match bulk_read(&device.handle, &device.dev_lost, &mut buf) {
            Ok(0) => {
                // Zero-length read — treat as end-of-stream so
                // callers using `.take(N)` / `for ... in iter`
                // don't spin forever on a degenerate device.
                self.device = None;
                None
            }
            Ok(n) => {
                buf.truncate(n);
                Some(Ok(buf))
            }
            Err(e) => {
                // Fuse after first error so subsequent calls
                // return `None` rather than re-yielding the
                // same error indefinitely.
                self.device = None;
                Some(Err(e))
            }
        }
    }
}

impl std::iter::FusedIterator for SampleIter<'_> {}

#[cfg(test)]
mod tests {
    use super::*;

    // Pin the trait-impl contract documented on `SampleIter` —
    // standard `Iterator` + `FusedIterator` so consumers can rely
    // on `for x in iter` shape AND on the post-fuse-returns-None
    // contract without empirical testing. If a refactor ever
    // changes the iterator shape, this fires at compile time.
    const _: fn() = || {
        fn assert_iter<T: Iterator>() {}
        fn assert_fused<T: std::iter::FusedIterator>() {}
        assert_iter::<SampleIter<'_>>();
        assert_fused::<SampleIter<'_>>();
    };

    // Pin `SampleIter: !Send` — the borrowed iterator is the
    // single-thread surface (the owned `ReaderIter` is the
    // sendable one). `SampleIter<'a>` borrows `&'a RtlSdrDevice`
    // and `RtlSdrDevice: !Sync`, so `&RtlSdrDevice: !Send`,
    // making `SampleIter: !Send` transitively. If a future field
    // change ever made `RtlSdrDevice: Sync`, `SampleIter` would
    // silently become Sendable — and a downstream consumer might
    // inadvertently move it across threads, violating the
    // documented "single-threaded sync iteration" contract.
    // Per audit issue #20.
    static_assertions::assert_not_impl_any!(SampleIter<'static>: Send);

    /// Per audit pass-2 #40: the `NoDevice → DeviceLost`
    /// translation must side-effect the shared `dev_lost` flag
    /// so the parent device's `Drop` can skip cleanup against a
    /// vanished handle.
    #[test]
    fn translate_no_device_sets_dev_lost_flag() {
        let flag = AtomicBool::new(false);
        let result = translate_bulk_result(Err(rusb::Error::NoDevice), &flag);
        assert!(matches!(result, Err(RtlSdrError::DeviceLost)));
        assert!(flag.load(Ordering::Acquire), "dev_lost should be set");
    }

    #[test]
    fn translate_ok_does_not_touch_dev_lost_flag() {
        let flag = AtomicBool::new(false);
        let result = translate_bulk_result(Ok(42), &flag);
        assert!(matches!(result, Ok(42)));
        assert!(!flag.load(Ordering::Acquire));
    }

    /// Per CodeRabbit on PR #80: `Pipe` and `Io` are the Linux
    /// hot-unplug surrogates surfaced before libusb downgrades
    /// to `NoDevice`. They must trigger the same side effect
    /// (set `dev_lost`) and normalize to the same error
    /// (`DeviceLost`) as `NoDevice` itself, otherwise the
    /// bulk-read surface and `is_disconnected()` disagree and
    /// `Drop` runs cleanup against a dead handle.
    #[test]
    fn translate_pipe_and_io_treated_as_disconnect() {
        for kind in [rusb::Error::Pipe, rusb::Error::Io] {
            let flag = AtomicBool::new(false);
            let result = translate_bulk_result(Err(kind), &flag);
            assert!(
                matches!(result, Err(RtlSdrError::DeviceLost)),
                "{kind:?} should normalize to DeviceLost"
            );
            assert!(
                flag.load(Ordering::Acquire),
                "dev_lost should be set for {kind:?}"
            );
        }
    }

    /// Pin that genuinely-transient transport errors do NOT
    /// trip the disconnect flag. `Timeout` is the most important
    /// — a slow stream is healthy, not lost.
    #[test]
    fn translate_other_errors_do_not_touch_dev_lost_flag() {
        for kind in [rusb::Error::Timeout, rusb::Error::Overflow] {
            let flag = AtomicBool::new(false);
            let _ = translate_bulk_result(Err(kind), &flag);
            assert!(
                !flag.load(Ordering::Acquire),
                "dev_lost should not fire for {kind:?}"
            );
        }
    }

    /// Pin idempotence: a second `NoDevice` after the flag is
    /// already set must be a no-op (still sets, still returns
    /// `DeviceLost`, no panic). Real bulk-read paths might
    /// retry once after the flag is set.
    #[test]
    fn translate_no_device_is_idempotent() {
        let flag = AtomicBool::new(true);
        let result = translate_bulk_result(Err(rusb::Error::NoDevice), &flag);
        assert!(matches!(result, Err(RtlSdrError::DeviceLost)));
        assert!(flag.load(Ordering::Acquire));
    }
}
