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

        self.rate = real_rate;

        // Set tuner bandwidth and update IF frequency
        if let Some(tuner) = &mut self.tuner {
            usb::set_i2c_repeater(&self.handle, true)?;
            let bw = if self.bw > 0 { self.bw } else { self.rate };
            if let Ok(if_freq) = tuner.set_bw(&self.handle, bw, self.rate) {
                // Update IF frequency registers (critical — audit fix #2)
                let _ = self.set_if_freq(if_freq);
                // Retune to apply new IF (audit fix #2)
                if self.freq > 0 {
                    if let Some(tuner) = &mut self.tuner {
                        let _ = tuner.set_freq(&self.handle, self.freq - self.offs_freq);
                    }
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
            r = tuner.set_freq(&self.handle, freq.wrapping_sub(self.offs_freq));
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

        // Based on keenerds 1/f noise measurements
        self.offs_freq = if on {
            (self.rate / 2) * OFFSET_TUNING_MULTIPLIER_NUM / OFFSET_TUNING_MULTIPLIER_DEN
        } else {
            0
        };
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
            let _ = tuner.set_bw(&self.handle, bw, self.rate);
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
    pub fn set_tuner_bandwidth(&mut self, bw: u32) -> Result<(), RtlSdrError> {
        if let Some(tuner) = &mut self.tuner {
            usb::set_i2c_repeater(&self.handle, true)?;
            let actual_bw = if bw > 0 { bw } else { self.rate };
            if let Ok(if_freq) = tuner.set_bw(&self.handle, actual_bw, self.rate) {
                let _ = self.set_if_freq(if_freq);
                if self.freq > 0 {
                    if let Some(tuner) = &mut self.tuner {
                        let _ = tuner.set_freq(&self.handle, self.freq - self.offs_freq);
                    }
                }
            }
            usb::set_i2c_repeater(&self.handle, false)?;
            self.bw = bw;
        }
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
// `dead_code` allow lifts in commit 2 of this branch when the
// three call sites in the impl above start using it. Per #10 plan.
#[allow(dead_code)]
fn freq_minus_offset(freq: u32, offs_freq: u32) -> Result<u32, RtlSdrError> {
    freq.checked_sub(offs_freq).ok_or_else(|| {
        RtlSdrError::InvalidParameter(format!(
            "freq {freq} Hz is below the offset-tuning floor {offs_freq} Hz"
        ))
    })
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
}
