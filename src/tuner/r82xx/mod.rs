//! Rafael Micro R820T/R828D tuner driver.
//!
//! Faithful port of tuner_r82xx.c. Split into sub-modules:
//! - `constants`: Init array, frequency ranges, gain tables
//! - `i2c`: Shadow register I2C communication
//! - `pll`: PLL frequency synthesis

// R820T2/R828D register-table constants — same faithful-port
// rationale as the sibling tuners (`tuner/mod.rs`). Per #630 CR
// round 2.
#[allow(dead_code)]
pub mod constants;
mod i2c;
mod pll;

use crate::error::RtlSdrError;
use crate::tuner::Tuner;

use constants::*;

/// R82XX tuner configuration.
#[derive(Clone, Debug)]
pub struct R82xxConfig {
    pub i2c_addr: u8,
    pub xtal: u32,
    pub rafael_chip: R82xxChip,
    pub max_i2c_msg_len: usize,
    pub use_predetect: bool,
}

/// R82XX tuner private state.
///
/// Ports `struct r82xx_priv` from tuner_r82xx.h.
pub struct R82xxPriv {
    // Config
    pub(crate) i2c_addr: u8,
    pub(crate) xtal: u32,
    pub(crate) rafael_chip: R82xxChip,
    pub(crate) max_i2c_msg_len: usize,
    pub(crate) use_predetect: bool,

    // State
    pub(crate) regs: [u8; NUM_REGS],
    pub(crate) buf: [u8; NUM_REGS + 1],
    pub(crate) xtal_cap_sel: XtalCapValue,
    pub(crate) int_freq: u32,
    pub(crate) fil_cal_code: u8,
    pub(crate) input: u8,
    pub(crate) init_done: bool,
    pub(crate) bw: u32,

    // Manufacturer check callback
    pub(crate) is_blog_v4: bool,
}

impl R82xxPriv {
    /// Create a new R82XX driver from configuration.
    pub fn new(config: &R82xxConfig) -> Self {
        Self {
            i2c_addr: config.i2c_addr,
            xtal: config.xtal,
            rafael_chip: config.rafael_chip,
            max_i2c_msg_len: config.max_i2c_msg_len,
            use_predetect: config.use_predetect,
            regs: [0u8; NUM_REGS],
            buf: [0u8; NUM_REGS + 1],
            xtal_cap_sel: XtalCapValue::HighCap0p,
            int_freq: 0,
            fil_cal_code: 0,
            input: 0,
            init_done: false,
            bw: 0,
            is_blog_v4: false,
        }
    }

    /// Set the Blog V4 detection flag.
    pub fn set_blog_v4(&mut self, is_v4: bool) {
        self.is_blog_v4 = is_v4;
    }

    // --- Internal functions ported from tuner_r82xx.c ---

    /// Set RF mux and tracking filter based on frequency.
    ///
    /// Exact port of `r82xx_set_mux`.
    fn set_mux(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        freq: u32,
    ) -> Result<(), RtlSdrError> {
        let freq_mhz = freq / 1_000_000;

        let range = &FREQ_RANGES[find_freq_range_idx(freq_mhz)];

        // Open Drain
        self.write_reg_mask(handle, 0x17, range.open_d, 0x08)?;

        // RF_MUX, Polymux
        self.write_reg_mask(handle, 0x1a, range.rf_mux_ploy, 0xc3)?;

        // TF BAND
        self.write_reg(handle, 0x1b, range.tf_c)?;

        // XTAL CAP & Drive
        let val = match self.xtal_cap_sel {
            XtalCapValue::LowCap30p | XtalCapValue::LowCap20p => range.xtal_cap20p | 0x08,
            XtalCapValue::LowCap10p => range.xtal_cap10p | 0x08,
            XtalCapValue::HighCap0p => range.xtal_cap0p,
            XtalCapValue::LowCap0p => range.xtal_cap0p | 0x08,
        };
        self.write_reg_mask(handle, 0x10, val, 0x0b)?;

        self.write_reg_mask(handle, 0x08, 0x00, 0x3f)?;
        self.write_reg_mask(handle, 0x09, 0x00, 0x3f)?;

        Ok(())
    }

    /// Configure system frequency parameters.
    ///
    /// Exact port of `r82xx_sysfreq_sel` (using default/DVB-T path).
    fn sysfreq_sel(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    ) -> Result<(), RtlSdrError> {
        // Default DVB-T 8M settings
        let mixer_top: u8 = 0x24;
        let lna_top: u8 = 0xe5;
        let lna_vth_l: u8 = 0x53;
        let mixer_vth_l: u8 = 0x75;
        let air_cable1_in: u8 = 0x00;
        let cable2_in: u8 = 0x00;
        let cp_cur: u8 = 0x38;
        let div_buf_cur: u8 = 0x30;
        let filter_cur: u8 = 0x40;
        let lna_discharge: u8 = 14;

        if self.use_predetect {
            self.write_reg_mask(handle, 0x06, 0x40, 0x40)?;
        }

        self.write_reg_mask(handle, 0x1d, lna_top, 0xc7)?;
        self.write_reg_mask(handle, 0x1c, mixer_top, 0xf8)?;
        self.write_reg(handle, 0x0d, lna_vth_l)?;
        self.write_reg(handle, 0x0e, mixer_vth_l)?;

        self.input = air_cable1_in;

        self.write_reg_mask(handle, 0x05, air_cable1_in, 0x60)?;
        self.write_reg_mask(handle, 0x06, cable2_in, 0x08)?;
        self.write_reg_mask(handle, 0x11, cp_cur, 0x38)?;
        self.write_reg_mask(handle, 0x17, div_buf_cur, 0x30)?;
        self.write_reg_mask(handle, 0x0a, filter_cur, 0x60)?;

        // Non-analog TV path (digital)
        self.write_reg_mask(handle, 0x1d, 0, 0x38)?;
        self.write_reg_mask(handle, 0x1c, 0, 0x04)?;
        self.write_reg_mask(handle, 0x06, 0, 0x40)?;
        self.write_reg_mask(handle, 0x1a, 0x30, 0x30)?;

        self.write_reg_mask(handle, 0x1d, 0x18, 0x38)?;
        self.write_reg_mask(handle, 0x1c, mixer_top, 0x04)?;
        self.write_reg_mask(handle, 0x1e, lna_discharge, 0x1f)?;
        self.write_reg_mask(handle, 0x1a, 0x20, 0x30)?;

        Ok(())
    }

    /// Set TV standard and perform filter calibration.
    ///
    /// Exact port of `r82xx_set_tv_standard` (BW < 6MHz / SDR path).
    fn set_tv_standard(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    ) -> Result<(), RtlSdrError> {
        let if_khz: u32 = 3570;
        let filt_cal_lo: u32 = 56000;
        let filt_gain: u8 = 0x10;
        let img_r: u8 = 0x00;
        let filt_q: u8 = 0x10;
        let hp_cor: u8 = 0x6b;
        let ext_enable: u8 = 0x60;
        let loop_through: u8 = 0x01;
        let lt_att: u8 = 0x00;
        let flt_ext_widest: u8 = 0x00;
        let polyfil_cur: u8 = 0x60;

        // Initialize shadow registers
        self.regs[..NUM_INIT_REGS].copy_from_slice(&R82XX_INIT_ARRAY);

        // Init Flag & Xtal_check Result
        self.write_reg_mask(handle, 0x0c, 0x00, 0x0f)?;

        // Version
        self.write_reg_mask(handle, 0x13, VER_NUM, 0x3f)?;

        // For LT Gain test (non-analog TV)
        self.write_reg_mask(handle, 0x1d, 0x00, 0x38)?;

        self.int_freq = if_khz * 1000;

        // Filter calibration (always needed for SDR use)
        for _ in 0..2 {
            self.write_reg_mask(handle, 0x0b, hp_cor, 0x60)?;
            self.write_reg_mask(handle, 0x0f, 0x04, 0x04)?;
            self.write_reg_mask(handle, 0x10, 0x00, 0x03)?;

            // map_err preserves the "during filter calibration"
            // context — the inner Err already names the freq.
            // Pre-#11 this was a `set_pll(...)?` + `if !self.has_lock`
            // post-check; #11 removed the field, so the post-check
            // disappears and the freq lives in the Err message.
            self.set_pll(handle, filt_cal_lo * 1000)
                .map_err(|e| RtlSdrError::Tuner(format!("filter calibration: {e}")))?;

            // Start/stop trigger
            self.write_reg_mask(handle, 0x0b, 0x10, 0x10)?;
            self.write_reg_mask(handle, 0x0b, 0x00, 0x10)?;
            self.write_reg_mask(handle, 0x0f, 0x00, 0x04)?;

            // Read calibration result
            let mut data = [0u8; 5];
            self.read(handle, 0x00, &mut data)?;

            self.fil_cal_code = data[4] & 0x0f;
            if self.fil_cal_code != 0 && self.fil_cal_code != 0x0f {
                break;
            }
        }

        if self.fil_cal_code == 0x0f {
            self.fil_cal_code = 0;
        }

        self.write_reg_mask(handle, 0x0a, filt_q | self.fil_cal_code, 0x1f)?;
        self.write_reg_mask(handle, 0x0b, hp_cor, 0xef)?;
        self.write_reg_mask(handle, 0x07, img_r, 0x80)?;
        self.write_reg_mask(handle, 0x06, filt_gain, 0x30)?;
        self.write_reg_mask(handle, 0x1e, ext_enable, 0x60)?;
        self.write_reg_mask(handle, 0x05, loop_through, 0x80)?;
        self.write_reg_mask(handle, 0x1f, lt_att, 0x80)?;
        self.write_reg_mask(handle, 0x0f, flt_ext_widest, 0x80)?;
        self.write_reg_mask(handle, 0x19, polyfil_cur, 0x60)?;

        self.bw = 0;

        Ok(())
    }

    /// Standby — put tuner in low-power mode.
    ///
    /// Exact port of `r82xx_standby`.
    pub fn standby(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    ) -> Result<(), RtlSdrError> {
        if !self.init_done {
            return Ok(());
        }

        self.write_reg(handle, 0x06, 0xb1)?;
        self.write_reg(handle, 0x05, 0xa0)?;
        self.write_reg(handle, 0x07, 0x3a)?;
        self.write_reg(handle, 0x08, 0x40)?;
        self.write_reg(handle, 0x09, 0xc0)?;
        self.write_reg(handle, 0x0a, 0x36)?;
        self.write_reg(handle, 0x0c, 0x35)?;
        self.write_reg(handle, 0x0f, 0x68)?;
        self.write_reg(handle, 0x11, 0x03)?;
        self.write_reg(handle, 0x17, 0xf4)?;
        self.write_reg(handle, 0x19, 0x0c)?;

        Ok(())
    }
}

impl Tuner for R82xxPriv {
    fn set_xtal(&mut self, xtal: u32) {
        self.xtal = xtal;
    }

    fn init(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    ) -> Result<(), RtlSdrError> {
        self.xtal_cap_sel = XtalCapValue::HighCap0p;

        // Initialize registers
        self.regs = [0u8; NUM_REGS];
        self.write(handle, 0x05, &R82XX_INIT_ARRAY)?;

        self.set_tv_standard(handle)?;
        self.sysfreq_sel(handle)?;

        self.init_done = true;
        Ok(())
    }

    fn exit(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
    ) -> Result<(), RtlSdrError> {
        self.standby(handle)
    }

    fn set_freq(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        freq: u32,
    ) -> Result<(), RtlSdrError> {
        // RTL-SDR Blog V4 HF upconversion
        let upconvert_freq = if self.is_blog_v4 && freq < 28_800_000 {
            freq.saturating_add(28_800_000)
        } else {
            freq
        };

        let lo_freq = upconvert_freq.saturating_add(self.int_freq);

        self.set_mux(handle, lo_freq)?;
        // Pre-#11 set_pll returned Ok(()) on lock failure and set
        // `has_lock = false`, requiring this post-check. Now the
        // failure is propagated as Err — the inner message names
        // the lo_freq value.
        self.set_pll(handle, lo_freq)?;

        // RTL-SDR Blog V4 band switching and notch filter logic
        if self.is_blog_v4 {
            let open_d = if freq <= 2_200_000
                || (freq >= 85_000_000 && freq <= 112_000_000)
                || (freq >= 172_000_000 && freq <= 242_000_000)
            {
                0x00
            } else {
                0x08
            };
            self.write_reg_mask(handle, 0x17, open_d, 0x08)?;

            let band = if freq <= 28_800_000 {
                HF
            } else if freq < 250_000_000 {
                VHF
            } else {
                UHF
            };

            if band != self.input {
                self.input = band;

                let cable_2_in = if band == HF { 0x08 } else { 0x00 };
                self.write_reg_mask(handle, 0x06, cable_2_in, 0x08)?;

                // Control upconverter GPIO switch on newer Blog V4 batches
                // (audit fix #3 — GPIO 5 for upconverter switch)
                crate::usb::set_gpio_output(handle, 5)?;
                crate::usb::set_gpio_bit(handle, 5, cable_2_in == 0)?;

                let cable_1_in = if band == VHF { 0x40 } else { 0x00 };
                self.write_reg_mask(handle, 0x05, cable_1_in, 0x40)?;

                let air_in = if band == UHF { 0x00 } else { 0x20 };
                self.write_reg_mask(handle, 0x05, air_in, 0x20)?;
            }
        } else if self.rafael_chip == R82xxChip::R828D {
            // Standard R828D: switch at 345 MHz
            let air_cable1_in = if freq > 345_000_000 { 0x00 } else { 0x60 };

            if air_cable1_in != self.input {
                self.input = air_cable1_in;
                self.write_reg_mask(handle, 0x05, air_cable1_in, 0x60)?;
            }
        }

        Ok(())
    }

    fn set_bw(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        bw: u32,
        _sample_rate: u32,
    ) -> Result<u32, RtlSdrError> {
        #[allow(clippy::similar_names)]
        let bw = bw as i32;

        let (reg_0a, reg_0b, int_freq) = if bw > 7_000_000 {
            // BW: 8 MHz
            (0x10_u8, 0x0b_u8, 4_570_000_u32)
        } else if bw > 6_000_000 {
            // BW: 7 MHz
            (0x10, 0x2a, 4_570_000)
        } else if bw > IF_LOW_PASS_BW_TABLE[0] + FILT_HP_BW1 + FILT_HP_BW2 {
            // BW: 6 MHz
            (0x10, 0x6b, 3_570_000)
        } else {
            let mut reg_0b: u8 = 0x80;
            let mut int_freq: u32 = 2_300_000;
            let mut real_bw: i32 = 0;
            let mut bw = bw;

            if bw > IF_LOW_PASS_BW_TABLE[0] + FILT_HP_BW1 {
                bw -= FILT_HP_BW2;
                int_freq += FILT_HP_BW2 as u32;
                real_bw += FILT_HP_BW2;
            } else {
                reg_0b |= 0x20;
            }

            if bw > IF_LOW_PASS_BW_TABLE[0] {
                bw -= FILT_HP_BW1;
                int_freq += FILT_HP_BW1 as u32;
                real_bw += FILT_HP_BW1;
            } else {
                reg_0b |= 0x40;
            }

            // Find low-pass filter
            let i = find_if_lpf_idx(bw);
            reg_0b |= (15 - i) as u8;
            real_bw += IF_LOW_PASS_BW_TABLE[i];

            int_freq -= (real_bw / 2) as u32;

            (0x00, reg_0b, int_freq)
        };

        self.int_freq = int_freq;

        self.write_reg_mask(handle, 0x0a, reg_0a, 0x10)?;
        self.write_reg_mask(handle, 0x0b, reg_0b, 0xef)?;

        Ok(self.int_freq)
    }

    fn set_gain(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        gain: i32,
    ) -> Result<(), RtlSdrError> {
        // Manual gain mode
        let mut total_gain = 0;
        let mut mix_index: u8 = 0;
        let mut lna_index: u8 = 0;

        // LNA auto off
        self.write_reg_mask(handle, 0x05, 0x10, 0x10)?;

        // Mixer auto off
        self.write_reg_mask(handle, 0x07, 0, 0x10)?;

        // Read current state
        let mut data = [0u8; 4];
        self.read(handle, 0x00, &mut data)?;

        // Set fixed VGA gain (16.3 dB)
        self.write_reg_mask(handle, 0x0c, 0x08, 0x9f)?;

        for _ in 0..15 {
            if total_gain >= gain {
                break;
            }
            // Try LNA step first
            if (lna_index as usize) < R82XX_LNA_GAIN_STEPS.len() - 1 {
                let step = R82XX_LNA_GAIN_STEPS[lna_index as usize + 1];
                if step > 0 {
                    lna_index += 1;
                    total_gain += step;
                }
            }

            if total_gain >= gain {
                break;
            }
            // Then mixer step — skip negative steps (e.g., index 15 = -8 dB)
            if (mix_index as usize) < R82XX_MIXER_GAIN_STEPS.len() - 1 {
                let step = R82XX_MIXER_GAIN_STEPS[mix_index as usize + 1];
                if step > 0 {
                    mix_index += 1;
                    total_gain += step;
                }
            }
        }

        // Set LNA gain
        self.write_reg_mask(handle, 0x05, lna_index, 0x0f)?;

        // Set Mixer gain
        self.write_reg_mask(handle, 0x07, mix_index, 0x0f)?;

        Ok(())
    }

    fn set_gain_mode(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        manual: bool,
    ) -> Result<(), RtlSdrError> {
        if manual {
            // LNA auto off
            self.write_reg_mask(handle, 0x05, 0x10, 0x10)?;
            // Mixer auto off
            self.write_reg_mask(handle, 0x07, 0, 0x10)?;
            // Fixed VGA gain (16.3 dB)
            self.write_reg_mask(handle, 0x0c, 0x08, 0x9f)?;
        } else {
            // LNA auto on
            self.write_reg_mask(handle, 0x05, 0, 0x10)?;
            // Mixer auto on
            self.write_reg_mask(handle, 0x07, 0x10, 0x10)?;
            // Fixed VGA gain (26.5 dB)
            self.write_reg_mask(handle, 0x0c, 0x0b, 0x9f)?;
        }
        Ok(())
    }
}

/// Pick the [`FREQ_RANGES`] entry covering `freq_mhz`. Returns the
/// index of the largest entry whose `freq` is `<= freq_mhz`,
/// clamping to `0` for the (impossible-for-`u32`) below-table case.
///
/// Extracted from [`R82xxPriv::set_mux`] for unit testability and to
/// fix a Rust-vs-C off-by-one in the original loop shape (see #5).
/// The C upstream uses `for (i = 0; i < N-1; i++) { if (...) break; }`
/// which leaves `i = N-1` after natural completion; the original
/// Rust port assigned `range_idx = i` inside the loop body and so
/// left `range_idx = N-2` after natural completion, silently selecting
/// the wrong entry above the topmost boundary. Using
/// [`<[T]>::partition_point`] sidesteps that whole loop-end-semantics
/// hazard since the table is sorted ascending by `freq`.
fn find_freq_range_idx(freq_mhz: u32) -> usize {
    FREQ_RANGES
        .partition_point(|r| r.freq <= freq_mhz)
        .saturating_sub(1)
}

/// Pick the [`IF_LOW_PASS_BW_TABLE`] entry for the requested
/// post-residual bandwidth `bw`. Returns the index of the narrowest
/// entry that still encompasses `bw`, falling back to the narrowest
/// available filter when `bw` is below the table's minimum.
///
/// Same Rust-vs-C off-by-one fix as [`find_freq_range_idx`]; the
/// table is sorted descending so the predicate matches "still wide
/// enough" entries (which form a prefix). See #5.
fn find_if_lpf_idx(bw: i32) -> usize {
    IF_LOW_PASS_BW_TABLE
        .partition_point(|&v| v >= bw)
        .saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_r82xx_priv_new() {
        let config = R82xxConfig {
            i2c_addr: 0x34,
            xtal: 28_800_000,
            rafael_chip: R82xxChip::R820T,
            max_i2c_msg_len: 8,
            use_predetect: false,
        };
        let priv_ = R82xxPriv::new(&config);
        assert_eq!(priv_.i2c_addr, 0x34);
        assert_eq!(priv_.xtal, 28_800_000);
        assert!(!priv_.init_done);
    }

    #[test]
    fn test_freq_range_lookup() {
        // 100 MHz should hit the range starting at 100
        let freq_mhz = 100;
        let range_idx = find_freq_range_idx(freq_mhz);
        assert!(FREQ_RANGES[range_idx].freq <= freq_mhz);
    }

    // --- find_freq_range_idx edge-case tests (regression for #5) ---
    //
    // FREQ_RANGES is 21 entries, sorted ascending by `freq`:
    // [0]=0, [1]=50, [2]=55, ..., [9]=100, ..., [19]=588, [20]=650.

    #[test]
    fn find_freq_range_idx_at_minimum_picks_first() {
        assert_eq!(find_freq_range_idx(0), 0);
    }

    #[test]
    fn find_freq_range_idx_below_first_boundary_picks_first() {
        // FREQ_RANGES[1].freq == 50; freq_mhz < 50 still maps to [0].
        assert_eq!(find_freq_range_idx(49), 0);
    }

    #[test]
    fn find_freq_range_idx_at_exact_boundary_picks_that_entry() {
        // freq_mhz == entry.freq selects that entry, not the previous one.
        assert_eq!(find_freq_range_idx(50), 1);
        assert_eq!(find_freq_range_idx(100), 9);
        assert_eq!(find_freq_range_idx(450), 18);
    }

    #[test]
    fn find_freq_range_idx_just_below_top_boundary_picks_second_to_last() {
        // FREQ_RANGES[19].freq == 588, [20].freq == 650; 649 < 650 → idx 19.
        assert_eq!(find_freq_range_idx(649), 19);
    }

    /// Regression for #5: at and above the topmost boundary
    /// (650 MHz), the loop must select the last entry, not the
    /// second-to-last. The original `for i in 0..N-1` loop assigned
    /// `range_idx = i` inside the body and so left `range_idx = N-2`
    /// after natural completion. The C upstream's `for (i = 0;
    /// i < N-1; i++) { if (...) break; }` leaves `i = N-1` after
    /// natural completion, which selects the last entry.
    ///
    /// Today this is masked because FREQ_RANGES rows 19 and 20 are
    /// byte-identical except for the `freq` sentinel — but any future
    /// field divergence between the rows would silently misbehave at
    /// the upper edge.
    #[test]
    fn find_freq_range_idx_at_top_boundary_picks_last() {
        assert_eq!(find_freq_range_idx(650), FREQ_RANGES.len() - 1);
    }

    #[test]
    fn find_freq_range_idx_above_top_picks_last() {
        assert_eq!(find_freq_range_idx(700), FREQ_RANGES.len() - 1);
        assert_eq!(find_freq_range_idx(1500), FREQ_RANGES.len() - 1);
    }

    // --- find_if_lpf_idx edge-case tests (regression for #5) ---
    //
    // IF_LOW_PASS_BW_TABLE is 10 entries, sorted DESCENDING:
    // [0]=1_700_000, [1]=1_600_000, ..., [5]=900_000, [6]=700_000,
    // ..., [9]=350_000.

    #[test]
    fn find_if_lpf_idx_above_widest_picks_widest() {
        assert_eq!(find_if_lpf_idx(2_000_000), 0);
    }

    #[test]
    fn find_if_lpf_idx_at_widest_exact_picks_widest() {
        assert_eq!(find_if_lpf_idx(1_700_000), 0);
    }

    #[test]
    fn find_if_lpf_idx_between_table_entries_picks_wider() {
        // [5] == 900_000, [6] == 700_000; 800k between them picks the
        // wider filter that still encompasses the requested bw.
        assert_eq!(find_if_lpf_idx(800_000), 5);
    }

    /// Regression for #5: at and below the narrowest entry
    /// (350 kHz), the loop must select the last (narrowest) entry,
    /// not the second-to-last. Same Rust-vs-C off-by-one as
    /// `find_freq_range_idx_at_top_boundary_picks_last`. Affects
    /// sub-MHz tuner bandwidth selection.
    #[test]
    fn find_if_lpf_idx_at_narrowest_exact_picks_last() {
        assert_eq!(find_if_lpf_idx(350_000), IF_LOW_PASS_BW_TABLE.len() - 1);
    }

    #[test]
    fn find_if_lpf_idx_below_narrowest_picks_last() {
        assert_eq!(find_if_lpf_idx(200_000), IF_LOW_PASS_BW_TABLE.len() - 1);
        assert_eq!(find_if_lpf_idx(1), IF_LOW_PASS_BW_TABLE.len() - 1);
    }
}
