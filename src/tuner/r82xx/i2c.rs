//! R82XX I2C communication with shadow register optimization.
//!
//! Exact port of the shadow_store, shadow_equal, r82xx_write,
//! r82xx_read, and r82xx_write_reg_mask functions.

use crate::error::{RtlSdrError, TunerError};
use crate::usb;

use super::R82xxPriv;
use super::constants::{NUM_REGS, REG_SHADOW_START, bitrev};

impl R82xxPriv {
    /// Store values in shadow registers.
    ///
    /// Ports `shadow_store`.
    pub(super) fn shadow_store(&mut self, reg: u8, val: &[u8]) {
        let mut r = reg as i32 - i32::from(REG_SHADOW_START);
        let mut offset = 0usize;
        let mut len = val.len() as i32;

        if r < 0 {
            len += r;
            offset = (-r) as usize;
            r = 0;
        }
        if len <= 0 {
            return;
        }
        let r = r as usize;
        if len > (NUM_REGS - r) as i32 {
            len = (NUM_REGS - r) as i32;
        }
        let len = len as usize;
        self.regs[r..r + len].copy_from_slice(&val[offset..offset + len]);
    }

    /// Check if shadow registers match the given values.
    ///
    /// Ports `shadow_equal`.
    pub(super) fn shadow_equal(&self, reg: u8, val: &[u8]) -> bool {
        let r = reg as i32 - i32::from(REG_SHADOW_START);
        let len = val.len() as i32;

        if r < 0 || len < 0 || len > (NUM_REGS as i32 - r) {
            return false;
        }
        let r = r as usize;
        let len = len as usize;
        self.regs[r..r + len] == val[..len]
    }

    /// Write to R82XX registers via I2C with shadow optimization.
    ///
    /// Ports `r82xx_write`. Skips writes if shadow matches.
    /// Splits large writes to respect max_i2c_msg_len.
    pub(super) fn write(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        reg: u8,
        val: &[u8],
    ) -> Result<(), RtlSdrError> {
        // Skip if shadow matches
        if self.shadow_equal(reg, val) {
            return Ok(());
        }

        let buf_capacity = self.buf.len();
        let mut pos = 0usize;
        let mut current_reg = reg;
        let mut remaining = val.len();

        while remaining > 0 {
            let size = i2c_chunk_size(remaining, self.max_i2c_msg_len, buf_capacity);

            // Build I2C message: [reg, data...]
            self.buf[0] = current_reg;
            self.buf[1..1 + size].copy_from_slice(&val[pos..pos + size]);

            let rc = usb::i2c_write(handle, self.i2c_addr, &self.buf[..size + 1])?;
            if rc != size + 1 {
                return Err(TunerError::I2cTransferFailed {
                    operation: "write",
                    got: rc,
                    expected: size + 1,
                }
                .into());
            }

            current_reg += size as u8;
            remaining -= size;
            pos += size;
        }

        // Update shadow only after all writes succeed.
        //
        // **Deliberate divergence from C upstream** (per audit
        // pass-2 #62): C's `r82xx_write` calls `shadow_store`
        // BEFORE the do/while I2C transmit loop
        // (`tuner_r82xx.c:282`). Moving it after gives us a
        // tighter shadow-vs-hardware invariant on first-byte
        // failure (cache reflects nothing, hardware unchanged)
        // at the cost of a different skew on multi-byte partial
        // failure (some bytes in hardware, cache reflects
        // nothing of the burst). The Rust shape is preferable
        // because the multi-byte case requires `max_i2c_msg_len
        // < val.len()` AND a transient I2C failure mid-burst
        // (extremely rare); the all-or-nothing path is the
        // common case.
        //
        // The module doc says "ports `shadow_store`" — that
        // describes function-level fidelity, not call-order
        // fidelity to the bug-for-bug C semantics. This comment
        // exists so future maintainers don't "fix" the order
        // back to C-style.
        self.shadow_store(reg, val);

        Ok(())
    }

    /// Write a single register.
    ///
    /// Ports `r82xx_write_reg`.
    pub(super) fn write_reg(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        reg: u8,
        val: u8,
    ) -> Result<(), RtlSdrError> {
        self.write(handle, reg, &[val])
    }

    /// Read from cached shadow register.
    ///
    /// Ports `r82xx_read_cache_reg`.
    pub(super) fn read_cache_reg(&self, reg: u8) -> Option<u8> {
        let r = reg as i32 - i32::from(REG_SHADOW_START);
        if r >= 0 && (r as usize) < NUM_REGS {
            Some(self.regs[r as usize])
        } else {
            None
        }
    }

    /// Write a register with bit mask (read-modify-write from cache).
    ///
    /// Ports `r82xx_write_reg_mask`.
    pub(super) fn write_reg_mask(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        reg: u8,
        val: u8,
        bit_mask: u8,
    ) -> Result<(), RtlSdrError> {
        let cached = self
            .read_cache_reg(reg)
            .ok_or(TunerError::ShadowCacheMiss { reg })?;

        let new_val = (cached & !bit_mask) | (val & bit_mask);
        self.write(handle, reg, &[new_val])
    }

    /// Read registers from the tuner via I2C.
    ///
    /// Ports `r82xx_read`. Data is bit-reversed per R82XX convention.
    pub(super) fn read(
        &mut self,
        handle: &rusb::DeviceHandle<rusb::GlobalContext>,
        reg: u8,
        out: &mut [u8],
    ) -> Result<(), RtlSdrError> {
        // Write register address
        self.buf[0] = reg;
        let rc = usb::i2c_write(handle, self.i2c_addr, &self.buf[..1])?;
        if rc != 1 {
            return Err(TunerError::I2cTransferFailed {
                operation: "read addr",
                got: rc,
                expected: 1,
            }
            .into());
        }

        // Read data
        let rc = usb::i2c_read(handle, self.i2c_addr, &mut self.buf[1..1 + out.len()])?;
        if rc != out.len() {
            return Err(TunerError::I2cTransferFailed {
                operation: "read data",
                got: rc,
                expected: out.len(),
            }
            .into());
        }

        // Bit-reverse the data
        for (i, byte) in self.buf[1..1 + out.len()].iter().enumerate() {
            out[i] = bitrev(*byte);
        }

        Ok(())
    }
}

/// Number of data bytes for one I2C write chunk.
///
/// `max_msg_len` is clamped to `[2, buf_capacity]` so the
/// [`R82xxPriv::write`] split-write loop is bounded:
/// - At `max_msg_len = 1`, `size = 0` would spin the loop forever
///   (`remaining` never decreases). Clamping the floor to `2`
///   guarantees at least one data byte per chunk.
/// - At `max_msg_len > buf_capacity`, the `self.buf[..size + 1]`
///   slice index would panic. Clamping the ceiling makes the
///   index obviously in-range.
///
/// `R82xxConfig::max_i2c_msg_len` is a `pub` field, so a misconfig
/// is a consumer-reachable footgun even though every in-tree
/// caller passes the safe value `8`. Per audit pass-2 #42.
fn i2c_chunk_size(remaining: usize, max_msg_len: usize, buf_capacity: usize) -> usize {
    let effective_max = max_msg_len.clamp(2, buf_capacity);
    remaining.min(effective_max - 1)
}

#[cfg(test)]
mod tests {
    use super::{NUM_REGS, i2c_chunk_size};

    const BUF_CAP: usize = NUM_REGS + 1;

    #[test]
    fn normal_max_msg_picks_max_msg_minus_one() {
        // max_msg=8 means [reg_byte, 7 data bytes] per chunk.
        assert_eq!(i2c_chunk_size(20, 8, BUF_CAP), 7);
        assert_eq!(i2c_chunk_size(7, 8, BUF_CAP), 7);
    }

    #[test]
    fn remaining_smaller_than_chunk_returns_remaining() {
        assert_eq!(i2c_chunk_size(3, 8, BUF_CAP), 3);
        assert_eq!(i2c_chunk_size(1, 8, BUF_CAP), 1);
    }

    #[test]
    fn max_msg_below_floor_clamps_so_loop_makes_progress() {
        // Per #42: max_msg=1 used to give size=0 → infinite loop.
        // Clamp to 2 so each chunk places at least 1 data byte.
        assert_eq!(i2c_chunk_size(10, 1, BUF_CAP), 1);
        assert_eq!(i2c_chunk_size(10, 0, BUF_CAP), 1);
    }

    #[test]
    fn max_msg_above_buf_capacity_clamps_so_no_oob() {
        // Per #42: max_msg > buf_capacity used to OOB-panic the
        // `self.buf[..size + 1]` slice index. Clamp upward.
        assert_eq!(i2c_chunk_size(100, 1000, BUF_CAP), BUF_CAP - 1);
        assert_eq!(i2c_chunk_size(100, BUF_CAP + 1, BUF_CAP), BUF_CAP - 1);
    }

    /// Pin the no-infinite-loop invariant: any `remaining > 0`
    /// must produce a positive chunk size for any `max_msg_len`.
    #[test]
    fn chunk_size_always_makes_progress_when_remaining_positive() {
        for max_msg in 0..=64 {
            for remaining in 1..=64 {
                let size = i2c_chunk_size(remaining, max_msg, BUF_CAP);
                assert!(
                    size > 0,
                    "no progress: remaining={remaining}, max_msg={max_msg}"
                );
            }
        }
    }
}
