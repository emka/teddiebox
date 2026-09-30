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
        // At shift 63 only the lowest payload bit fits in a u64. `<<` would
        // silently drop higher bits, so reject them.
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
        // Given
        let mut pos = 0;

        // When
        let parsed = read_varint(&[0x01], &mut pos);

        // Then
        assert_eq!(parsed, Some(1));
        assert_eq!(pos, 1);
    }

    #[test]
    fn reads_multi_byte_values() {
        // Given: 300 == 0xAC 0x02
        let mut pos = 0;

        // When
        let parsed = read_varint(&[0xAC, 0x02], &mut pos);

        // Then
        assert_eq!(parsed, Some(300));
        assert_eq!(pos, 2);
    }

    /// At shift 63 only the lowest payload bit fits, and it must: without it
    /// the top half of the range could not be read.
    #[test]
    fn reads_the_largest_value() {
        // Given: nine bytes of seven ones, then the top bit
        let bytes = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01];
        let mut pos = 0;

        // When
        let parsed = read_varint(&bytes, &mut pos);

        // Then
        assert_eq!(parsed, Some(u64::MAX));
    }

    #[test]
    fn rejects_truncated_input() {
        // Given
        let mut pos = 0;

        // When
        let parsed = read_varint(&[0x80], &mut pos);

        // Then
        assert_eq!(parsed, None);
    }

    #[test]
    fn rejects_overlong_encoding() {
        // Given
        let mut pos = 0;

        // When
        let parsed = read_varint(&[0x80; 12], &mut pos);

        // Then
        assert_eq!(parsed, None);
    }

    #[test]
    fn rejects_a_final_byte_whose_payload_bits_overrun_64_bits() {
        // Given: nine continuation bytes with zero payload put the tenth (last)
        // byte at shift 63. Its payload of 2 would need bit 64, so the value
        // does not fit in a u64.
        let bytes = [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02];
        let mut pos = 0;

        // When
        let parsed = read_varint(&bytes, &mut pos);

        // Then
        assert_eq!(parsed, None);
    }
}
