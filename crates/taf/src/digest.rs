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

    #[test]
    fn digest_trait_contract_matches_rfc3174_vector() {
        let mut d = RustCrypto(<sha1::Sha1 as sha1::Digest>::new());
        d.update(b"abc");
        assert_eq!(
            d.finalize(),
            [
                0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
                0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d
            ]
        );
    }
}
