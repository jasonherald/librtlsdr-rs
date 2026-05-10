//! R82XX PLL frequency synthesis and lock detection.
//!
//! Exact port of `r82xx_set_pll` from tuner_r82xx.c.

use crate::error::{RtlSdrError, TunerError};

use super::R82xxPriv;
use super::constants::{R82xxChip, REG_SHADOW_START};

/// Apply a masked value to a register byte.
#[inline]
fn mask_reg8(reg: u8, val: u8, mask: u8) -> u8 {
    (reg & !mask) | (val & mask)
}

impl R82xxPriv {
    /// Set the PLL to the given frequency in Hz.
    ///
    /// Exact port of `r82xx_set_pll`. Calculates the VCO divider,
    /// programs the PLL registers, and checks for lock.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub(super) fn set_pll(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        freq: u32,
    ) -> Result<(), RtlSdrError> {
        let pll_ref = self.xtal;
        // Guard: a zero crystal frequency would div-by-zero in the
        // `vco_div = ... / (2 * pll_ref)` calculation below. Caught
        // explicitly with a typed error rather than letting it
        // panic. Per audit slice D / #11.
        if pll_ref == 0 {
            return Err(TunerError::XtalIsZero.into());
        }

        let vco_min: u32 = 1_770_000; // kHz
        let vco_max: u32 = vco_min * 2;
        let freq_khz = freq.saturating_add(500) / 1000;

        // Set PLL autotune = 128kHz
        self.write_reg_mask(handle, 0x1a, 0x00, 0x0c)?;

        // Copy regs 0x10 to 0x16 from shadow
        let mut regs = [0u8; 7];
        let shadow_offset = (0x10 - REG_SHADOW_START) as usize;
        regs.copy_from_slice(&self.regs[shadow_offset..shadow_offset + 7]);

        let refdiv2: u8 = 0;
        regs[0] = mask_reg8(regs[0], refdiv2, 0x10);

        // Set VCO current = 100
        regs[2] = mask_reg8(regs[2], 0x80, 0xe0);

        // Calculate divider
        let mut mix_div: u32 = 2;
        let mut div_num: u8 = 0;

        while mix_div <= 64 {
            // Widened to u64 so the multiplication stays
            // safe-by-construction outside the in-spec range.
            // Today `freq_khz ≤ 4.29M kHz` and `mix_div ≤ 64`,
            // so the u32 product (max ~274M) fits comfortably —
            // but if `mix_div` ever grows past 67, silent
            // wraparound. Per audit pass-2 #64.
            let mix_freq = u64::from(freq_khz) * u64::from(mix_div);
            if mix_freq >= u64::from(vco_min) && mix_freq < u64::from(vco_max) {
                let mut div_buf = mix_div;
                while div_buf > 2 {
                    div_buf >>= 1;
                    div_num += 1;
                }
                break;
            }
            mix_div <<= 1;
        }

        // Check that we found a valid divider
        if mix_div > 64 {
            return Err(TunerError::PllProgrammingFailed {
                backend: "R82xx",
                freq_hz: freq,
                reason: "no valid VCO divider",
            }
            .into());
        }

        // Read back and check VCO fine tune
        let mut data = [0u8; 5];
        self.read(handle, 0x00, &mut data)?;

        let vco_power_ref: u8 = if self.rafael_chip == R82xxChip::R828D {
            1
        } else {
            2
        };

        let vco_fine_tune = (data[4] & 0x30) >> 4;

        if vco_fine_tune > vco_power_ref {
            div_num = div_num.saturating_sub(1);
        } else if vco_fine_tune < vco_power_ref {
            div_num += 1;
        }

        regs[0] = mask_reg8(regs[0], div_num << 5, 0xe0);

        let vco_freq = u64::from(freq) * u64::from(mix_div);

        // Calculate nint and sdm:
        // vco_div = int( (pll_ref + 65536 * vco_freq) / (2 * pll_ref) )
        let vco_div = (u64::from(pll_ref) + 65536 * vco_freq) / (2 * u64::from(pll_ref));
        let nint = (vco_div / 65536) as u32;
        let sdm = (vco_div % 65536) as u32;

        // Split the nint-range check into two diagnostics so a
        // diagnostic-driven debugging session can tell which
        // boundary failed (low-frequency unreachable vs
        // high-frequency unreachable). Per audit pass-2 #64.
        if nint < 13 {
            return Err(TunerError::PllProgrammingFailed {
                backend: "R82xx",
                freq_hz: freq,
                reason: "PLL nint below 13 (frequency too low at this divider)",
            }
            .into());
        }
        let nint_max = (128 / u32::from(vco_power_ref)) - 1;
        if nint > nint_max {
            return Err(TunerError::PllProgrammingFailed {
                backend: "R82xx",
                freq_hz: freq,
                reason: "PLL nint above max (frequency too high at this divider)",
            }
            .into());
        }

        // u8 truncation casts: `nint <= nint_max` (max 63 for
        // R820T at vco_power_ref=2; max 127 for R828D at
        // vco_power_ref=1), so `(nint - 13) / 4` fits in u8 by
        // a wide margin. The debug_assert pins the implicit
        // bound so a future tweak to the upper guard above
        // can't silently make this truncate. Per audit pass-2 #64.
        debug_assert!(nint < 13 + 4 * 64, "ni cast assumes nint - 13 < 256");
        let ni = ((nint - 13) / 4) as u8;
        let si = (nint - 4 * u32::from(ni) - 13) as u8;

        regs[4] = ni + (si << 6);

        // pw_sdm
        let val = if sdm == 0 { 0x08 } else { 0x00 };
        regs[2] = mask_reg8(regs[2], val, 0x08);

        regs[5] = (sdm & 0xff) as u8;
        regs[6] = (sdm >> 8) as u8;

        self.write(handle, 0x10, &regs)?;

        // Check PLL lock (try twice)
        let mut locked = false;
        let mut data3 = [0u8; 3];
        for i in 0..2 {
            self.read(handle, 0x00, &mut data3)?;
            if data3[2] & 0x40 != 0 {
                locked = true;
                break;
            }

            if i == 0 {
                // Increase VCO current on first failure
                self.write_reg_mask(handle, 0x12, 0x60, 0xe0)?;
            }
        }

        if !locked {
            // Pre-#11 this returned Ok(()) and set `self.has_lock =
            // false`, requiring every caller to remember to check
            // the field. New callers who forgot would silently tune
            // to a wrong frequency. Now propagate as Err so the
            // typed error path is the only outcome — matches the
            // sibling tuners (E4K returns Err on lock failure).
            // Per audit slice D I-5 / #11.
            return Err(TunerError::PllNotLocked { freq_hz: freq }.into());
        }

        // Set PLL autotune = 8kHz
        self.write_reg_mask(handle, 0x1a, 0x08, 0x08)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::mask_reg8;

    #[test]
    fn test_mask_reg8() {
        assert_eq!(mask_reg8(0xff, 0x00, 0x0f), 0xf0);
        assert_eq!(mask_reg8(0x00, 0xff, 0x0f), 0x0f);
        assert_eq!(mask_reg8(0xaa, 0x55, 0xff), 0x55);
        assert_eq!(mask_reg8(0xaa, 0x55, 0x00), 0xaa);
    }
}
