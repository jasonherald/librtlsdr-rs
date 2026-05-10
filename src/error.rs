//! Error types for the RTL-SDR driver.

/// Errors from RTL-SDR USB operations.
///
/// `Clone`, `PartialEq`, and `Eq` are derived to support the
/// common consumer patterns of stashing the last error in an
/// `Arc<Mutex<Option<RtlSdrError>>>`, snapshotting it across UI
/// re-renders, or asserting equality in tests. Per #15.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RtlSdrError {
    /// USB communication error.
    #[error("USB error: {0}")]
    Usb(#[from] rusb::Error),

    /// Device not found at the specified index.
    #[error("device not found at index {0}")]
    DeviceNotFound(u32),

    /// No supported tuner detected on the device.
    #[error("no supported tuner found")]
    NoTuner,

    /// Tuner operation failed.
    #[error("tuner error: {0}")]
    Tuner(String),

    /// Invalid sample rate.
    #[error("invalid sample rate: {0} Hz")]
    InvalidSampleRate(u32),

    /// Invalid parameter.
    #[error("invalid parameter: {0}")]
    InvalidParameter(String),

    /// Device is busy (async read in progress).
    #[error("device busy")]
    DeviceBusy,

    /// Device was lost (USB disconnect).
    #[error("device lost")]
    DeviceLost,

    /// Register write/read failed.
    #[error("register access failed")]
    RegisterAccess,
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
    /// Useful in retry-with-backoff wrappers around bulk reads:
    ///
    /// ```
    /// use librtlsdr_rs::RtlSdrError;
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
        assert!(!RtlSdrError::DeviceNotFound(0).is_disconnected());
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
        assert!(!RtlSdrError::RegisterAccess.is_timeout());
    }
}
