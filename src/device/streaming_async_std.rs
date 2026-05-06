//! async-std `Stream` adapter for IQ-sample reads.
//!
//! Gated on `feature = "async-std"`. Bridges the synchronous USB
//! bulk-read path into a `Stream` consumable from an async-std
//! executor without blocking it.
//!
//! # Implementation
//!
//! Mirrors the tokio variant (`super::streaming_tokio`) with
//! two differences:
//!
//! - **Blocking offload** uses [`async_std::task::spawn_blocking`]
//!   instead of `tokio::task::spawn_blocking`.
//! - **mpsc bridge** uses the runtime-agnostic [`async_channel`]
//!   crate, whose `Receiver` already implements `Stream` natively
//!   — no manual `poll_next` impl needed; we wrap the receiver in
//!   a newtype to keep the public type name runtime-tagged.
//!
//! Same back-pressure-by-default channel depth
//! ([`STREAM_BACKPRESSURE_DEPTH`]) as the tokio variant — sample
//! drops are usually fatal for SDR (gaps in the stream).
//!
//! # Drop semantics
//!
//! Same trade-off as the tokio path: between-reads drops exit
//! within one buffer cadence (~65 ms typical at 2 Msps); a drop
//! while a USB read is in flight waits up to one read timeout
//! (~5 s on a stalled device). True in-flight cancellation needs
//! libusb's async-submit + cancel API and is tracked as #633.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;

use crate::error::RtlSdrError;

use super::RtlSdrDevice;

/// Channel depth — see `super::streaming_tokio` for the
/// derivation (4 × 256 KB ≈ 1 MB ≈ 250 ms at 2 Msps × 2 bytes/
/// sample = 4 MB/s). Matched here so async-std consumers see
/// the same back-pressure shape as tokio consumers.
const STREAM_BACKPRESSURE_DEPTH: usize = 4;

/// Type alias for the boxed-pinned async-channel receiver used
/// inside [`AsyncStdSampleStream`]. Pulled out to keep the
/// struct definition readable (and to satisfy clippy's
/// `type_complexity` lint).
type BoxedReceiver = Pin<Box<async_channel::Receiver<Result<Vec<u8>, RtlSdrError>>>>;

impl RtlSdrDevice {
    /// Stream IQ samples as an async-std-friendly `Stream`.
    ///
    /// Same shape as `Self::stream_samples_tokio` (only present
    /// when the `tokio` feature is enabled) — consumes the
    /// device, returns the device on the error path so the
    /// caller can recover its configured state. This method
    /// differs only in which runtime drives the blocking
    /// offload.
    ///
    /// # Errors
    ///
    /// Currently never fails at the preflight stage —
    /// `async_std::task::spawn_blocking` works without an
    /// active executor (queues the closure for the next
    /// poll). The error type is kept as
    /// `Box<(RtlSdrError, Self)>` for shape parity with the
    /// other runtime variants in case a future error path
    /// (e.g. allocator failure on the channel construction)
    /// surfaces.
    ///
    /// ```no_run
    /// # #[cfg(feature = "async-std")]
    /// # async fn example() -> Result<(), sdr_rtlsdr::RtlSdrError> {
    /// use futures_core::Stream;
    /// use std::pin::Pin;
    /// use sdr_rtlsdr::RtlSdrDevice;
    ///
    /// let dev = RtlSdrDevice::open(0)?;
    /// dev.reset_buffer()?;
    /// let stream = dev.stream_samples_async_std(262_144).map_err(|boxed| boxed.0)?;
    /// let mut stream: Pin<Box<dyn Stream<Item = _>>> = Box::pin(stream);
    /// // futures_util::StreamExt::next() — left to the consumer's choice of helper crate.
    /// # Ok(())
    /// # }
    /// ```
    pub fn stream_samples_async_std(
        self,
        buffer_size: usize,
    ) -> Result<AsyncStdSampleStream, Box<(RtlSdrError, Self)>> {
        let (tx, rx) = async_channel::bounded(STREAM_BACKPRESSURE_DEPTH);

        // Spawn the blocking iterator-driver onto async-std's
        // blocking thread pool. Unlike tokio, async-std's
        // `spawn_blocking` doesn't require an active runtime
        // context — it manages its own pool — so no preflight
        // check needed here.
        async_std::task::spawn_blocking(move || {
            let dev = self;
            let mut iter = dev.iter_samples(buffer_size);
            loop {
                if tx.is_closed() {
                    return;
                }
                match iter.next() {
                    Some(chunk) => {
                        let is_err = chunk.is_err();
                        if tx.send_blocking(chunk).is_err() {
                            return;
                        }
                        if is_err {
                            return;
                        }
                    }
                    None => return,
                }
            }
        });

        Ok(AsyncStdSampleStream { rx: Box::pin(rx) })
    }
}

/// async-std's `Stream` over IQ-sample buffers, returned by
/// [`RtlSdrDevice::stream_samples_async_std`].
///
/// Wraps a pinned, boxed [`async_channel::Receiver`] whose
/// `Stream` impl drives `poll_next`. The receiver is `!Unpin`
/// (carries a `PhantomPinned` to keep its internal event-listener
/// in place), so storing it as `Pin<Box<…>>` lets us pin-project
/// safely without unsafe code or a `pin-project` macro dep. The
/// single heap allocation happens once at stream construction.
///
/// Newtype'd so consumers don't need to import `async_channel` to
/// name the stream type.
pub struct AsyncStdSampleStream {
    rx: BoxedReceiver,
}

impl Stream for AsyncStdSampleStream {
    type Item = Result<Vec<u8>, RtlSdrError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.as_mut().poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Same trait + marker contract as the tokio variant.
    const _: fn() = || {
        fn assert_stream<T: Stream>() {}
        fn assert_send<T: Send>() {}
        assert_stream::<AsyncStdSampleStream>();
        assert_send::<AsyncStdSampleStream>();
    };
}
