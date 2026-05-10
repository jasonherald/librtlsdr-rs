//! Error types for the RTL-SDR driver.
//!
//! Two enums:
//! - [`RtlSdrError`] — the unified error type returned by every
//!   fallible operation on the public API. `#[non_exhaustive]`
//!   since 0.2 (per #16); always include a catch-all `_ => ...`
//!   arm in exhaustive matches.
//! - [`TunerError`] — typed sub-variant carried by
//!   [`RtlSdrError::Tuner`] since 0.2 (was `String` in 0.1.x).
//!   Lets consumers programmatically discriminate PLL-not-locked
//!   from gain-out-of-range etc. without parsing message strings.

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

    /// Tuner operation failed. **Carries [`TunerError`] since
    /// 0.2** — was `Tuner(String)` in 0.1.x. Match on the inner
    /// [`TunerError`] for typed discrimination of the underlying
    /// failure (e.g. `Err(Tuner(TunerError::PllNotLocked { .. }))`).
    #[error("tuner error: {0}")]
    Tuner(#[from] TunerError),

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

/// Typed sub-variant of [`RtlSdrError::Tuner`].
///
/// Since 0.2, tuner-side failures carry this enum instead of a
/// stringly-typed `String`. Consumers can match on the variants
/// to discriminate failure modes (e.g. retry on
/// `PllNotLocked`, fail-fast on `XtalIsZero`).
///
/// Some variants carry a `&'static str` `backend` field naming
/// the IC family (`"R82xx"`, `"FC0012"`, `"FC0013"`, `"E4K"`,
/// `"FC2580"`); use it to disambiguate when the same failure
/// shape can happen on multiple ICs.
///
/// `#[non_exhaustive]` so adding a new variant in any future
/// patch release is non-breaking. Per #16.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TunerError {
    /// PLL did not achieve lock for the requested LO frequency
    /// within the IC's retry budget. Usually means the
    /// frequency is at an awkward divider boundary or the
    /// crystal/clock is misconfigured. Backends: R82xx, E4K.
    #[error("PLL not locked for {freq_hz} Hz")]
    PllNotLocked { freq_hz: u32 },

    /// The configured crystal reference is zero, which would
    /// divide-by-zero in PLL math. Backends: R82xx (general),
    /// FC2580 (more specifically "crystal frequency too low").
    #[error("PLL reference (xtal) is zero or below the minimum")]
    XtalIsZero,

    /// PLL programming failed for an IC-specific reason that
    /// doesn't share a common shape with other backends. Catch-
    /// all for "no valid divider found", "computed PLL value
    /// exceeds register range", "VCO out of range", etc. The
    /// `reason` carries a static diagnostic string identifying
    /// the specific failure.
    ///
    /// Backends: R82xx, FC0012, FC0013, FC2580, E4K.
    #[error("{backend}: PLL programming failed for {freq_hz} Hz ({reason})")]
    PllProgrammingFailed {
        backend: &'static str,
        freq_hz: u32,
        reason: &'static str,
    },

    /// I2C transfer to the tuner returned fewer bytes than
    /// expected. `operation` names which step failed
    /// (`"write"`, `"read addr"`, `"read data"`).
    #[error("I2C {operation} failed: got {got} bytes, expected {expected}")]
    I2cTransferFailed {
        operation: &'static str,
        got: usize,
        expected: usize,
    },

    /// R82xx: register read attempted before the shadow cache
    /// was populated. Indicates a programming error in the
    /// crate (caller used the helper before `init`), not a
    /// hardware fault.
    #[error("R82xx: no cached value for register 0x{reg:02x}")]
    ShadowCacheMiss { reg: u8 },

    /// FC2580: the configured filter-bandwidth mode index
    /// doesn't match any supported bandwidth in the IC's LUT.
    /// `mode` is the internal mode tag (not a Hz value).
    #[error("FC2580: unsupported filter bandwidth mode {mode}")]
    UnsupportedFilterBandwidth { mode: u8 },

    /// Gain parameter out of valid range. `what` names the
    /// parameter (`"E4K IF gain stage"`, `"E4K mixer gain"`,
    /// `"E4K enhancement gain"`, etc.) and `detail` is a
    /// human-readable specifier describing the bad value.
    /// Backends: E4K (the only IC with multi-stage gain that
    /// validates per stage).
    #[error("invalid gain ({what}): {detail}")]
    InvalidGain { what: &'static str, detail: String },

    /// Operation context wrapper. Used to add a `&'static str`
    /// prefix (e.g. `"filter calibration"`) to an inner
    /// `TunerError` without losing the typed inner variant.
    /// `#[source]` makes the inner error walkable via
    /// `std::error::Error::source` for consumers using
    /// `anyhow`-style chained-error UI.
    ///
    /// **Coverage caveat (per audit pass-2 #74):** `Context`
    /// only wraps `TunerError` — by construction it can't carry
    /// a `Usb(rusb::Error)` or `DeviceLost` from the same
    /// operation. The R82xx filter-calibration path uses this
    /// shape today: failures from the calibration's tuner-side
    /// math get wrapped (`Context { context: "filter
    /// calibration", source: ... }`), but a USB transport error
    /// during the same calibration sequence propagates as a
    /// bare `RtlSdrError::Usb(...)` with no calibration-context
    /// breadcrumb. Consumers building diagnostic UIs should
    /// match on both shapes if they want full coverage of the
    /// "what was the device doing when it failed" question.
    #[error("{context}: {source}")]
    Context {
        context: &'static str,
        #[source]
        source: Box<TunerError>,
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
    /// Recognises [`RtlSdrError::DeviceLost`] (the crate's
    /// internal "we observed disconnect" sentinel — see `read_sync`
    /// and friends) plus the underlying rusb variants that
    /// commonly surface a yanked dongle:
    /// - [`rusb::Error::NoDevice`] — libusb's authoritative
    ///   disconnect signal, fires on the next call after the
    ///   kernel observes the unplug
    /// - [`rusb::Error::Pipe`] — endpoint stall; on Linux this
    ///   commonly surfaces from a mid-flight bulk read at the
    ///   moment the device disappears, before libusb downgrades
    ///   subsequent calls to `NoDevice`
    /// - [`rusb::Error::Io`] — generic transport I/O failure;
    ///   same Linux mid-flight-disconnect surrogate
    ///
    /// Pre-#43 (0.2.0 and earlier) only matched `DeviceLost` and
    /// `NoDevice`, so a reconnect loop using `is_disconnected`
    /// to gate the retry path mistreated `Pipe`/`Io` from a
    /// hot-unplug as transient and waited a full bulk-read cycle
    /// before getting an actionable signal. Per audit pass-2 #43.
    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        matches!(
            self,
            Self::DeviceLost
                | Self::Usb(rusb::Error::NoDevice | rusb::Error::Pipe | rusb::Error::Io)
        )
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

    /// Per audit pass-2 #43: Linux hot-unplug commonly surfaces
    /// `Pipe` / `Io` from a mid-flight bulk read before libusb
    /// downgrades to `NoDevice`. A reconnect-loop consumer using
    /// `is_disconnected` should treat both as disconnect, not
    /// transient.
    #[test]
    fn is_disconnected_recognises_linux_hot_unplug_surrogates() {
        assert!(RtlSdrError::Usb(rusb::Error::Pipe).is_disconnected());
        assert!(RtlSdrError::Usb(rusb::Error::Io).is_disconnected());
    }

    #[test]
    fn is_disconnected_returns_false_for_other_variants() {
        assert!(!RtlSdrError::DeviceBusy.is_disconnected());
        assert!(!RtlSdrError::Usb(rusb::Error::Timeout).is_disconnected());
        assert!(!RtlSdrError::NoTuner.is_disconnected());
        assert!(!RtlSdrError::DeviceNotFound { index: 0 }.is_disconnected());
        assert!(!RtlSdrError::Tuner(TunerError::XtalIsZero).is_disconnected());
        // `Overflow`, `Access`, `Other`, etc. are not Linux
        // disconnect surrogates; pin them as not-disconnect so a
        // future widening doesn't sweep too broadly.
        assert!(!RtlSdrError::Usb(rusb::Error::Overflow).is_disconnected());
        assert!(!RtlSdrError::Usb(rusb::Error::Access).is_disconnected());
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
        assert!(!RtlSdrError::Usb(rusb::Error::Pipe).is_timeout());
        assert!(
            !RtlSdrError::RegisterAccess {
                block: crate::reg::Block::Demod,
                address: 0
            }
            .is_timeout()
        );
    }
}
