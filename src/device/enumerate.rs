//! Device enumeration and USB string queries.
//!
//! Ports `rtlsdr_get_device_count`, `rtlsdr_get_device_name`,
//! `rtlsdr_get_device_usb_strings`, `rtlsdr_get_index_by_serial`.
//!
//! Plus [`list_devices`] — a Rust-idiomatic collected enumeration
//! that returns one [`DeviceInfo`] per dongle in a single call.

use crate::constants::find_known_device;
use crate::error::RtlSdrError;

/// One entry returned by [`list_devices`] / [`crate::RtlSdrDevice::list`].
///
/// Carries the four pieces of information you can read about a
/// dongle without opening it: its enumeration index, the
/// human-friendly device name from the USB known-devices table
/// (e.g. "Generic RTL2832U OEM"), and the USB descriptor strings
/// (manufacturer / product / serial). The serial string is what
/// you'd hand to [`crate::RtlSdrDevice::builder`] /
/// [`get_index_by_serial`] to open a specific dongle when more
/// than one is plugged in.
///
/// USB string fields fall back to an empty `String` when the
/// descriptor read fails (e.g. permissions, transient bus error)
/// — the entry still appears so you can see something is plugged
/// in even if the strings aren't readable from this process.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceInfo {
    /// Zero-based enumeration index — the same value you'd pass to
    /// [`crate::RtlSdrDevice::open`].
    pub index: u32,
    /// Human-friendly device name from the known-devices table
    /// (e.g. "Realtek RTL2838UHIDIR"). Equivalent to
    /// [`get_device_name`] but already populated on the entry.
    pub name: String,
    /// USB manufacturer descriptor string. May be empty if the
    /// descriptor read failed.
    pub manufacturer: String,
    /// USB product descriptor string. May be empty if the
    /// descriptor read failed.
    pub product: String,
    /// USB serial-number descriptor string. May be empty if the
    /// descriptor read failed (rare in practice — most RTL-SDR
    /// flashes ship with a unique serial). Use
    /// [`crate::RtlSdrDevice::builder`]`.serial(...)` to open by
    /// serial when more than one dongle is plugged in.
    pub serial: String,
}

/// Enumerate all connected RTL-SDR dongles in one call.
///
/// More ergonomic than the count + per-index pair when the caller
/// just wants "tell me what's plugged in." Internally this is
/// [`get_device_count`] plus per-index [`get_device_name`] +
/// [`get_device_usb_strings`], collected into a `Vec`. The
/// returned slice is in enumeration-index order, so
/// `list_devices()[i].index == i as u32` for any `i` in range.
///
/// Returns an empty `Vec` when no devices are present (matches
/// the implicit "count is 0" path of the underlying enumerate).
///
/// # Performance
///
/// This walks the USB device tree and, for each match, *opens*
/// the device briefly to read its USB descriptor strings —
/// strings aren't cached in the bus topology, the kernel has to
/// be asked. Roughly `O(n_dongles)` USB control transfers. Cheap
/// for the common 1-or-2-dongle case but not something to call
/// in a tight loop. Cache the result.
#[must_use]
pub fn list_devices() -> Vec<DeviceInfo> {
    let count = get_device_count();
    (0..count)
        .map(|index| {
            let name = get_device_name(index);
            let (manufacturer, product, serial) = get_device_usb_strings(index)
                .unwrap_or_else(|_| (String::new(), String::new(), String::new()));
            DeviceInfo {
                index,
                name,
                manufacturer,
                product,
                serial,
            }
        })
        .collect()
}

/// Get the number of connected RTL-SDR devices.
///
/// Ports `rtlsdr_get_device_count`. Returns `0` both when no
/// dongles are plugged in and when the USB subsystem itself is
/// unreachable (libusb init failed, permission revoked, etc.) —
/// the latter case is logged at `tracing::warn!` so the
/// distinction isn't fully invisible. Per audit issue #18.
#[must_use]
pub fn get_device_count() -> u32 {
    let mut count = 0u32;
    match rusb::devices() {
        Ok(devices) => {
            for device in devices.iter() {
                if let Ok(dd) = device.device_descriptor() {
                    if find_known_device(dd.vendor_id(), dd.product_id()).is_some() {
                        count += 1;
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!("get_device_count: rusb::devices() failed ({e}); reporting 0 devices");
        }
    }
    count
}

/// Get the name of a device by index.
///
/// Ports `rtlsdr_get_device_name`. Returns the empty string both
/// when the index is out of range and when the USB subsystem is
/// unreachable — same dual-meaning + tracing as
/// [`get_device_count`]. Per audit issue #18.
#[must_use]
pub fn get_device_name(index: u32) -> String {
    let mut count = 0u32;
    match rusb::devices() {
        Ok(devices) => {
            for device in devices.iter() {
                if let Ok(dd) = device.device_descriptor() {
                    if let Some(known) = find_known_device(dd.vendor_id(), dd.product_id()) {
                        if count == index {
                            return known.name.to_string();
                        }
                        count += 1;
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                "get_device_name({index}): rusb::devices() failed ({e}); returning empty name"
            );
        }
    }
    String::new()
}

/// Get USB strings (manufacturer, product, serial) by device index.
///
/// Ports `rtlsdr_get_device_usb_strings`. Opens the device temporarily
/// to read the descriptor strings.
pub fn get_device_usb_strings(index: u32) -> Result<(String, String, String), RtlSdrError> {
    let (device, dd) = find_device_by_index(index)?;
    let handle = device.open()?;

    let manufact = handle
        .read_manufacturer_string_ascii(&dd)
        .unwrap_or_default();
    let product = handle.read_product_string_ascii(&dd).unwrap_or_default();
    let serial = handle
        .read_serial_number_string_ascii(&dd)
        .unwrap_or_default();

    Ok((manufact, product, serial))
}

/// Find a device index by its serial number string.
///
/// Ports `rtlsdr_get_index_by_serial`.
pub fn get_index_by_serial(serial: &str) -> Result<u32, RtlSdrError> {
    lookup_serial(serial, get_device_count(), get_device_usb_strings)
}

/// Pure lookup helper extracted from [`get_index_by_serial`] so the
/// matching algorithm can be unit-tested without depending on real
/// USB enumeration. Per #6.
pub(crate) fn lookup_serial<F>(serial: &str, count: u32, lookup: F) -> Result<u32, RtlSdrError>
where
    F: Fn(u32) -> Result<(String, String, String), RtlSdrError>,
{
    for i in 0..count {
        if let Ok((_, _, dev_serial)) = lookup(i) {
            if dev_serial == serial {
                return Ok(i);
            }
        }
    }

    Err(RtlSdrError::InvalidParameter(format!(
        "no device with serial '{serial}'"
    )))
}

/// Find a USB device by its RTL-SDR index.
pub(crate) fn find_device_by_index(
    index: u32,
) -> Result<(rusb::Device<rusb::GlobalContext>, rusb::DeviceDescriptor), RtlSdrError> {
    let devices = rusb::devices()?;
    let mut count = 0u32;

    for device in devices.iter() {
        if let Ok(dd) = device.device_descriptor() {
            if find_known_device(dd.vendor_id(), dd.product_id()).is_some() {
                if count == index {
                    return Ok((device, dd));
                }
                count += 1;
            }
        }
    }

    Err(RtlSdrError::DeviceNotFound { index })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// Regression for #6: when no devices are plugged in,
    /// `lookup_serial` must return [`RtlSdrError::InvalidParameter`]
    /// with the requested serial in the message — not
    /// [`RtlSdrError::DeviceNotFound(0)`], which lies about both the
    /// failure mode (it's a serial mismatch, not a missing index)
    /// and the input (the user passed a serial, not index 0).
    #[test]
    fn lookup_serial_count_zero_returns_invalid_parameter() {
        let bogus_serial = "nonexistent_serial_for_test_6";
        let lookup_called = Cell::new(false);
        let result = lookup_serial(bogus_serial, 0, |_| {
            lookup_called.set(true);
            Ok((String::new(), String::new(), String::new()))
        });
        assert!(
            !lookup_called.get(),
            "lookup must not be called when count == 0",
        );
        assert!(
            matches!(&result, Err(RtlSdrError::InvalidParameter(msg)) if msg.contains(bogus_serial)),
            "expected InvalidParameter containing serial, got {result:?}",
        );
    }

    #[test]
    fn lookup_serial_finds_match_at_index() {
        let result = lookup_serial("wanted", 3, |i| {
            Ok((
                String::new(),
                String::new(),
                match i {
                    0 => "other_a".to_string(),
                    1 => "wanted".to_string(),
                    _ => "other_b".to_string(),
                },
            ))
        });
        assert_eq!(result.ok(), Some(1));
    }

    #[test]
    fn lookup_serial_no_match_returns_invalid_parameter() {
        let bogus_serial = "wanted";
        let result = lookup_serial(bogus_serial, 2, |_| {
            Ok((String::new(), String::new(), "nope".to_string()))
        });
        assert!(
            matches!(&result, Err(RtlSdrError::InvalidParameter(msg)) if msg.contains(bogus_serial)),
            "expected InvalidParameter containing serial, got {result:?}",
        );
    }

    /// Per-device lookup failures (e.g. permission denied) must not
    /// abort the search — the loop should keep trying subsequent
    /// indices in case the wanted serial is on a later device.
    #[test]
    fn lookup_serial_skips_lookup_errors_and_continues() {
        let result = lookup_serial("wanted", 3, |i| {
            if i == 1 {
                Err(RtlSdrError::Usb(rusb::Error::Access))
            } else {
                Ok((
                    String::new(),
                    String::new(),
                    if i == 2 {
                        "wanted".to_string()
                    } else {
                        "other".to_string()
                    },
                ))
            }
        });
        assert_eq!(result.ok(), Some(2));
    }
}
