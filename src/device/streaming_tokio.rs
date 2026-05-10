//! Tokio `Stream` adapter for IQ-sample reads.
//!
//! Gated on `feature = "tokio"`. Bridges the synchronous USB
//! bulk-read path into an async `Stream` consumable from any
//! tokio runtime context, without blocking the executor.
//!
//! # Implementation
//!
//! `tokio::task::spawn_blocking` runs a [`super::ReaderIter`]
//! loop on tokio's blocking-task thread pool, pushing each
//! yielded buffer through a `tokio::sync::mpsc` channel. The
//! returned [`TokioSampleStream`] drains the receiver as a
//! `Stream`.
//!
//! Bounded channel ([`STREAM_BACKPRESSURE_DEPTH`] = 4) provides
//! back-pressure: if the consumer falls behind the reader thread
//! blocks on `blocking_send` rather than dropping samples. For
//! SDR, sample drops are usually fatal (gaps in the stream) — the
//! back-pressure default is correct. Tune the consumer (or scale
//! up to a faster runtime) rather than widening the channel.
//!
//! When the consumer drops the `Stream`, the worker observes the
//! closed channel between reads and exits cleanly. On transport
//! error the worker pushes the error and exits; the `Stream`
//! yields the error then `None`.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_core::Stream;

use crate::error::RtlSdrError;

use crate::constants::STREAM_BACKPRESSURE_DEPTH;

use super::RtlSdrReader;
use super::reader::ReaderBusyGuard;

impl RtlSdrReader {
    /// Stream IQ samples as a tokio-friendly async `Stream`.
    ///
    /// Consumes the reader. The returned [`TokioSampleStream`]
    /// owns the reader inside a blocking task; build a fresh
    /// reader via [`super::RtlSdrDevice::reader`] if you need
    /// another stream session.
    ///
    /// # Errors
    ///
    /// On contention with an existing bulk-read activity, returns
    /// [`RtlSdrError::DeviceBusy`] paired with the unconsumed
    /// [`RtlSdrReader`] so the caller can retry once the active
    /// stream drops. Per #7.
    ///
    /// On preflight failure (no tokio runtime active) the
    /// returned `Err` carries both the diagnostic
    /// [`RtlSdrError`] and the unconsumed [`RtlSdrReader`] back
    /// to the caller. The reader is cheap to recreate but the
    /// pattern matches the std-library "error preserves the
    /// resource" idiom (see `Vec::push_within_capacity`,
    /// `mpsc::Sender::send`'s `SendError<T>`); boxed because
    /// the inline tuple trips clippy's `result_large_err` lint.
    ///
    /// ```no_run
    /// # #[cfg(feature = "tokio")]
    /// # async fn example() -> Result<(), librtlsdr_rs::RtlSdrError> {
    /// use futures_core::Stream;
    /// use std::pin::Pin;
    /// use librtlsdr_rs::RtlSdrDevice;
    ///
    /// let mut dev = RtlSdrDevice::open(0)?;
    /// dev.reset_buffer()?;
    /// let reader = dev.reader();
    /// let stream = reader.stream_samples_tokio(262_144).map_err(|boxed| boxed.0)?;
    /// let mut stream: Pin<Box<dyn Stream<Item = _>>> = Box::pin(stream);
    /// // futures_util::StreamExt::next() — left to the consumer's choice of helper crate.
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// `buffer_size` follows the same guidance as
    /// [`Self::iter_samples`] — 256 KB / 64 KB are typical good
    /// values. Passing `0` selects the default.
    ///
    /// # Runtime requirement
    ///
    /// Must be called from inside a tokio runtime context (the
    /// implementation calls [`tokio::task::spawn_blocking`]).
    /// Returns [`RtlSdrError::InvalidParameter`] when called
    /// outside a runtime — checked via
    /// [`tokio::runtime::Handle::try_current`] before any task
    /// spawn so the failure mode is a clean error instead of
    /// the runtime's own panic.
    ///
    /// # Drop semantics
    ///
    /// Between-reads drops exit within one buffer cadence
    /// (~65 ms typical at 2 Msps); a drop while a USB read is
    /// in flight waits up to one read timeout (~5 s on a
    /// stalled device). True in-flight cancellation needs
    /// libusb's async-submit + cancel API and is tracked as
    /// #633.
    pub fn stream_samples_tokio(
        self,
        buffer_size: usize,
    ) -> Result<TokioSampleStream, Box<(RtlSdrError, Self)>> {
        // Preflight runtime check BEFORE consuming the reader.
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(Box::new((
                RtlSdrError::InvalidParameter(
                    "stream_samples_tokio must be called from within a Tokio runtime".to_string(),
                ),
                self,
            )));
        }

        // Eagerly acquire the reader-busy guard. On contention,
        // return the unconsumed reader so the caller can retry once
        // the existing stream drops. Per #7. The guard is moved
        // into the spawn_blocking closure below, so it lives for
        // the entire worker's lifetime and releases on Drop when
        // the worker returns (clean exit, error, or consumer drop).
        let guard = match ReaderBusyGuard::try_acquire(Arc::clone(&self.busy)) {
            Ok(g) => g,
            Err(e) => return Err(Box::new((e, self))),
        };

        let buffer_size = if buffer_size == 0 {
            crate::constants::DEFAULT_BUF_LENGTH as usize
        } else {
            buffer_size
        };

        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_BACKPRESSURE_DEPTH);

        // The blocking task owns the reader (and via it the
        // Arc<DeviceHandle> clone) for the duration of the
        // stream. Pre-read drop check via `tx.is_closed()`
        // catches consumer drops in the brief window between
        // reads; mid-read drops still wait for the in-flight
        // bulk transfer to return (see method-level "Drop
        // semantics" docs).
        //
        // The read loop calls `bulk_read` directly rather than
        // `iter_samples` to avoid the iterator's own re-acquire
        // path — we already hold the guard. Per #7.
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let reader = self;
            loop {
                if tx.is_closed() {
                    return;
                }
                let mut buf = vec![0u8; buffer_size];
                match super::streaming::bulk_read(&reader.handle, &mut buf) {
                    Ok(0) => return, // fuse on zero-length read
                    Ok(n) => {
                        buf.truncate(n);
                        if tx.blocking_send(Ok(buf)).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx.blocking_send(Err(e));
                        return;
                    }
                }
            }
        });

        Ok(TokioSampleStream { rx })
    }
}

/// Async `Stream` over IQ-sample buffers, returned by
/// [`RtlSdrReader::stream_samples_tokio`].
///
/// Owns the receiver end of the tokio mpsc channel; the worker
/// task on the other end terminates when this stream is dropped
/// (next blocking-send fails). No additional cleanup required
/// from the consumer.
pub struct TokioSampleStream {
    rx: tokio::sync::mpsc::Receiver<Result<Vec<u8>, RtlSdrError>>,
}

impl Stream for TokioSampleStream {
    type Item = Result<Vec<u8>, RtlSdrError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pin the Stream + Send contract.
    const _: fn() = || {
        fn assert_stream<T: Stream>() {}
        fn assert_send<T: Send>() {}
        assert_stream::<TokioSampleStream>();
        assert_send::<TokioSampleStream>();
    };
}
