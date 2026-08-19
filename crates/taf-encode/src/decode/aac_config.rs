//! The one repair an AAC configuration gets before a decoder is built from it.
//!
//! # What is wrong with these files
//!
//! An AAC track's shape is not in the container but in an `AudioSpecificConfig`, a bit-packed
//! description the MP4 carries in its `esds` atom. Its first fields are five bits of object type,
//! four of sample rate index and four of channel configuration; for the object types in ordinary
//! use a `GASpecificConfig` follows, opening with one bit of frame length, one bit saying the
//! stream *depends on a core coder*, and — when that bit is set — the fourteen bits of core coder
//! delay it then has to state.
//!
//! Every audiobook this crate was written for states exactly two bytes there:
//!
//! ```text
//! 0x12 0x12   00010 0100 0010 0 1 0
//!             │     │    │    │ │ └─ one bit left, and fourteen have to follow
//!             │     │    │    │ └─── dependsOnCoreCoder
//!             │     │    │    └───── frameLengthFlag: 1024 samples
//!             │     │    └────────── channel configuration 2: stereo
//!             │     └─────────────── sample rate index 4: 44 100 Hz
//!             └───────────────────── object type 2: AAC Low Complexity
//! ```
//!
//! The flag is set and the delay it announces does not fit — the configuration ends one bit later.
//! It is a shape Audible's AAX files carry through every conversion of them: the books here were
//! converted by the `OpenAudible` tool and remuxed by ffmpeg, which copies a codec's configuration
//! through byte for byte rather than rewriting it. Eleven of eleven m4b files on the machine this was
//! written on state `0x12 0x12`.
//!
//! Players decode them because their bit readers run off the end of a configuration rather than
//! failing on it, and because the delay is meaningless to a Low Complexity decoder, which has no
//! core coder to be delayed against. symphonia's reader stops at the end and reports an
//! unexpected end of bitstream, so the whole file fails to open — over one vestigial bit.
//!
//! # Why clearing it is safe
//!
//! A stream that truly depends on a core coder *must* state the fourteen bits of delay behind the
//! flag. A configuration with no room for them was therefore never such a stream, whatever its
//! flag says — so clearing the flag cannot change the meaning of a configuration that was written
//! correctly, because no correctly written configuration can meet the condition. Everything else
//! is left exactly as the file states it: a well-formed configuration that sets the flag and does
//! carry its delay is passed on untouched, and so is one that never set the flag.

/// The configuration to build a decoder from instead of this one, when this one states a
/// dependency it has no room to describe. `None` leaves the file's own configuration in place,
/// which is what every well-formed one gets.
pub(super) fn repaired(config: &[u8]) -> Option<Vec<u8>> {
    /// The object type this is about: the one a decoder here can decode, and the one every m4b is
    /// written in.
    const LOW_COMPLEXITY: u8 = 2;
    /// The sample rate index that means the rate is spelled out in twenty-four bits behind it,
    /// which moves every field this counts on.
    const RATE_IS_SPELLED_OUT: u8 = 15;
    /// The flag, as the second byte holds it.
    const FLAG: u8 = 0b10;
    /// How long a configuration has to be, in bits, for the delay behind the flag to fit into it:
    /// five bits of object type, four of sample rate index, four of channel configuration, one of
    /// frame length, the flag itself, and the fourteen of delay it announces.
    const THE_DELAY_FITS_FROM: usize = 29;

    let (&opening, rest) = config.split_first()?;
    let (&flags, tail) = rest.split_first()?;

    let object_type = opening >> 3;
    let rate_index = ((opening & 0b111) << 1) | (flags >> 7);
    if object_type != LOW_COMPLEXITY || rate_index == RATE_IS_SPELLED_OUT {
        return None;
    }
    if flags & FLAG == 0 {
        return None;
    }
    // A configuration long enough to hold the delay is one this has nothing to say about, whether
    // it holds a delay or something else entirely.
    if config.len().saturating_mul(8) >= THE_DELAY_FITS_FROM {
        return None;
    }

    let mut repaired = vec![opening, flags & !FLAG];
    repaired.extend_from_slice(tail);

    Some(repaired)
}

#[cfg(test)]
mod tests {
    use super::repaired;

    /// AAC Low Complexity, 44 100 Hz, stereo, 1024 samples a frame, depending on nothing — the
    /// canonical two bytes, and what a repair has to produce.
    const CANONICAL: [u8; 2] = [0x12, 0x10];

    /// The same, with the core coder dependency set and nothing behind it.
    const VESTIGIAL: [u8; 2] = [0x12, 0x12];

    #[test]
    fn a_dependency_that_cannot_be_described_is_cleared() {
        assert_eq!(repaired(&VESTIGIAL), Some(CANONICAL.to_vec()));
    }

    #[test]
    fn a_configuration_that_states_no_dependency_is_left_alone() {
        assert_eq!(repaired(&CANONICAL), None);
    }

    #[test]
    fn a_dependency_a_third_byte_cannot_describe_either_is_cleared() {
        // Twenty-four bits: five past the flag, and the delay needs fourteen.
        let three_bytes = [0x12, 0x12, 0x80];

        assert_eq!(repaired(&three_bytes), Some(vec![0x12, 0x10, 0x80]));
    }

    #[test]
    fn a_dependency_with_room_for_its_delay_is_left_alone() {
        // Four bytes: the same opening, the flag set, and fourteen bits of delay behind it with a
        // bit to spare — which is what a stream that really depends on a core coder states.
        let with_delay = [0x12, 0x12, 0x34, 0x56];

        assert_eq!(repaired(&with_delay), None);
    }

    #[test]
    fn a_configuration_of_another_object_type_is_left_alone() {
        // Object type 1, AAC Main, with the same flag set: not what this is about, and not
        // something the decoder behind it would take either way.
        let main = [0x0a, 0x12];

        assert_eq!(repaired(&main), None);
    }

    #[test]
    fn a_configuration_spelling_out_its_sample_rate_is_left_alone() {
        // Sample rate index 15 puts twenty-four bits of rate where this counts on finding the
        // channel configuration, so nothing about it can be read the short way — least of all
        // these two bytes, which are the opening of one and no more.
        let spelled_out = [0x17, 0x92];

        assert_eq!(repaired(&spelled_out), None);
    }

    #[test]
    fn a_configuration_too_short_to_read_is_left_alone() {
        assert_eq!(repaired(&[]), None);
        assert_eq!(repaired(&[0x12]), None);
    }
}
