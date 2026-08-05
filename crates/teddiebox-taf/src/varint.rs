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
        result |= u64::from(byte & 0x7F) << shift;
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
}
