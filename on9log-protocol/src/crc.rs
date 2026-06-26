//! CRC-16-CCITT (CCITT-FALSE) matching `esp_stdio_log_vfs.c`.
//!
//! Initial value `0xffff`, polynomial `0x1021`, no reflection, no final xor.
//! The firmware appends the little-endian result before SLIP-escaping it.

/// Pre-computed CRC-16-CCITT (CCITT-FALSE) table using the IBM-3740 polynomial.
const CRC16_CCITT_FALSE: crc::Crc<u16> = crc::Crc::<u16>::new(&crc::CRC_16_IBM_3740);

/// Compute CRC-16-CCITT over `header` followed by `payload`.
pub fn compute(header: &[u8], payload: &[u8]) -> u16 {
    let mut digest = CRC16_CCITT_FALSE.digest();
    digest.update(header);
    digest.update(payload);
    digest.finalize()
}

/// Verify a frame: `crc_bytes` is the little-endian trailing checksum.
pub fn verify(header: &[u8], payload: &[u8], crc_bytes: &[u8; 2]) -> bool {
    let expected = compute(header, payload);
    expected == u16::from_le_bytes(*crc_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    // "123456789" with CRC-16/CCITT-FALSE = 0x29B1 (standard check value).
    #[test]
    fn ccitt_false_check_value() {
        assert_eq!(compute(b"123456789", b""), 0x29b1);
    }
}
