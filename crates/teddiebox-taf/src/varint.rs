//! Minimal protobuf base-128 varint reader.

/// Reads one varint, advancing `pos`. Returns `None` on truncation or overflow.
pub fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    loop {
        if shift >= 64 {
            return None;
        }
        let byte = *buf.get(*pos)?;
        *pos += 1;
        let payload = u64::from(byte & 0x7F);
        // At shift 63, only the payload's lowest bit lands inside a 64-bit
        // result (bit 63); any higher payload bit would need bit 64 or
        // beyond, which `<<` on a u64 would silently discard rather than
        // erroring on. Reject instead of wrapping.
        if shift == 63 && payload > 1 {
            return None;
        }
        result |= payload << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_single_byte_values() {
        let mut pos = 0;
        assert_eq!(read_varint(&[0x01], &mut pos), Some(1));
        assert_eq!(pos, 1);
    }

    #[test]
    fn reads_multi_byte_values() {
        // 300 == 0xAC 0x02
        let mut pos = 0;
        assert_eq!(read_varint(&[0xAC, 0x02], &mut pos), Some(300));
        assert_eq!(pos, 2);
    }

    #[test]
    fn rejects_truncated_input() {
        let mut pos = 0;
        assert_eq!(read_varint(&[0x80], &mut pos), None);
    }

    #[test]
    fn rejects_overlong_encoding() {
        let mut pos = 0;
        assert_eq!(read_varint(&[0x80; 12], &mut pos), None);
    }

    #[test]
    fn rejects_a_final_byte_whose_payload_bits_overrun_64_bits() {
        // Nine continuation bytes carrying zero payload, putting `shift` at
        // 63 for the tenth (final, non-continuation) byte. That byte's
        // 7-bit payload can only contribute its lowest bit (position 63) to
        // a 64-bit result; a payload of 2 needs bit 64, which doesn't
        // exist. Before this was checked, `result |= payload << shift`
        // silently dropped that bit instead of erroring, returning
        // `Some(0)` for an input that has no valid 64-bit representation.
        let bytes = [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02];
        let mut pos = 0;
        assert_eq!(read_varint(&bytes, &mut pos), None);
    }
}
