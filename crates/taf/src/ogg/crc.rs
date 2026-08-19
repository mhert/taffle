//! The checksum every Ogg page carries.
//!
//! RFC 3533 sums a page with a CRC-32 that is not the one zlib and every other format use: the
//! polynomial is the usual `0x04c11db7`, but the register starts empty rather than full, neither
//! the input nor the output is reflected, and nothing is xored into the result. A page states the
//! sum of its own bytes with those four bytes read as zero.

/// The polynomial RFC 3533 sums pages with, in the usual high-bit-first notation.
const POLYNOMIAL: u32 = 0x04c1_1db7;

/// The entries the byte-at-a-time table holds: one per byte value.
const TABLE_LEN: usize = 256;

/// The bit that leaves the register on every step, and so decides whether the polynomial is
/// folded back in.
const HIGH_BIT: u32 = 0x8000_0000;

/// What each byte value sums to on its own, worked out while the crate compiles.
const TABLE: [u32; TABLE_LEN] = build_table();

/// Builds the table: entry `n` is what the single byte `n` leaves in an empty register.
///
/// The entries are filled in one after another rather than by index, which is the same checked
/// stepping the rest of this crate reads its bytes with.
const fn build_table() -> [u32; TABLE_LEN] {
    let mut table = [0; TABLE_LEN];
    let mut rest: &mut [u32] = &mut table;
    let mut byte: u32 = 0;

    while let Some((entry, tail)) = rest.split_first_mut() {
        *entry = fold(byte << 24);
        rest = tail;
        byte += 1;
    }

    table
}

/// Shifts a register eight places, folding the polynomial back in wherever a set bit falls out.
const fn fold(mut remainder: u32) -> u32 {
    let mut step = 0;

    while step < 8 {
        remainder = if remainder & HIGH_BIT == 0 {
            remainder << 1
        } else {
            (remainder << 1) ^ POLYNOMIAL
        };
        step += 1;
    }

    remainder
}

/// Sums `data` the way RFC 3533 sums an Ogg page.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    crc32_from(0, data)
}

/// Sums `data` into a register that already holds the sum of the bytes before it.
///
/// A page being read stays where it lies, and the four bytes its checksum occupies have to be
/// summed as zeros — so summing one means summing three pieces in a row rather than copying the
/// page somewhere it can be blanked.
pub(crate) fn crc32_from(crc: u32, data: &[u8]) -> u32 {
    data.iter().fold(crc, |crc, &byte| {
        // The byte about to leave the register decides which entry is folded back in, and every
        // byte value has one — so the table never comes up empty.
        let [leaving, ..] = crc.to_be_bytes();
        let entry = TABLE.get(usize::from(leaving ^ byte)).copied().unwrap_or(0);

        (crc << 8) ^ entry
    })
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
    use alloc::vec::Vec;

    const GOLDEN: &[u8] = include_bytes!("../../tests/fixtures/golden-sine.taf");

    /// The audio region's first page: 27 bytes of header, one lacing value, and the 19-byte
    /// `OpusHead` packet.
    const FIRST_PAGE: core::ops::Range<usize> = 4096..4143;

    /// Where a page states its checksum, and how wide it is.
    const CHECKSUM: core::ops::Range<usize> = 22..26;

    /// The checksum summed one bit at a time, the way RFC 3533 words it.
    ///
    /// The table the implementation steps through is an optimization of exactly this loop, so a
    /// table that is off by an entry sums differently from this and the tests below say so.
    fn bitwise(data: &[u8]) -> u32 {
        let mut crc: u32 = 0;

        for &byte in data {
            crc ^= u32::from(byte) << 24;
            for _ in 0..8 {
                crc = if crc & HIGH_BIT == 0 {
                    crc << 1
                } else {
                    (crc << 1) ^ POLYNOMIAL
                };
            }
        }

        crc
    }

    #[test]
    fn sums_an_empty_slice_to_an_empty_register() {
        assert_eq!(crc32(&[]), 0);
    }

    #[test]
    fn sums_nothing_onto_a_register_that_already_holds_something() {
        assert_eq!(crc32_from(0x1234_5678, &[]), 0x1234_5678);
    }

    #[test]
    fn matches_the_check_value_of_this_crc_variant() {
        // The value the CRC catalogue states for `123456789` under these parameters, which is
        // what tells this variant apart from the reflected CRC-32 everything else uses.
        assert_eq!(crc32(b"123456789"), 0x89a1_897f);
    }

    #[test]
    fn reproduces_the_checksum_the_golden_fixture_states() {
        let mut page: [u8; 47] = GOLDEN[FIRST_PAGE].try_into().unwrap();
        let stated = u32::from_le_bytes(page[CHECKSUM].try_into().unwrap());
        page[CHECKSUM].fill(0);

        assert_eq!(stated, 0xe661_88bd, "the checksum the fixture stores");
        assert_eq!(crc32(&page), stated);
    }

    #[test]
    fn sums_what_a_bit_at_a_time_implementation_sums() {
        // Every byte value, fed in at every offset of the register, plus a real page.
        let sweep: Vec<u8> = (0..=u8::MAX).cycle().take(1031).collect();

        for data in [&[][..], b"a", b"123456789", &sweep, &GOLDEN[FIRST_PAGE]] {
            assert_eq!(crc32(data), bitwise(data), "{} bytes", data.len());
        }
    }

    #[test]
    fn sums_in_pieces_what_it_sums_in_one_go() {
        let page = &GOLDEN[FIRST_PAGE];

        for split in [0, 1, 22, 26, page.len()] {
            let (head, tail) = page.split_at(split);

            assert_eq!(
                crc32_from(crc32(head), tail),
                crc32(page),
                "split at {split}"
            );
        }
    }

    #[test]
    fn builds_a_table_of_what_each_byte_sums_to_on_its_own() {
        let table = build_table();

        for (byte, &entry) in table.iter().enumerate() {
            assert_eq!(
                entry,
                bitwise(&[u8::try_from(byte).unwrap()]),
                "byte {byte}"
            );
        }

        // What the crate sums with is what this builder builds, only worked out while the crate
        // compiled rather than while it runs.
        assert_eq!(table, TABLE);

        // The first two entries spell out the polynomial itself: an empty register stays empty,
        // and a single set bit leaves the polynomial behind.
        assert_eq!(TABLE[0], 0);
        assert_eq!(TABLE[1], POLYNOMIAL);
        assert_eq!(TABLE[255], 0xb1f7_40b4);
    }
}
