//! Builder for [`RtlSdrDevice::open`] with named selectors.

use crate::error::RtlSdrError;

use super::RtlSdrDevice;
use super::enumerate::get_index_by_serial;

/// Selector for which dongle to open.
#[derive(Debug, Clone)]
enum Selector {
    /// Open the dongle at this enumeration index.
    Index(u32),
    /// Open the dongle whose USB serial-number descriptor matches.
    Serial(String),
}

impl Default for Selector {
    fn default() -> Self {
        // Matches `RtlSdrDevice::open(0)` — first dongle plugged in.
        Self::Index(0)
    }
}

/// Builder for [`RtlSdrDevice::open`] / [`RtlSdrDevice::builder`].
///
/// Lets callers express device selection by serial without
/// threading [`super::enumerate::get_index_by_serial`] into their
/// own code:
///
/// ```no_run
/// # use librtlsdr_rs::{RtlSdrDevice, RtlSdrError};
/// # fn main() -> Result<(), RtlSdrError> {
/// // Open by index (default — same as RtlSdrDevice::open(0)):
/// let dev = RtlSdrDevice::builder().open()?;
///
/// // Open by index explicitly:
/// let dev = RtlSdrDevice::builder().index(1).open()?;
///
/// // Open by serial (multi-dongle setups):
/// let dev = RtlSdrDevice::builder().serial("00000001").open()?;
/// # Ok(())
/// # }
/// ```
///
/// When neither selector is set, the builder defaults to
/// `index(0)` — picking the first dongle plugged in. The last
/// selector wins; e.g. `.index(0).serial("X")` opens by serial.
///
/// The builder is `Clone` so callers staging open-attempts (retry
/// loops, fallback paths) can fork a partially-configured builder
/// without rebuilding from scratch.
#[derive(Debug, Clone, Default)]
pub struct RtlSdrDeviceBuilder {
    selector: Selector,
}

impl RtlSdrDeviceBuilder {
    /// Open the dongle at the given enumeration index.
    ///
    /// Same value space as [`RtlSdrDevice::open`] / the free
    /// [`super::enumerate::get_device_count`] return value. The
    /// builder defaults to `index(0)`, so calling this is only
    /// necessary when you want a specific other dongle by
    /// position (or for self-documenting clarity).
    #[must_use]
    pub fn index(mut self, index: u32) -> Self {
        self.selector = Selector::Index(index);
        self
    }

    /// Open the dongle whose USB serial-number descriptor matches.
    ///
    /// Resolved at [`Self::open`] time via
    /// [`super::enumerate::get_index_by_serial`] — the resolution
    /// is one USB descriptor read per dongle until the match is
    /// found. The serial string is the same one
    /// [`super::enumerate::get_device_usb_strings`] /
    /// [`super::enumerate::list_devices`] return; most RTL-SDR
    /// dongles ship with a non-empty serial preprogrammed at the
    /// factory.
    ///
    /// `serial` accepts anything `Into<String>` so you can pass a
    /// `&str`, `String`, or borrowed config value without an
    /// explicit conversion at the call site.
    #[must_use]
    pub fn serial(mut self, serial: impl Into<String>) -> Self {
        self.selector = Selector::Serial(serial.into());
        self
    }

    /// Open the device with the configured selector.
    ///
    /// # Errors
    ///
    /// Failure shape depends on which selector you chose:
    ///
    /// **Index path (`.index(N)` or default):**
    /// - [`RtlSdrError::DeviceNotFound`] when the index is out of
    ///   range OR no devices are plugged in at all.
    ///
    /// **Serial path (`.serial(s)`):**
    /// - [`RtlSdrError::InvalidParameter`] when no plugged-in
    ///   dongle's serial matches — including the no-devices case
    ///   (`get_index_by_serial` returns `InvalidParameter` rather
    ///   than `DeviceNotFound` when the enumerate is empty,
    ///   because the failure mode is "this serial isn't here,"
    ///   not "this index is out of range." Per #6 / 0.1.1 fix.)
    ///   The error message names the requested serial string for
    ///   diagnosis.
    ///
    /// **Both paths:**
    /// - Any [`RtlSdrError`] [`RtlSdrDevice::open`] would return on
    ///   the resolved index (USB transport errors, baseband-init
    ///   failure, unknown tuner, etc.).
    ///
    /// In particular: `DeviceNotFound` only fires on the index
    /// path. If you want a single error code that covers
    /// "couldn't find a device by either selector," handle both
    /// `DeviceNotFound` and `InvalidParameter` at the call site.
    pub fn open(self) -> Result<RtlSdrDevice, RtlSdrError> {
        let index = match self.selector {
            Selector::Index(i) => i,
            Selector::Serial(s) => get_index_by_serial(&s)?,
        };
        RtlSdrDevice::open(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_builder_uses_index_zero() {
        let b = RtlSdrDeviceBuilder::default();
        assert!(matches!(b.selector, Selector::Index(0)));
    }

    #[test]
    fn last_selector_wins() {
        let b = RtlSdrDeviceBuilder::default()
            .index(2)
            .serial("ABCD")
            .index(5);
        assert!(matches!(b.selector, Selector::Index(5)));

        let b = RtlSdrDeviceBuilder::default().index(7).serial("WXYZ");
        assert!(matches!(b.selector, Selector::Serial(ref s) if s == "WXYZ"));
    }

    #[test]
    fn builder_is_clone() {
        let b = RtlSdrDeviceBuilder::default().serial("base");
        let _b2 = b.clone();
        let _b3 = b.clone().index(3);
    }
}
