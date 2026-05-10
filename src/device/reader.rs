//! Streaming-focused handle that runs concurrently with control.
//!
//! See [`RtlSdrReader`] and [`RtlSdrDevice::reader`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::RtlSdrError;

// Brought into scope only so this file's intra-doc links
// (`[`RtlSdrDevice::reader`]`, etc.) resolve under
// `cargo doc -D warnings`. The type is not referenced by name in
// any module-level code path here — clippy's `unused_imports` lint
// flags this without the explicit allow. Per Code Rabbit on #23.
#[allow(unused_imports)]
use super::RtlSdrDevice;

/// RAII guard for the per-device reader-busy flag. Acquiring sets
/// the flag to `true` via `compare_exchange`; dropping clears it.
///
/// Used to ensure at most one bulk-read activity (sync read,
/// blocking iterator, async stream) is in flight on USB endpoint
/// 0x81 at a time. Concurrent bulk reads on the same endpoint
/// silently split the contiguous IQ stream between callers — each
/// thread sees valid bytes for its own transfer, but neither has
/// the complete signal. Per #7.
///
/// Constructed via [`Self::try_acquire`]; never instantiated
/// directly.
//
// `dead_code` allow lifts in the follow-up commits that wire the
// guard into `RtlSdrDevice` + `RtlSdrReader` bulk-read entry points.
// Per #7 plan; remove this allow when those callers exist.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct ReaderBusyGuard {
    flag: Arc<AtomicBool>,
}

#[allow(dead_code)]
impl ReaderBusyGuard {
    /// Try to acquire the reader-busy flag. Returns
    /// `Err(RtlSdrError::DeviceBusy)` if another bulk-read activity
    /// is already in flight on this device.
    ///
    /// `Acquire` ordering on success and on the failure-load path is
    /// sufficient: the only invariant being synchronized is "another
    /// caller holds the flag," and the matching `Release` in `Drop`
    /// happens-before the next successful acquire.
    pub(crate) fn try_acquire(flag: Arc<AtomicBool>) -> Result<Self, RtlSdrError> {
        flag.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| RtlSdrError::DeviceBusy)?;
        Ok(Self { flag })
    }
}

impl Drop for ReaderBusyGuard {
    fn drop(&mut self) {
        // Release ordering pairs with the Acquire on the next
        // try_acquire — any writes performed while the guard was
        // held are observable to the next acquirer.
        self.flag.store(false, Ordering::Release);
    }
}

/// Streaming-focused handle. Acquired via [`RtlSdrDevice::reader`].
///
/// `RtlSdrReader` exists to resolve the design tension between
/// Rust's ownership model (control methods like
/// [`RtlSdrDevice::set_center_freq`] take `&mut self`; concurrent
/// streaming would require holding `self` for the duration) and
/// the underlying USB protocol's reality (bulk reads use endpoint
/// 0x81; control transfers use endpoint 0x00 — different
/// endpoints, no conflict on real hardware).
///
/// The reader internally clones the device's
/// `Arc<rusb::DeviceHandle>`, then exposes the streaming surface
/// (sync iterator + per-runtime async streams) by consuming the
/// reader. The parent retains the [`RtlSdrDevice`] for control:
///
/// ```no_run
/// # use librtlsdr_rs::{RtlSdrDevice, RtlSdrError};
/// # fn example() -> Result<(), RtlSdrError> {
/// let mut device = RtlSdrDevice::open(0)?;
/// device.set_sample_rate(2_400_000)?;
/// device.set_center_freq(100_000_000)?;
/// device.reset_buffer()?;
///
/// // Hand a reader to a worker thread.
/// let reader = device.reader();
/// let thread = std::thread::spawn(move || {
///     for chunk in reader.iter_samples(262_144) {
///         match chunk {
///             Ok(buf) => { /* push to ring / DSP */ let _ = buf; }
///             Err(e) => { eprintln!("read error: {e}"); break; }
///         }
///     }
/// });
///
/// // Parent thread retains control of the device while the reader
/// // streams — separate USB endpoints, no rusb-level conflict.
/// device.set_center_freq(101_000_000)?;
/// device.set_tuner_gain(150)?;
/// # let _ = thread;
/// # Ok(())
/// # }
/// ```
///
/// # Concurrency safety
///
/// The shared-handle pattern (one `Arc<DeviceHandle>` reffed by
/// both the parent device and any reader) is what upstream
/// `librtlsdr`'s reference implementations have used for years.
/// Bulk reads on endpoint 0x81 don't interfere with control
/// transfers on endpoint 0x00 at the libusb level on real hardware.
///
/// **However**, libusb's documentation does not formally
/// guarantee that concurrent bulk and control transfers on a
/// single device handle are safe. The shared-handle pattern is a
/// practical convention rather than a documented promise. If you
/// need strict by-the-book safety, sequence the operations from
/// a single thread (e.g. fully drop the reader before retuning,
/// then build a new reader). For the typical "stream while
/// retuning the satellite at AOS" pattern this works reliably on
/// the dongles in active use; verify against your specific
/// hardware in production.
///
/// # Single active streaming session
///
/// At most one bulk-read activity may be in flight on the device
/// at a time — across `RtlSdrDevice::{read_sync, iter_samples,
/// read_async_blocking}` and `RtlSdrReader::{read_sync,
/// iter_samples, stream_samples_tokio, stream_samples_smol}`.
/// Concurrent attempts return [`RtlSdrError::DeviceBusy`].
///
/// This invariant exists because libusb permits concurrent submits
/// on the same endpoint, but the resulting transfer responses
/// interleave non-deterministically — each thread sees valid
/// bytes for its own libusb transfer, but neither has the
/// complete IQ stream. The runtime guard makes the contention
/// observable as a typed error rather than as silent
/// sample-stream corruption.
///
/// `RtlSdrDevice::usb_handle()` is the documented escape hatch and
/// is *not* gated; bypassing the typed reader path lets you
/// re-create the corruption hazard. See its method docs.
///
/// # Cheap clone via the device
///
/// A [`RtlSdrReader`] is just an `Arc` clone of the device's USB
/// handle plus the per-device busy-flag. Build one via
/// [`RtlSdrDevice::reader`] any time you need a fresh streaming
/// handle — the cost is two atomic increments. Cloning is allowed
/// (and cheap), but only one clone may have an active streaming
/// session at a time; the rest get [`RtlSdrError::DeviceBusy`].
#[derive(Clone)]
pub struct RtlSdrReader {
    pub(crate) handle: Arc<rusb::DeviceHandle<rusb::GlobalContext>>,
    /// Per-device reader-busy flag (cloned from the parent
    /// [`RtlSdrDevice::reader_busy`]). Acquired via
    /// [`ReaderBusyGuard::try_acquire`] at the top of every
    /// bulk-read entry point on this reader to enforce single-active-
    /// reader. Per #7.
    pub(crate) busy: Arc<AtomicBool>,
}

impl RtlSdrReader {
    /// Synchronous bulk read into a caller-owned buffer.
    ///
    /// Mirror of [`RtlSdrDevice::read_sync`] with the same
    /// semantics, exposed on the Reader so streaming code that
    /// already has a Reader doesn't need to round-trip through
    /// the device.
    ///
    /// # Errors
    ///
    /// - [`RtlSdrError::DeviceLost`] if the dongle was
    ///   disconnected.
    /// - [`RtlSdrError::DeviceBusy`] if another bulk-read activity
    ///   (sync read, blocking iterator, async stream) is already
    ///   in flight on this device. Per #7.
    /// - [`RtlSdrError::Usb`] for any other rusb transport
    ///   error.
    pub fn read_sync(&self, buf: &mut [u8]) -> Result<usize, RtlSdrError> {
        let _guard = ReaderBusyGuard::try_acquire(Arc::clone(&self.busy))?;
        super::streaming::bulk_read(&self.handle, buf)
    }

    /// Sync iterator over IQ-sample buffers, consuming the
    /// reader.
    ///
    /// Each [`Iterator::next`] performs one [`Self::read_sync`]
    /// into a freshly-allocated `Vec<u8>` and yields it. Same
    /// fuse-on-error semantics as [`RtlSdrDevice::iter_samples`]:
    /// returns `None` permanently after the first error or
    /// zero-length read.
    ///
    /// Consumes the reader so the iterator owns the
    /// `Arc<DeviceHandle>` clone — usable across thread
    /// boundaries (`'static`-friendly, sendable).
    ///
    /// # Buffer size
    ///
    /// Same guidance as [`RtlSdrDevice::iter_samples`] — 256 KB
    /// (`262_144`) is the librtlsdr-equivalent default. Passing
    /// `0` selects the default.
    #[must_use]
    pub fn iter_samples(self, buffer_size: usize) -> ReaderIter {
        let buffer_size = if buffer_size == 0 {
            crate::constants::DEFAULT_BUF_LENGTH as usize
        } else {
            buffer_size
        };
        // Acquire the reader-busy guard for the iterator's lifetime.
        // On contention, `pending_error` carries the `DeviceBusy`
        // to yield on first `next()` (then fuse) — matches the
        // existing fuse-on-error contract documented on
        // `ReaderIter`. Per #7.
        let (guard, pending_error) = match ReaderBusyGuard::try_acquire(Arc::clone(&self.busy)) {
            Ok(g) => (Some(g), None),
            Err(e) => (None, Some(e)),
        };
        ReaderIter {
            reader: Some(self),
            buffer_size,
            _guard: guard,
            pending_error,
        }
    }
}

/// Owned, sendable iterator over IQ-sample buffers, returned by
/// [`RtlSdrReader::iter_samples`].
///
/// Differs from [`crate::SampleIter`] in that it owns the reader
/// (and thus the underlying `Arc<DeviceHandle>` clone) rather
/// than borrowing the device — so it satisfies `'static` and can
/// be sent to other threads / async runtimes. Same
/// `FusedIterator` contract: `None` permanently after the first
/// error or zero read.
pub struct ReaderIter {
    /// `None` once the iterator has fused.
    reader: Option<RtlSdrReader>,
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
    /// iterator fuses normally afterward via `reader = None`.
    pending_error: Option<RtlSdrError>,
}

impl Iterator for ReaderIter {
    type Item = Result<Vec<u8>, RtlSdrError>;

    fn next(&mut self) -> Option<Self::Item> {
        // Yield any deferred construction error first, then fuse.
        if let Some(e) = self.pending_error.take() {
            self.reader = None;
            return Some(Err(e));
        }
        let reader = self.reader.as_ref()?;
        let mut buf = vec![0u8; self.buffer_size];
        // Bypass `reader.read_sync` (which would re-acquire its own
        // guard per call) — the iterator already holds the guard
        // for its lifetime via `_guard`. Per #7.
        match super::streaming::bulk_read(&reader.handle, &mut buf) {
            Ok(0) => {
                self.reader = None;
                None
            }
            Ok(n) => {
                buf.truncate(n);
                Some(Ok(buf))
            }
            Err(e) => {
                self.reader = None;
                Some(Err(e))
            }
        }
    }
}

impl std::iter::FusedIterator for ReaderIter {}

#[cfg(test)]
mod tests {
    use super::*;

    // Pin the trait + marker contract: ReaderIter is Iterator +
    // FusedIterator + Send. The Send guarantee is the whole
    // point of the Reader split — the iterator must move freely
    // between threads / async runtimes.
    const _: fn() = || {
        fn assert_iter<T: Iterator>() {}
        fn assert_fused<T: std::iter::FusedIterator>() {}
        fn assert_send<T: Send>() {}
        assert_iter::<ReaderIter>();
        assert_fused::<ReaderIter>();
        assert_send::<ReaderIter>();
        assert_send::<RtlSdrReader>();
    };

    /// Per #7: a second `try_acquire` while a guard is alive must
    /// return [`RtlSdrError::DeviceBusy`].
    #[test]
    fn busy_guard_first_acquire_succeeds_second_returns_device_busy() {
        let flag = Arc::new(AtomicBool::new(false));
        let _guard1 = ReaderBusyGuard::try_acquire(Arc::clone(&flag))
            .expect("first acquire on a free flag must succeed");
        let result = ReaderBusyGuard::try_acquire(Arc::clone(&flag));
        assert!(
            matches!(result, Err(RtlSdrError::DeviceBusy)),
            "expected DeviceBusy on contended acquire, got {result:?}",
        );
    }

    /// Per #7: dropping the guard must clear the flag so subsequent
    /// acquires succeed.
    #[test]
    fn busy_guard_drop_releases_flag() {
        let flag = Arc::new(AtomicBool::new(false));
        {
            let _guard = ReaderBusyGuard::try_acquire(Arc::clone(&flag))
                .expect("first acquire must succeed");
            assert!(
                flag.load(Ordering::Acquire),
                "flag must be set while guard is alive",
            );
        }
        assert!(
            !flag.load(Ordering::Acquire),
            "flag must be cleared after guard drop",
        );
        let _guard2 = ReaderBusyGuard::try_acquire(Arc::clone(&flag))
            .expect("acquire after drop must succeed");
    }
}
