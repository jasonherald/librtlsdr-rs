//! Error types for the RTL-SDR driver.

/// Errors from RTL-SDR USB operations.
///
/// `Clone`, `PartialEq`, and `Eq` are derived to support the
/// common consumer patterns of stashing the last error in an
/// `Arc<Mutex<Option<RtlSdrError>>>`, snapshotting it across UI
/// re-renders, or asserting equality in tests. Per #15.
///
/// `#[non_exhaustive]` so adding a new variant in a future patch
/// release is non-breaking. Consumers should always include a
/// catch-all `_ => ...` arm in exhaustive match. Per #16 / 0.2.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RtlSdrError {
    /// USB communication error.
    #[error("USB error: {0}")]
    Usb(#[from] rusb::Error),

    /// Device not found at the specified index. **Struct variant
    /// since 0.2** — was `DeviceNotFound(u32)` in 0.1.x.
    #[error("device not found at index {index}")]
    DeviceNotFound { index: u32 },

    /// No supported tuner detected on the device.
    #[error("no supported tuner found")]
    NoTuner,

    /// Tuner operation failed.
    #[error("tuner error: {0}")]
    Tuner(String),

    /// Invalid sample rate. **Struct variant since 0.2** — was
    /// `InvalidSampleRate(u32)` in 0.1.x.
    #[error("invalid sample rate: {rate_hz} Hz")]
    InvalidSampleRate { rate_hz: u32 },

    /// Invalid parameter.
    #[error("invalid parameter: {0}")]
    InvalidParameter(String),

    /// Device is busy (another bulk-read activity is already in
    /// flight on this device — see `RtlSdrReader`'s busy-flag
    /// guard added in 0.1.1 / #7).
    #[error("device busy")]
    DeviceBusy,

    /// Device was lost (USB disconnect).
    #[error("device lost")]
    DeviceLost,

    /// Register write/read failed: the USB control transfer
    /// reported fewer bytes than the operation requested.
    /// `block` names the access category (block-addressed write,
    /// demod-addressed write, I2C / EEPROM, etc.); `address` is
    /// the register address the operation was targeting.
    /// **Struct variant with context fields since 0.2** — was
    /// `RegisterAccess` (no payload) in 0.1.x.
    #[error("register access failed (block={block:?}, addr=0x{address:04x})")]
    RegisterAccess {
        block: crate::reg::Block,
        address: u16,
    },
}

impl RtlSdrError {
    /// Returns `true` if the error indicates the dongle was
    /// disconnected (USB unplug, kernel-driver-rebind, etc.).
    ///
    /// Useful in reconnect loops:
    ///
    /// ```
    /// use librtlsdr_rs::RtlSdrError;
    /// // Synthesize for the doctest; in practice this would come
    /// // from a method like `read_sync`.
    /// let e = RtlSdrError::DeviceLost;
    /// assert!(e.is_disconnected());
    /// ```
    ///
    /// Recognises both [`RtlSdrError::DeviceLost`] (the crate's
    /// internal "we observed disconnect" sentinel — see `read_sync`
    /// and friends) and the underlying [`rusb::Error::NoDevice`]
    /// case for paths that haven't translated yet. Per #15.
    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        matches!(self, Self::DeviceLost | Self::Usb(rusb::Error::NoDevice))
    }

    /// Returns `true` if the error is a transient transport
    /// timeout (the USB transfer didn't complete within the
    /// configured deadline).
    ///
    /// Useful in retry-with-backoff wrappers around bulk reads.
    /// The example uses the crate's `pub use rusb` re-export so
    /// the consumer doesn't need a direct `rusb` dependency
    /// (the whole point of [`crate::rusb`]):
    ///
    /// ```
    /// use librtlsdr_rs::{RtlSdrError, rusb};
    /// let e = RtlSdrError::Usb(rusb::Error::Timeout);
    /// assert!(e.is_timeout());
    /// ```
    ///
    /// A timeout typically means "device is alive but didn't have
    /// data ready" — distinct from [`Self::is_disconnected`].
    /// Per #15.
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Usb(rusb::Error::Timeout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_disconnected_recognises_device_lost_and_no_device() {
        assert!(RtlSdrError::DeviceLost.is_disconnected());
        assert!(RtlSdrError::Usb(rusb::Error::NoDevice).is_disconnected());
    }

    #[test]
    fn is_disconnected_returns_false_for_other_variants() {
        assert!(!RtlSdrError::DeviceBusy.is_disconnected());
        assert!(!RtlSdrError::Usb(rusb::Error::Timeout).is_disconnected());
        assert!(!RtlSdrError::NoTuner.is_disconnected());
        assert!(!RtlSdrError::DeviceNotFound { index: 0 }.is_disconnected());
        assert!(!RtlSdrError::Tuner("anything".to_string()).is_disconnected());
    }

    #[test]
    fn is_timeout_recognises_only_usb_timeout() {
        assert!(RtlSdrError::Usb(rusb::Error::Timeout).is_timeout());
    }

    #[test]
    fn is_timeout_returns_false_for_other_variants() {
        assert!(!RtlSdrError::DeviceLost.is_timeout());
        assert!(!RtlSdrError::DeviceBusy.is_timeout());
        assert!(!RtlSdrError::Usb(rusb::Error::NoDevice).is_timeout());
        assert!(!RtlSdrError::Usb(rusb::Error::Io).is_timeout());
        assert!(
            !RtlSdrError::RegisterAccess {
                block: crate::reg::Block::Demod,
                address: 0
            }
            .is_timeout()
        );
    }
}
