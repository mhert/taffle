//! Base-128 varints, the one number encoding the header's protobuf message uses.

use super::HeaderError;

/// The bits a varint byte carries; the eighth bit says whether another byte follows.
const PAYLOAD_BITS: u8 = 0x7f;

/// The bit every byte of a varint sets except the last.
const CONTINUATION: u8 = 0x80;

/// The most bytes a 64-bit varint can occupy: `ceil(64 / 7)`.
const MAX_LEN: usize = 10;

/// Decodes the varint at the front of `buf` into the value it carries and the bytes it occupied.
///
/// Bytes after the varint are left alone, so callers walking a message advance by the returned
/// length. Over-long encodings of small values are accepted, as protobuf readers generally do;
/// only values that no longer fit 64 bits are rejected.
///
/// # Errors
///
/// - [`HeaderError::TruncatedField`] if `buf` ends before the varint's last byte.
/// - [`HeaderError::VarintOverflow`] if the varint carries more than 64 bits.
pub(super) fn decode_u64(buf: &[u8]) -> Result<(u64, usize), HeaderError> {
    let mut value: u64 = 0;
    let mut shift: u32 = 0;

    for (index, &byte) in buf.iter().take(MAX_LEN).enumerate() {
        let payload = u64::from(byte & PAYLOAD_BITS);
        // Ten bytes at seven bits each is one bit more than a `u64` holds, and `shift` stays
        // below 64 because the loop stops at ten bytes — so only the last byte can lose bits.
        let shifted = payload << shift;
        if shifted >> shift != payload {
            return Err(HeaderError::VarintOverflow);
        }
        value |= shifted;

        if byte & CONTINUATION == 0 {
            return Ok((value, index + 1));
        }
        shift += 7;
    }

    if buf.len() >= MAX_LEN {
        Err(HeaderError::VarintOverflow)
    } else {
        Err(HeaderError::TruncatedField)
    }
}

/// Decodes the varint at the front of `buf` into a `u32`.
///
/// # Errors
///
/// - [`HeaderError::TruncatedField`] if `buf` ends before the varint's last byte.
/// - [`HeaderError::VarintOverflow`] if the value does not fit a `u32`.
pub(super) fn decode_u32(buf: &[u8]) -> Result<(u32, usize), HeaderError> {
    let (value, len) = decode_u64(buf)?;
    let value = u32::try_from(value).map_err(|_| HeaderError::VarintOverflow)?;

    Ok((value, len))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::header::HeaderError;

    #[test]
    fn decodes_a_two_byte_varint() {
        assert_eq!(decode_u32(&[0x96, 0x01]), Ok((150, 2)));
    }

    #[test]
    fn decodes_a_single_byte_varint_and_leaves_the_rest() {
        assert_eq!(decode_u64(&[0x00, 0xff]), Ok((0, 1)));
        assert_eq!(decode_u64(&[0x7f, 0xff]), Ok((127, 1)));
    }

    #[test]
    fn decodes_the_widest_u64() {
        let widest = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];

        assert_eq!(decode_u64(&widest), Ok((u64::MAX, 10)));
    }

    #[test]
    fn decodes_the_widest_u32() {
        assert_eq!(
            decode_u32(&[0xff, 0xff, 0xff, 0xff, 0x0f]),
            Ok((u32::MAX, 5))
        );
    }

    #[test]
    fn rejects_a_six_byte_u32_varint() {
        assert_eq!(
            decode_u32(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x01]),
            Err(HeaderError::VarintOverflow)
        );
    }

    #[test]
    fn rejects_a_varint_carrying_more_than_sixty_four_bits() {
        let too_wide = [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02];

        assert_eq!(decode_u64(&too_wide), Err(HeaderError::VarintOverflow));
    }

    #[test]
    fn rejects_a_varint_longer_than_ten_bytes() {
        let unterminated = [0x80; 11];

        assert_eq!(decode_u64(&unterminated), Err(HeaderError::VarintOverflow));
    }

    #[test]
    fn rejects_a_varint_that_the_buffer_ends_in_the_middle_of() {
        assert_eq!(decode_u64(&[]), Err(HeaderError::TruncatedField));
        assert_eq!(decode_u64(&[0x80, 0x80]), Err(HeaderError::TruncatedField));
    }
}
