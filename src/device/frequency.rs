//! Frequency control — sample rate, center frequency, PPM correction, offset tuning.
//!
//! Ports `rtlsdr_set_sample_rate`, `rtlsdr_set_center_freq`,
//! `rtlsdr_set_freq_correction`, `rtlsdr_set_offset_tuning`.

use crate::error::RtlSdrError;
use crate::reg::TunerType;
use crate::usb;

/// Offset tuning multiplier: 1.7x half-rate, based on keenerds 1/f noise measurements.
const OFFSET_TUNING_MULTIPLIER_NUM: u32 = 170;
const OFFSET_TUNING_MULTIPLIER_DEN: u32 = 100;

use super::RtlSdrDevice;

impl RtlSdrDevice {
    /// Set the sample rate in Hz.
    ///
    /// Ports `rtlsdr_set_sample_rate`. Valid ranges: 225001-300000, 900001-3200000.
    pub fn set_sample_rate(&mut self, samp_rate: u32) -> Result<(), RtlSdrError> {
        if (samp_rate <= 225_000)
            || (samp_rate > 3_200_000)
            || (samp_rate > 300_000 && samp_rate <= 900_000)
        {
            return Err(RtlSdrError::InvalidSampleRate(samp_rate));
        }

        let rsamp_ratio =
            ((f64::from(self.rtl_xtal) * (1u64 << 22) as f64) / f64::from(samp_rate)) as u32;
        let rsamp_ratio = rsamp_ratio & 0x0fff_fffc;

        let real_rsamp_ratio = rsamp_ratio | ((rsamp_ratio & 0x0800_0000) << 1);
        let real_rate =
            (f64::from(self.rtl_xtal) * (1u64 << 22) as f64 / f64::from(real_rsamp_ratio)) as u32;

        if samp_rate != real_rate {
            tracing::debug!("Exact sample rate: {} Hz", real_rate);
        }

        // Preflight: when offset tuning is active and the current
        // center freq is set, the trailing
        // `set_offset_tuning(true)?` at the bottom of this function
        // would fail (post-#10) if the new sample rate's floor put
        // us at or below the current freq — but by then `self.rate`
        // and several device registers would already be updated
        // (partial-apply). Catch the bad combination before any
        // state mutation. Per #10 round 2 (Code Rabbit).
        if self.offs_freq > 0 && self.freq > 0 {
            let new_floor = offset_tuning_floor(real_rate);
            if self.freq <= new_floor {
                return Err(RtlSdrError::InvalidParameter(format!(
                    "cannot set sample rate to {real_rate} Hz: current freq {} Hz would fall \
                     at or below the new offset-tuning floor {new_floor} Hz \
                     (≈ 0.85 × sample_rate); disable offset tuning or tune higher first",
                    self.freq,
                )));
            }
        }

        self.rate = real_rate;

        // Set tuner bandwidth and update IF frequency.
        //
        // All three inner failures (set_bw, set_if_freq, retune) are
        // best-effort — a sample-rate change has already partially
        // applied to the demod side by the time we reach the demod
        // register writes below, so propagating an Err here would
        // leave the device half-configured AND fail to complete the
        // demod-side updates. Pre-#9 these were silent; post-#9
        // they're logged at `tracing::warn!` for visibility while
        // preserving the best-effort propagation shape. Per audit
        // slice C I-4 / #9.
        if let Some(tuner) = &mut self.tuner {
            usb::set_i2c_repeater(&self.handle, true)?;
            let bw = if self.bw > 0 { self.bw } else { self.rate };
            match tuner.set_bw(&self.handle, bw, self.rate) {
                Ok(if_freq) => {
                    if let Err(e) = self.set_if_freq(if_freq) {
                        tracing::warn!("set_sample_rate: IF freq update failed: {e}");
                    }
                    if self.freq > 0 {
                        if let Ok(adjusted) = freq_minus_offset(self.freq, self.offs_freq) {
                            if let Some(tuner) = &mut self.tuner {
                                if let Err(e) = tuner.set_freq(&self.handle, adjusted) {
                                    tracing::warn!(
                                        "set_sample_rate: tuner retune failed: {e}; \
                                         resetting cached freq to 0 (parity with \
                                         set_center_freq's audit-fix-#11)"
                                    );
                                    // Match set_center_freq's audit-fix-#11
                                    // pattern: tuner is now on an unknown freq,
                                    // so cache that uncertainty rather than
                                    // claiming the old value. Per audit slice
                                    // C I-5 / #9.
                                    self.freq = 0;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("set_sample_rate: tuner set_bw failed: {e}");
                }
            }
            usb::set_i2c_repeater(&self.handle, false)?;
        }

        let tmp = (rsamp_ratio >> 16) as u16;
        usb::demod_write_reg(&self.handle, 1, 0x9f, tmp, 2)?;
        let tmp = (rsamp_ratio & 0xffff) as u16;
        usb::demod_write_reg(&self.handle, 1, 0xa1, tmp, 2)?;

        self.set_sample_freq_correction(self.corr)?;

        // Reset demod (bit 3, soft_rst)
        usb::demod_write_reg(&self.handle, 1, 0x01, 0x14, 1)?;
        usb::demod_write_reg(&self.handle, 1, 0x01, 0x10, 1)?;

        // Recalculate offset frequency if offset tuning is enabled
        if self.offs_freq > 0 {
            self.set_offset_tuning(true)?;
        }

        Ok(())
    }

    /// Set center frequency in Hz.
    ///
    /// Ports `rtlsdr_set_center_freq`.
    pub fn set_center_freq(&mut self, freq: u32) -> Result<(), RtlSdrError> {
        let mut r = Err(RtlSdrError::NoTuner);

        if self.direct_sampling != 0 {
            r = self.set_if_freq(freq);
        } else if let Some(tuner) = &mut self.tuner {
            usb::set_i2c_repeater(&self.handle, true)?;
            // Subtract the offset-tuning floor before programming
            // the tuner. Pre-#10 this used `wrapping_sub`, silently
            // producing a huge u32 when freq < offs_freq; now
            // returns a friendly `InvalidParameter` instead.
            r = freq_minus_offset(freq, self.offs_freq)
                .and_then(|adjusted| tuner.set_freq(&self.handle, adjusted));
            usb::set_i2c_repeater(&self.handle, false)?;
        }

        match r {
            Ok(()) => {
                self.freq = freq;
            }
            Err(ref _e) => {
                // Reset freq on error (audit fix #11)
                self.freq = 0;
            }
        }

        r
    }

    /// Set frequency correction in PPM.
    ///
    /// Ports `rtlsdr_set_freq_correction`.
    pub fn set_freq_correction(&mut self, ppm: i32) -> Result<(), RtlSdrError> {
        if self.corr == ppm {
            return Ok(());
        }

        self.corr = ppm;

        self.set_sample_freq_correction(ppm)?;

        // Propagate corrected xtal to tuner (audit fix #4)
        let corrected_xtal = self.get_tuner_xtal();
        if let Some(tuner) = &mut self.tuner {
            tuner.set_xtal(corrected_xtal);
        }

        if self.freq > 0 {
            self.set_center_freq(self.freq)?;
        }

        Ok(())
    }

    /// Set offset tuning mode.
    ///
    /// Ports `rtlsdr_set_offset_tuning`. Not supported for R82XX tuners.
    ///
    /// # Offset-tuning floor
    ///
    /// When enabled, the LO is offset below the requested center
    /// frequency by ≈ `0.85 × sample_rate` (keenerds' 1/f noise
    /// measurement; specifically `(rate / 2) × 1.7`). This means a
    /// sample rate of 2.4 Msps yields a floor of ≈ 2.04 MHz; you
    /// cannot tune below the floor while offset tuning is on.
    /// Set the center frequency to a value above the floor *before*
    /// enabling offset tuning, or expect [`RtlSdrError::InvalidParameter`].
    ///
    /// # Errors
    ///
    /// - [`RtlSdrError::InvalidParameter`] for R82XX tuners (the IC
    ///   doesn't support offset tuning).
    /// - [`RtlSdrError::InvalidParameter`] when called in direct
    ///   sampling mode.
    /// - [`RtlSdrError::InvalidParameter`] when enabling offset
    ///   tuning while the current center frequency is at or below
    ///   the computed floor — pre-#10 the IF registers were
    ///   silently written but the tuner stayed on the old frequency
    ///   (partial-state hazard). Per audit slice C I-6.
    pub fn set_offset_tuning(&mut self, on: bool) -> Result<(), RtlSdrError> {
        if self.tuner_type == TunerType::R820T || self.tuner_type == TunerType::R828D {
            return Err(RtlSdrError::InvalidParameter(
                "offset tuning not supported for R82XX tuners".to_string(),
            ));
        }

        if self.direct_sampling != 0 {
            return Err(RtlSdrError::InvalidParameter(
                "offset tuning not available in direct sampling mode".to_string(),
            ));
        }

        // Based on keenerds 1/f noise measurements; see
        // [`offset_tuning_floor`].
        let new_offs_freq = if on {
            offset_tuning_floor(self.rate)
        } else {
            0
        };

        // Refuse to enable offset tuning when the current center
        // frequency is at or below the computed floor — the
        // tuner can't be retuned to `freq - offs_freq` and the
        // pre-#10 code would silently leave the device with the
        // IF written but the tuner on the old frequency. Validate
        // before any state mutation so a rejected call is a no-op.
        // Per audit slice C I-6.
        if on && self.freq > 0 && self.freq <= new_offs_freq {
            return Err(RtlSdrError::InvalidParameter(format!(
                "cannot enable offset tuning: current freq {} Hz is at or below the \
                 computed floor {} Hz (≈ 0.85 × sample_rate); tune above the floor first",
                self.freq, new_offs_freq,
            )));
        }

        self.offs_freq = new_offs_freq;
        self.set_if_freq(self.offs_freq)?;

        if let Some(tuner) = &mut self.tuner {
            usb::set_i2c_repeater(&self.handle, true)?;
            let bw = if on {
                2 * self.offs_freq
            } else if self.bw > 0 {
                self.bw
            } else {
                self.rate
            };
            // Best-effort: the BW update is secondary to the IF
            // change above. Log on failure rather than silently
            // swallowing. Per audit slice C I-4 / #9.
            if let Err(e) = tuner.set_bw(&self.handle, bw, self.rate) {
                tracing::warn!("set_offset_tuning: tuner set_bw failed: {e}");
            }
            usb::set_i2c_repeater(&self.handle, false)?;
        }

        if self.freq > self.offs_freq {
            self.set_center_freq(self.freq)?;
        }

        Ok(())
    }

    /// Set tuner bandwidth in Hz.
    ///
    /// Ports `rtlsdr_set_tuner_bandwidth`.
    ///
    /// # Errors
    ///
    /// - The tuner's `set_bw` error if the IC rejects the
    ///   bandwidth. The cached bandwidth is **not** updated on
    ///   failure (so subsequent reads never lie about a setting
    ///   the hardware never accepted). Per audit slice C I-3 / #9.
    /// - Any [`RtlSdrError::Usb`] propagated from the I2C-repeater
    ///   toggling.
    ///
    /// Best-effort failures of the secondary IF-frequency update
    /// and tuner-retune are logged at `tracing::warn!` but don't
    /// invalidate the bandwidth setting itself — the bandwidth is
    /// the primary side effect this method is named for.
    pub fn set_tuner_bandwidth(&mut self, bw: u32) -> Result<(), RtlSdrError> {
        let actual_bw = if bw > 0 { bw } else { self.rate };
        let rate = self.rate;

        // No tuner attached → no-op (matches existing behavior).
        let Some(tuner) = self.tuner.as_mut() else {
            return Ok(());
        };

        usb::set_i2c_repeater(&self.handle, true)?;
        let bw_result = tuner.set_bw(&self.handle, actual_bw, rate);
        // tuner borrow released after the expression above.

        let if_freq = match bw_result {
            Ok(f) => f,
            Err(e) => {
                // Tuner rejected the bandwidth. Restore I2C-repeater
                // state (best-effort — the propagated error is more
                // informative than any cleanup failure) and DO NOT
                // update self.bw. Per audit slice C I-3 / #9.
                if let Err(close_err) = usb::set_i2c_repeater(&self.handle, false) {
                    tracing::warn!(
                        "set_tuner_bandwidth: I2C repeater restore failed after set_bw error: {close_err}"
                    );
                }
                return Err(e);
            }
        };

        // Best-effort: update IF-frequency demod registers. A failure
        // here doesn't invalidate the BW setting — log + continue.
        if let Err(e) = self.set_if_freq(if_freq) {
            tracing::warn!("set_tuner_bandwidth: IF freq update failed: {e}");
        }

        // Best-effort: retune the tuner to apply the new IF. Skip
        // when below the offset-tuning floor (pre-#10 panic-shape;
        // freq_minus_offset returns Err which we swallow silently
        // here since it's an explicit "not applicable" signal, not
        // a transport failure).
        if self.freq > 0 {
            if let Ok(adjusted) = freq_minus_offset(self.freq, self.offs_freq) {
                if let Some(tuner) = self.tuner.as_mut() {
                    if let Err(e) = tuner.set_freq(&self.handle, adjusted) {
                        tracing::warn!(
                            "set_tuner_bandwidth: tuner retune failed: {e}; \
                             resetting cached freq to 0 (parity with set_sample_rate \
                             and set_center_freq's audit-fix-#11)"
                        );
                        // Match the parity decision: tuner is now on
                        // an unknown freq, so cache that uncertainty
                        // rather than claiming the old value.
                        self.freq = 0;
                    }
                }
            }
        }

        usb::set_i2c_repeater(&self.handle, false)?;
        self.bw = bw;
        Ok(())
    }
}

/// Subtract the offset-tuning floor from a target frequency.
///
/// When offset tuning is enabled the tuner is programmed at
/// `freq - offs_freq` so the LO sits below the requested center
/// frequency by the configured offset (≈ 0.85 × sample_rate when
/// offset tuning is on; `0` when it's off, making this a no-op).
///
/// # Errors
///
/// Returns [`RtlSdrError::InvalidParameter`] when `freq < offs_freq`.
/// Historically the C upstream (and this crate before #10) did
/// unsigned-wrapping subtraction here, producing a huge `u32` that
/// the tuner rejected with an opaque "frequency out of range"
/// error. Catching it before the tuner call yields a friendly
/// typed error naming the floor and the requested value.
//
fn freq_minus_offset(freq: u32, offs_freq: u32) -> Result<u32, RtlSdrError> {
    freq.checked_sub(offs_freq).ok_or_else(|| {
        RtlSdrError::InvalidParameter(format!(
            "freq {freq} Hz is below the offset-tuning floor {offs_freq} Hz"
        ))
    })
}

/// Compute the offset-tuning floor for a given sample rate.
///
/// `≈ 0.85 × rate`, matching keenerds' 1/f noise measurements (the
/// `OFFSET_TUNING_MULTIPLIER_*` constants encode `1.7 × half-rate`).
/// When offset tuning is enabled the tuner is programmed at
/// `freq - floor`, so `freq` must be strictly above this value or
/// `freq_minus_offset` returns an error.
///
/// Used by both [`RtlSdrDevice::set_offset_tuning`] (to decide
/// whether to accept the toggle) and [`RtlSdrDevice::set_sample_rate`]
/// (to preflight the post-rate-change `set_offset_tuning(true)?`
/// call so the rate update can't half-apply).
fn offset_tuning_floor(rate: u32) -> u32 {
    (rate / 2) * OFFSET_TUNING_MULTIPLIER_NUM / OFFSET_TUNING_MULTIPLIER_DEN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freq_minus_offset_above_floor_subtracts() {
        assert_eq!(
            freq_minus_offset(100_000_000, 2_720_000).ok(),
            Some(97_280_000)
        );
    }

    #[test]
    fn freq_minus_offset_at_floor_returns_zero() {
        assert_eq!(freq_minus_offset(2_720_000, 2_720_000).ok(), Some(0));
    }

    #[test]
    fn freq_minus_offset_with_zero_offset_is_identity() {
        assert_eq!(freq_minus_offset(100_000_000, 0).ok(), Some(100_000_000));
    }

    /// Per #10: below-floor inputs must return a friendly typed
    /// error naming the requested freq and the floor — not silently
    /// wrap to a huge u32 (audit's documented hazard).
    #[test]
    fn freq_minus_offset_below_floor_returns_invalid_parameter() {
        let result = freq_minus_offset(100_000, 2_720_000);
        assert!(
            matches!(
                &result,
                Err(RtlSdrError::InvalidParameter(msg))
                    if msg.contains("100000") && msg.contains("2720000")
            ),
            "expected InvalidParameter naming both values, got {result:?}",
        );
    }

    /// Per #10 round 2 (Code Rabbit): `offset_tuning_floor`
    /// produces `(rate / 2) × 1.7 = 0.85 × rate`. Pin a few
    /// known sample rates so a future tweak to the
    /// `OFFSET_TUNING_MULTIPLIER_*` constants has to reckon with
    /// the user-visible floor values (used in error messages and
    /// in `set_sample_rate`'s preflight check).
    #[test]
    fn offset_tuning_floor_at_2_4msps_is_2_04mhz() {
        // (2_400_000 / 2) * 170 / 100 = 1_200_000 * 1.7 = 2_040_000
        assert_eq!(offset_tuning_floor(2_400_000), 2_040_000);
    }

    #[test]
    fn offset_tuning_floor_at_2_048msps_is_1_7408mhz() {
        // (2_048_000 / 2) * 170 / 100 = 1_024_000 * 1.7 = 1_740_800
        assert_eq!(offset_tuning_floor(2_048_000), 1_740_800);
    }

    #[test]
    fn offset_tuning_floor_zero_rate_is_zero() {
        // Defensive: rate=0 should not panic (integer division ok).
        assert_eq!(offset_tuning_floor(0), 0);
    }
}
