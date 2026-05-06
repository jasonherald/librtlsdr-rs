//! smol `Stream` adapter for IQ-sample reads.
//!
//! Gated on `feature = "smol"`. Bridges the synchronous USB
//! bulk-read path into a `Stream` consumable from a smol-family
//! executor (smol, async-executor, async-global-executor) without
//! blocking it.
//!
//! # Implementation
//!
//! Mirrors the async-std variant (`super::streaming_async_std`)
//! with one difference: the blocking offload uses
//! [`blocking::unblock`] from the `blocking` crate (which is what
//! `smol::unblock` re-exports under the hood) rather than
//! async-std's spawn_blocking. Same
//! [`async_channel`] mpsc bridge, same back-pressure shape, same
//! drop semantics (~65 ms typical, up to one read timeout on a
//! stalled device — see #633 for the libusb-cancel follow-up).
//!
//! Pulling in `blocking` directly rather than the full `smol`
//! crate keeps the dep tree minimal — `smol` re-exports
//! `blocking::unblock` as `smol::unblock`, so the public surface
//! is the same regardless.
//!
//! # Returning the Task
//!
//! [`blocking::unblock`] returns a [`blocking::Task`] which the
//! runtime drives forward when polled. Unlike tokio's
//! `spawn_blocking` (fire-and-forget) or async-std's
//! `spawn_blocking` (also fire-and-forget — its `JoinHandle`
//! detaches automatically), `blocking::Task` cancels its
//! underlying work if dropped without `.detach()`. We call
//! `.detach()` so the worker continues running until the
//! channel closes naturally on consumer drop, matching the
//! semantics of the other two runtime variants.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;

use crate::error::RtlSdrError;

use super::RtlSdrDevice;

/// Channel depth — same derivation as the other runtime
/// variants; see `super::streaming_tokio`.
const STREAM_BACKPRESSURE_DEPTH: usize = 4;

/// Type alias for the boxed-pinned async-channel receiver used
/// inside [`SmolSampleStream`] — keeps the struct readable and
/// satisfies clippy's `type_complexity` lint.
type BoxedReceiver = Pin<Box<async_channel::Receiver<Result<Vec<u8>, RtlSdrError>>>>;

impl RtlSdrDevice {
    /// Stream IQ samples as a smol-friendly `Stream`.
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
    /// [`blocking::unblock`] runs on its own internal thread
    /// pool independent of any active executor. The error type
    /// is kept as `Box<(RtlSdrError, Self)>` for shape parity
    /// with the other runtime variants.
    ///
    /// ```no_run
    /// # #[cfg(feature = "smol")]
    /// # async fn example() -> Result<(), sdr_rtlsdr::RtlSdrError> {
    /// use futures_core::Stream;
    /// use std::pin::Pin;
    /// use sdr_rtlsdr::RtlSdrDevice;
    ///
    /// let dev = RtlSdrDevice::open(0)?;
    /// dev.reset_buffer()?;
    /// let stream = dev.stream_samples_smol(262_144).map_err(|boxed| boxed.0)?;
    /// let mut stream: Pin<Box<dyn Stream<Item = _>>> = Box::pin(stream);
    /// // futures_util::StreamExt::next() — left to the consumer's choice of helper crate.
    /// # Ok(())
    /// # }
    /// ```
    pub fn stream_samples_smol(
        self,
        buffer_size: usize,
    ) -> Result<SmolSampleStream, Box<(RtlSdrError, Self)>> {
        let (tx, rx) = async_channel::bounded(STREAM_BACKPRESSURE_DEPTH);

        // `blocking::unblock` returns a `Task` that cancels its
        // underlying work on drop. Detach so the worker runs to
        // natural completion (channel closure on consumer drop)
        // — matches the fire-and-forget shape of the tokio /
        // async-std variants.
        blocking::unblock(move || {
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
        })
        .detach();

        Ok(SmolSampleStream { rx: Box::pin(rx) })
    }
}

/// smol's `Stream` over IQ-sample buffers, returned by
/// [`RtlSdrDevice::stream_samples_smol`].
///
/// Same shape as `super::AsyncStdSampleStream` — wraps a
/// pinned, boxed [`async_channel::Receiver`] (the receiver is
/// `!Unpin`; pinning into a `Box` lets us project safely without
/// unsafe code or a `pin-project` macro dep). One heap alloc at
/// stream construction.
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
    };
}
