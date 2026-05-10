//! Tuner gain control.
//!
//! Ports `rtlsdr_set_tuner_gain`, `rtlsdr_set_tuner_gain_mode`,
//! `rtlsdr_set_tuner_if_gain`, `rtlsdr_set_agc_mode`.

use crate::error::RtlSdrError;
use crate::usb;

use super::RtlSdrDevice;

impl RtlSdrDevice {
    /// Set tuner gain in tenths of dB.
    ///
    /// Ports `rtlsdr_set_tuner_gain`.
    pub fn set_tuner_gain(&mut self, gain: i32) -> Result<(), RtlSdrError> {
        if let Some(tuner) = &mut self.tuner {
            usb::set_i2c_repeater(&self.handle, true)?;
            let result = tuner.set_gain(&self.handle, gain);
            // Capture the close result without `?` so a transient
            // close failure doesn't suppress the cache update for
            // a `set_gain` that already succeeded against the
            // hardware. CodeRabbit on PR #80.
            let close_result = usb::set_i2c_repeater(&self.handle, false);

            if result.is_ok() {
                self.gain = gain;
            }
            // Pre-#46 (0.2.1 and earlier) reset `self.gain = 0`
            // on the error path. But `0` is a *valid* tuner gain
            // (it's the first entry in `R82XX_GAINS`), so the
            // reset made `tuner_gain()` lie that gain was 0 dB
            // when in practice the hardware likely still held
            // the previous setting. Now: leave the cache alone
            // on `Err` — the cached value continues to reflect
            // the last *successful* setting, matching the
            // contract `tuner_gain()` documents. Per audit
            // pass-2 #46.

            // Propagate either error (close error first since
            // the underlying I2C-repeater state is "off" is the
            // invariant the next call assumes; a leaked `true`
            // is the more dangerous condition).
            close_result?;
            result
        } else {
            Ok(())
        }
    }

    /// Set tuner gain mode.
    ///
    /// Ports `rtlsdr_set_tuner_gain_mode`.
    /// `manual = true` for manual gain, `false` for automatic.
    pub fn set_tuner_gain_mode(&mut self, manual: bool) -> Result<(), RtlSdrError> {
        if let Some(tuner) = &mut self.tuner {
            tracing::info!(
                "set_tuner_gain_mode: {}",
                if manual { "manual" } else { "automatic (AGC)" },
            );
            usb::set_i2c_repeater(&self.handle, true)?;
            let result = tuner.set_gain_mode(&self.handle, manual);
            usb::set_i2c_repeater(&self.handle, false)?;
            result
        } else {
            Ok(())
        }
    }

    /// Set the gain of one of the tuner's IF stages, in tenths of
    /// dB.
    ///
    /// Ports `rtlsdr_set_tuner_if_gain`. Upstream dispatches to the
    /// active tuner's `set_if_gain` callback; all tuners other than
    /// the E4000 implement that as a no-op. Our `Tuner` trait has a
    /// matching no-op default, so this method forwards unconditionally
    /// and the non-E4000 modules silently return Ok. Returns Ok with
    /// no side effects when no tuner is attached (mirrors the
    /// `set_tuner_gain` / `set_tuner_gain_mode` convention above).
    ///
    /// `stage` is 1-based (1 through 6 on the E4000). `gain` is
    /// signed tenths of dB — the same unit the rtl_tcp wire
    /// protocol uses for command `0x06`.
    pub fn set_tuner_if_gain(&mut self, stage: i32, gain: i32) -> Result<(), RtlSdrError> {
        if let Some(tuner) = &mut self.tuner {
            usb::set_i2c_repeater(&self.handle, true)?;
            let result = tuner.set_if_gain(&self.handle, stage, gain);
            usb::set_i2c_repeater(&self.handle, false)?;
            result
        } else {
            Ok(())
        }
    }

    /// Set RTL2832 AGC mode.
    ///
    /// Ports `rtlsdr_set_agc_mode`. Writes demod-page-0 register
    /// `0x19` whole — see [`crate::RtlSdrDevice::set_testmode`]'s
    /// "Shared-register caveat" for the AGC ↔ testmode interaction
    /// hazard (per audit issue #18). Set AGC *after* any
    /// `set_testmode(false)` you intend to keep effective.
    pub fn set_agc_mode(&self, on: bool) -> Result<(), RtlSdrError> {
        tracing::info!("set_agc_mode: {}", if on { "on" } else { "off" });
        usb::demod_write_reg(&self.handle, 0, 0x19, if on { 0x25 } else { 0x05 }, 1)
    }
}
