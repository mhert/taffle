//! The hashing interface TAF needs, without an implementation behind it.

/// A streaming SHA-1 digest, supplied by whoever uses this crate.
///
/// The dependency is inverted on purpose: `taf` never implements SHA-1, it only states what it
/// needs from one. A TAF header carries a SHA-1 of the audio region, so writing and validating
/// files requires a digest — but requiring a *particular* digest would cost this crate its
/// zero-dependency, `no_std` footing. Consumers inject an implementation instead: a software
/// crate on a host, a hardware peripheral on a microcontroller.
///
/// Implementations must be RFC 3174 SHA-1: feeding `b"abc"` — in one call or several — and then
/// finalizing yields `a9993e364706816aba3e25717850c26c9cd0d89d`.
pub trait Sha1 {
    /// Feeds the next chunk of the message into the digest.
    fn update(&mut self, data: &[u8]);

    /// Consumes the digest and returns the hash of everything fed to it.
    fn finalize(self) -> [u8; 20];
}

/// The half of a [`Sha1`] that only takes bytes in, which is all a reader ever asks of one.
///
/// [`Sha1::finalize`] takes `self` by value — a digest has to be spent to hand its hash over — and
/// that is what keeps `dyn Sha1` from existing. Reading a file only ever feeds bytes, so
/// [`Validator`](crate::reader::Validator) asks for this instead and leaves finalizing to the
/// caller, which is what lets it take *some* digest without being generic over which. Every
/// `Sha1` is one of these; nothing has to implement it by hand. The two traits state the same
/// method, so a type that implements [`Sha1`] needs `Sha1::update(&mut digest, bytes)` to feed it
/// where both traits are in scope — they are one method, whichever way it is named.
pub trait Sha1Update {
    /// Feeds the next chunk of the message into the digest.
    fn update(&mut self, data: &[u8]);
}

impl<T: Sha1> Sha1Update for T {
    fn update(&mut self, data: &[u8]) {
        Sha1::update(self, data);
    }
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

    struct RustCrypto(sha1::Sha1);
    impl Sha1 for RustCrypto {
        fn update(&mut self, data: &[u8]) {
            sha1::Digest::update(&mut self.0, data);
        }
        fn finalize(self) -> [u8; 20] {
            sha1::Digest::finalize(self.0).into()
        }
    }

    /// The RFC 3174 vector for `b"abc"`.
    const ABC: [u8; 20] = [
        0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50, 0xc2,
        0x6c, 0x9c, 0xd0, 0xd8, 0x9d,
    ];

    #[test]
    fn every_digest_takes_bytes_through_a_reference_that_hides_which_one_it_is() {
        let mut digest = RustCrypto(<sha1::Sha1 as sha1::Digest>::new());

        {
            // What a reader holds: some digest, whichever it is, that the bytes go into. The two
            // calls prove the reference feeds the digest itself rather than a copy of it.
            let taking: &mut dyn Sha1Update = &mut digest;
            taking.update(b"a");
            taking.update(b"bc");
        }

        assert_eq!(digest.finalize(), ABC);
    }

    #[test]
    fn digest_trait_contract_matches_rfc3174_vector() {
        let mut d = RustCrypto(<sha1::Sha1 as sha1::Digest>::new());
        Sha1::update(&mut d, b"abc");
        assert_eq!(d.finalize(), ABC);
    }
}
