//! Tokio `Stream` adapter for IQ-sample reads.
//!
//! Gated on `feature = "tokio"`. Bridges the synchronous USB
//! bulk-read path into an async `Stream` consumable from any
//! tokio runtime context, without blocking the executor.
//!
//! # Implementation
//!
//! `tokio::task::spawn_blocking` runs the underlying
//! [`super::SampleIter`] loop on tokio's blocking-task thread
//! pool, pushing each yielded buffer through a
//! `tokio::sync::mpsc` channel. The returned [`SampleStream`]
//! drains the receiver as a `Stream`.
//!
//! Bounded channel (depth = [`STREAM_BACKPRESSURE_DEPTH`])
//! provides back-pressure: if the consumer falls behind the
//! reader thread blocks on `blocking_send` rather than dropping
//! samples. For SDR, sample drops are usually fatal (gaps in
//! the stream) — the back-pressure default is correct. Tune
//! the consumer (or scale up to a faster runtime) rather than
//! widening the channel.
//!
//! When the consumer drops the `Stream`, the channel closes and
//! the worker exits on the next `blocking_send` failure. On
//! transport error the worker pushes the error and exits; the
//! `Stream` yields the error, then `None`.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;

use crate::error::RtlSdrError;

use super::RtlSdrDevice;

/// Number of buffers the tokio mpsc channel holds before the
/// reader thread blocks. Picked to give the consumer ~1 second
/// of slack at typical RTL-SDR rates (4 × 256 KB ≈ 1 MB ≈ 0.4 s
/// at 2 Msps × 2 bytes/sample, so 4 buffers ≈ 260 ms — enough
/// to absorb a slow tick on the consumer without dropping a
/// transfer, not so much that latency-sensitive consumers
/// observe a long queue).
const STREAM_BACKPRESSURE_DEPTH: usize = 4;

impl RtlSdrDevice {
    /// Stream IQ samples as a tokio-friendly async `Stream`.
    ///
    /// Consumes the device. The returned [`SampleStream`] owns
    /// the [`RtlSdrDevice`] inside a blocking task — there's no
    /// way to drive both the stream and other control methods
    /// concurrently against the same handle without giving up
    /// the `Send`-but-not-`Sync` guarantees we documented on
    /// the device. Configure the device (frequency, bandwidth,
    /// gain, etc.) before calling this.
    ///
    /// # Errors / termination
    ///
    /// Each yielded item is `Result<Vec<u8>, RtlSdrError>`. The
    /// stream ends (`Poll::Ready(None)`) when:
    /// - The reader observed a transport error and yielded it
    ///   on the previous `poll_next` call. Standard
    ///   error-then-fuse contract.
    /// - The underlying `read_sync` returned zero bytes (rare,
    ///   degenerate-device case).
    /// - The consumer drops the stream — the worker observes
    ///   the closed channel and exits cleanly.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # #[cfg(feature = "tokio")]
    /// # async fn example() -> Result<(), sdr_rtlsdr::RtlSdrError> {
    /// use futures_core::Stream;
    /// use std::pin::Pin;
    /// use sdr_rtlsdr::RtlSdrDevice;
    ///
    /// let dev = RtlSdrDevice::open(0)?;
    /// dev.reset_buffer()?;
    /// let stream = dev.stream_samples_tokio(262_144)?;
    /// let mut stream: Pin<Box<dyn Stream<Item = _>>> = Box::pin(stream);
    /// // futures_util::StreamExt::next() — left to the consumer's choice of helper crate.
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// `buffer_size` follows the same guidance as
    /// [`Self::iter_samples`] — 256 KB / 64 KB are typical good
    /// values; smaller for lower latency, larger to amortise
    /// USB overhead.
    ///
    /// # Runtime requirement
    ///
    /// Must be called from inside a tokio runtime context (the
    /// implementation calls [`tokio::task::spawn_blocking`]
    /// internally). Returns
    /// [`RtlSdrError::InvalidParameter`] when called outside a
    /// runtime — checked via
    /// [`tokio::runtime::Handle::try_current`] before any task
    /// spawn so the failure mode is a clean error instead of
    /// the runtime's own panic.
    ///
    /// # Drop semantics
    ///
    /// When the consumer drops the [`SampleStream`], the worker
    /// observes the closed channel **between** USB reads and
    /// exits cleanly — typical drop latency is one read cadence
    /// (~65 ms at 2 Msps with the default 256 KB buffer). On a
    /// stalled device the worst case is one read timeout (5 s
    /// per [`RtlSdrDevice::read_sync`]). For sub-millisecond
    /// cancellation of an in-flight bulk transfer we'd need
    /// libusb's async-submit + cancel API rather than the
    /// blocking read; that's tracked as #633 rather than done
    /// here. Per #632 CR round 1.
    ///
    /// # Errors
    ///
    /// - [`RtlSdrError::InvalidParameter`] if no tokio runtime
    ///   is active when this method is called.
    pub fn stream_samples_tokio(self, buffer_size: usize) -> Result<SampleStream, RtlSdrError> {
        // Preflight runtime check. `tokio::task::spawn_blocking`
        // doesn't document its outside-runtime behaviour but
        // panics in practice; library code shouldn't panic, so
        // detect explicitly via `try_current` and return an
        // `RtlSdrError`. Per #632 CR round 1.
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(RtlSdrError::InvalidParameter(
                "stream_samples_tokio must be called from within a Tokio runtime".to_string(),
            ));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_BACKPRESSURE_DEPTH);

        // The blocking task owns the device for the duration
        // of the stream — no `Arc<Mutex<…>>`, no shared
        // mutable access. When the consumer drops the
        // `SampleStream` the channel closes; we observe that
        // via `tx.is_closed()` between reads (so a healthy
        // streaming device exits within one buffer cadence)
        // and via `tx.blocking_send` returning `Err` after the
        // read (so a still-completing read isn't wasted). On
        // exit, tokio's runtime drops the task's stack
        // including the device, which runs `Drop` and releases
        // the USB interface cleanly.
        tokio::task::spawn_blocking(move || {
            let dev = self;
            let mut iter = dev.iter_samples(buffer_size);
            loop {
                // Pre-read drop check: catches the common case
                // of a consumer dropping the stream during the
                // brief window between reads. For an in-flight
                // read we still wait for it to return (see
                // method-level "Drop semantics" docs).
                if tx.is_closed() {
                    return;
                }
                match iter.next() {
                    Some(chunk) => {
                        let is_err = chunk.is_err();
                        if tx.blocking_send(chunk).is_err() {
                            return;
                        }
                        if is_err {
                            // Iterator fuses on error; yielding
                            // once matches the documented
                            // "yields the error, then `None`"
                            // contract.
                            return;
                        }
                    }
                    None => return,
                }
            }
        });

        Ok(SampleStream { rx })
    }
}

/// Async `Stream` wrapping the tokio mpsc receiver fed by
/// [`RtlSdrDevice::stream_samples_tokio`]'s blocking worker.
///
/// Owns the receiver end of the channel; the worker task on
/// the other end terminates when this stream is dropped (next
/// blocking-send fails). No additional cleanup is required
/// from the consumer.
pub struct SampleStream {
    rx: tokio::sync::mpsc::Receiver<Result<Vec<u8>, RtlSdrError>>,
}

impl Stream for SampleStream {
    type Item = Result<Vec<u8>, RtlSdrError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pin the trait-impl + marker contract documented on
    // `SampleStream`: it implements `Stream` (so consumers can
    // use `StreamExt`) and is `Send` (so it can cross `await`
    // boundaries on multi-threaded executors). If a future
    // refactor changes the receiver type or adds non-`Send`
    // state, the assertion fires at compile time before
    // breaking downstream consumers.
    const _: fn() = || {
        fn assert_stream<T: Stream>() {}
        fn assert_send<T: Send>() {}
        assert_stream::<SampleStream>();
        assert_send::<SampleStream>();
    };
}
