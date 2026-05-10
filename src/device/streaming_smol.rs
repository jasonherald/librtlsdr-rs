//! smol `Stream` adapter for IQ-sample reads.
//!
//! Gated on `feature = "smol"`. Bridges the synchronous USB
//! bulk-read path into a `Stream` consumable from a smol-family
//! executor (smol, async-executor, async-global-executor) without
//! blocking it.
//!
//! Mirrors the tokio variant (`super::streaming_tokio`) but uses
//! [`blocking::unblock`] (the foundation `smol::unblock` re-
//! exports) for the offload and `async_channel` for the mpsc
//! bridge instead of `tokio::sync::mpsc`. Same back-pressure
//! shape, same drop semantics.
//!
//! [`blocking::unblock`] returns a [`blocking::Task`] which
//! cancels its underlying work if dropped. We call `.detach()`
//! so the worker runs to natural completion — matches the
//! fire-and-forget shape of the tokio variant.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_core::Stream;

use crate::error::RtlSdrError;

use crate::constants::STREAM_BACKPRESSURE_DEPTH;

use super::RtlSdrReader;
use super::reader::ReaderBusyGuard;

// Audit issue #20 suggested dropping `Pin<Box<Receiver>>` in
// favor of storing `async_channel::Receiver` directly and pinning
// it on each poll via `Pin::new(&mut self.rx)`. **That doesn't
// work with current `async-channel` (2.5.0):** the receiver is
// implemented via `pin_project!` and is `!Unpin` (the inner
// `event-listener` machinery requires pinning). `Pin::new`
// requires the pointee to be `Unpin`, so the suggested
// simplification doesn't compile.
//
// `Box<T>: Unpin` always (regardless of `T`), so the original
// `Pin<Box<Receiver>>` shape sidesteps the `!Unpin` Receiver via
// the Box's own Unpin. Keep it. Revisit if `async-channel` ever
// makes Receiver Unpin (unlikely without a major version bump).
type BoxedReceiver = Pin<Box<async_channel::Receiver<Result<Vec<u8>, RtlSdrError>>>>;
impl RtlSdrReader {
    /// Stream IQ samples as a smol-friendly `Stream`.
    ///
    /// Same shape as `Self::stream_samples_tokio` (only present
    /// when the `tokio` feature is enabled). Differs only in
    /// which runtime drives the blocking offload.
    ///
    /// # Errors
    ///
    /// - [`RtlSdrError::DeviceBusy`] if another bulk-read activity
    ///   (sync read, blocking iterator, async stream — including a
    ///   tokio stream on the same device) is already in flight.
    ///   The unconsumed reader is returned to the caller so it can
    ///   be retried once the existing stream drops. Per #7.
    /// - No runtime preflight errors today —
    ///   [`blocking::unblock`] runs on its own internal thread pool
    ///   independent of any active executor.
    ///
    /// ```no_run
    /// # #[cfg(feature = "smol")]
    /// # async fn example() -> Result<(), librtlsdr_rs::RtlSdrError> {
    /// use futures_core::Stream;
    /// use std::pin::Pin;
    /// use librtlsdr_rs::RtlSdrDevice;
    ///
    /// let mut dev = RtlSdrDevice::open(0)?;
    /// dev.reset_buffer()?;
    /// let reader = dev.reader();
    /// let stream = reader.stream_samples_smol(262_144).map_err(|boxed| boxed.0)?;
    /// let mut stream: Pin<Box<dyn Stream<Item = _>>> = Box::pin(stream);
    /// # Ok(())
    /// # }
    /// ```
    pub fn stream_samples_smol(
        self,
        buffer_size: usize,
    ) -> Result<SmolSampleStream, Box<(RtlSdrError, Self)>> {
        // Eagerly acquire the reader-busy guard. On contention,
        // return the unconsumed reader so the caller can retry once
        // the existing stream drops. The guard moves into the
        // unblock closure below and releases on Drop when the
        // worker returns. Per #7.
        let guard = match ReaderBusyGuard::try_acquire(Arc::clone(&self.busy)) {
            Ok(g) => g,
            Err(e) => return Err(Box::new((e, self))),
        };

        let buffer_size = if buffer_size == 0 {
            crate::constants::DEFAULT_BUF_LENGTH as usize
        } else {
            buffer_size
        };

        let (tx, rx) = async_channel::bounded(STREAM_BACKPRESSURE_DEPTH);

        // Read loop calls `bulk_read` directly rather than
        // `iter_samples` to avoid the iterator's own re-acquire
        // path — we already hold the guard. Per #7.
        blocking::unblock(move || {
            let _guard = guard;
            let reader = self;
            loop {
                if tx.is_closed() {
                    return;
                }
                let mut buf = vec![0u8; buffer_size];
                match super::streaming::bulk_read(&reader.handle, &reader.dev_lost, &mut buf) {
                    Ok(0) => return, // fuse on zero-length read
                    Ok(n) => {
                        buf.truncate(n);
                        if tx.send_blocking(Ok(buf)).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send_blocking(Err(e));
                        return;
                    }
                }
            }
        })
        .detach();

        Ok(SmolSampleStream { rx: Box::pin(rx) })
    }
}

/// smol's `Stream` over IQ-sample buffers, returned by
/// [`RtlSdrReader::stream_samples_smol`].
pub struct SmolSampleStream {
    rx: BoxedReceiver,
}

impl Stream for SmolSampleStream {
    type Item = Result<Vec<u8>, RtlSdrError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.as_mut().poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const _: fn() = || {
        fn assert_stream<T: Stream>() {}
        fn assert_send<T: Send>() {}
        assert_stream::<SmolSampleStream>();
        assert_send::<SmolSampleStream>();
        // Item: Send pin — same rationale as the tokio sibling.
        // Per audit issue #20.
        assert_send::<<SmolSampleStream as Stream>::Item>();
    };
}
