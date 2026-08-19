//! Padding an Opus packet out to an exact length, the way RFC 6716 §3.2.5 provides for.
//!
//! Every block-aligned page of a TAF is exactly 4096 bytes, and what makes it come out that way is
//! the last packet on it: it is padded until the page closes the block. RFC 6716 keeps room for
//! exactly that — a code 3 packet may carry "Opus padding" behind its frames, bytes that belong to
//! the packet but not to the audio — so a padded packet still plays as the packet it was made
//! from. teddycloud pads with libopus's `opus_packet_pad`; [`pad_to`] is that job spelled out from
//! the RFC, and `FORMAT.md` in this crate says where a TAF needs it.
//!
//! Only code 3 packets carry padding, so a packet in any other code is framed again as one. The
//! frames themselves are never touched: nothing here decodes audio, or looks inside a frame, or
//! cares which configuration the TOC byte states.

use core::fmt;
use core::iter;

use alloc::vec::Vec;

/// Why an Opus packet could not be padded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PadError {
    /// No bytes were handed in at all, and RFC 6716 has every packet start with a TOC byte `[R1]`.
    EmptyPacket,
    /// The length asked for is below the length the packet already has.
    TargetTooSmall,
    /// The length asked for is past the 65 024 bytes an Ogg page's lacing table describes.
    TargetTooLarge,
    /// The packet does not divide into the frames its TOC byte says it holds.
    MalformedToc,
}

impl fmt::Display for PadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::EmptyPacket => f.write_str("an Opus packet is at least one byte"),
            Self::TargetTooSmall => {
                f.write_str("an Opus packet cannot be padded to fewer bytes than it has")
            }
            Self::TargetTooLarge => write!(
                f,
                "an Opus packet cannot be padded past the {MAX_PADDED_LEN} bytes an Ogg page carries"
            ),
            Self::MalformedToc => {
                f.write_str("an Opus packet does not divide into the frames its TOC byte states")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for PadError {}

/// The longest packet [`pad_to`] builds: what an Ogg page's 255 lacing values describe.
///
/// A padded packet exists to be written onto a page, and no page carries a longer one — the writer
/// would refuse it and the number would have come from a mistake somewhere upstream. Bounding the
/// target here answers that mistake where it was made, and keeps the allocation a caller's number
/// asks for bounded with it.
const MAX_PADDED_LEN: usize = 65_024;

/// The bits of the TOC byte that state the frame count code, "c" of RFC 6716 §3.1.
const CODE: u8 = 0b0000_0011;

/// Code 0: one frame in the packet (§3.2.2).
const CODE_ONE_FRAME: u8 = 0;

/// Code 1: two frames of one size (§3.2.3).
const CODE_TWO_EQUAL: u8 = 1;

/// Code 2: two frames of two sizes, the first behind its stated length (§3.2.4).
const CODE_TWO_STATED: u8 = 2;

/// Code 3: a signalled number of frames, the only code that carries padding (§3.2.5).
const CODE_SIGNALLED: u8 = 3;

/// The bits of the frame count byte that state how many frames the packet holds — "M" of §3.2.5.
const COUNT: u8 = 0b0011_1111;

/// The bit of the frame count byte that states padding follows — "p" of §3.2.5.
const PADDED: u8 = 0b0100_0000;

/// The bit of the frame count byte that states a length per frame — "v" of §3.2.5.
const VBR: u8 = 0b1000_0000;

/// The frames one packet may hold: `[R5]` holds a packet to 120 ms of audio, which is 48
/// frames even at the 2.5 ms a frame carries at least.
const MAX_FRAMES: u8 = 48;

/// The first frame-length byte from which a second one follows (§3.2.1).
const TWO_BYTE_LEN: u8 = 252;

/// What the second frame-length byte counts in: the length is `second * 4 + first` (§3.2.1).
const LEN_STEP: usize = 4;

/// The padding length byte that says another one follows behind it (§3.2.5).
const PAD_CONTINUES: u8 = 255;

/// The padding bytes one length byte states at most.
///
/// A byte of 255 stands for 254 of them rather than 255, because the byte behind it is the next
/// length byte and takes the 255th.
const PAD_MAX: usize = 254;

/// The bytes a code 3 packet's framing takes before any padding length or frame length: the TOC
/// byte and the frame count byte, the "N-2" of `[R6]`.
const FRAMING_LEN: usize = 2;

/// Pads `packet` out to exactly `target_len` bytes, and hands over the packet that comes out.
///
/// What comes back is a code 3 packet (RFC 6716 §3.2.5) carrying the frames the packet handed in
/// carries, in the same order and byte for byte, with Opus padding behind them: the padding bit in
/// the frame count byte, the length of the padding, and the zero bytes themselves at the end. A
/// decoder plays a padded packet exactly as it plays the packet it was made from, because padding
/// is framing rather than audio — which is what makes it the way to close an Ogg page on a
/// boundary.
///
/// The packet's configuration and its mono/stereo flag — the top six bits of the TOC byte — come
/// through untouched. Only the frame count code changes, and only when it has to: a packet already
/// `target_len` bytes long is handed back byte for byte, whatever code it is in, which is what
/// libopus's `opus_packet_pad` does with the same two arguments.
///
/// Padding a packet that carries padding already does not wrap it: the packet is taken apart into
/// its frames, and framed again with one padding run behind them. That is what libopus's
/// repacketizer does too, so what comes out of here for a given frame set and length is the packet
/// teddycloud's files carry.
///
/// Framing again is not always longer than what it replaces, and where it lands exactly on
/// `target_len` there is nothing left to pad: the packet comes back the length that was asked for
/// with its padding bit clear, still carrying the same frames.
///
/// # Errors
///
/// - [`PadError::EmptyPacket`] if no bytes were handed in. `[R1]` has every packet start with a TOC
///   byte, so there is nothing here to pad and nothing to state a configuration.
/// - [`PadError::TargetTooSmall`] if `target_len` is below the length `packet` already has.
///   Padding never shortens a packet, not even one whose own padding could be dropped.
/// - [`PadError::TargetTooLarge`] if `target_len` is past the 65 024 bytes an Ogg page's lacing
///   table describes, which is the longest packet a page can carry at all.
/// - [`PadError::MalformedToc`] if the packet does not divide into the frames its TOC byte states,
///   so there is no frame set to frame again: a code 1 packet whose payload does not halve
///   `[R3]`, a code 2 or code 3 packet whose stated lengths overrun it `[R4,R7]`, a code 3
///   packet with no frame count byte, one stating no frames or more than 48 `[R5]`, one whose
///   padding run does not end inside it or overruns it `[R6,R7]`, or a CBR code 3 packet whose
///   bytes do not divide by its frame count `[R6]`.
pub fn pad_to(packet: &[u8], target_len: usize) -> Result<Vec<u8>, PadError> {
    let (&toc, payload) = packet.split_first().ok_or(PadError::EmptyPacket)?;

    if target_len < packet.len() {
        return Err(PadError::TargetTooSmall);
    }
    if target_len > MAX_PADDED_LEN {
        return Err(PadError::TargetTooLarge);
    }
    if target_len == packet.len() {
        return Ok(packet.to_vec());
    }

    let frames = Frames::parse(toc, payload)?;

    Ok(padded(toc, &frames, target_len))
}

/// The frames a packet carries, ready to be framed again with padding behind them.
///
/// Every code of RFC 6716 §3.2 lays its frames down as one run of bytes, so taking a packet apart
/// copies nothing: both slices point into the packet handed in.
#[derive(Debug)]
struct Frames<'a> {
    /// How many frames the packet carries — the "M" of §3.2.5, which is never zero.
    count: u8,
    /// The lengths of all but the last frame, exactly the bytes the packet stated them in — or
    /// nothing at all when the frames are all one size and the CBR framing states none.
    lengths: &'a [u8],
    /// The frames themselves, back to back.
    data: &'a [u8],
}

impl<'a> Frames<'a> {
    /// Takes apart the packet whose TOC byte is `toc` and whose remaining bytes are `payload`.
    fn parse(toc: u8, payload: &'a [u8]) -> Result<Self, PadError> {
        match toc & CODE {
            // §3.2.2: the one frame is everything behind the TOC byte.
            CODE_ONE_FRAME => Self::cbr(1, payload),
            // §3.2.3: two frames of (N-1)/2 bytes, which is [R3]'s "N-1 is even" — the same
            // arithmetic CBR does for any frame count.
            CODE_TWO_EQUAL => Self::cbr(2, payload),
            // §3.2.4: a stated length, the frame it states, and the rest — VBR with two frames.
            CODE_TWO_STATED => Self::vbr(2, payload),
            // §3.2.5.
            _ => Self::signalled(payload),
        }
    }

    /// The CBR shape of §3.2.5: `count` frames of one size, no length stated for any of them.
    ///
    /// `[R6]` has R — the bytes left once the padding is off — a non-negative integer multiple
    /// of M, which is what makes every frame R/M bytes long. `checked_rem` rather than `%` so
    /// that a frame count of zero, which `[R5]` forbids and [`signalled`](Self::signalled)
    /// refuses, could not divide by itself here.
    fn cbr(count: u8, body: &'a [u8]) -> Result<Self, PadError> {
        if body.len().checked_rem(usize::from(count)) != Some(0) {
            return Err(PadError::MalformedToc);
        }

        Ok(Self {
            count,
            lengths: &[],
            data: body,
        })
    }

    /// The VBR shape of §3.2.5: `count` frames, all but the last behind a stated length.
    ///
    /// `[R7]` has the packet hold those lengths and the bytes they state.
    fn vbr(count: u8, body: &'a [u8]) -> Result<Self, PadError> {
        let mut at = 0;
        let mut stated = 0;
        let mut first = None;
        let mut equal = true;

        for _ in 1..count {
            let (len, end) = frame_len(body, at)?;

            // The first length goes in and compares equal to itself; every later one is compared
            // against it.
            equal = equal && *first.get_or_insert(len) == len;
            stated += len;
            at = end;
        }

        // `frame_len` read every one of those bytes, so both halves are there.
        let lengths = body.get(..at).unwrap_or_default();
        let data = body.get(at..).unwrap_or_default();
        // The last frame is whatever the stated ones leave, and [R7] has them leave something.
        let last = data
            .len()
            .checked_sub(stated)
            .ok_or(PadError::MalformedToc)?;

        if equal && last == first.unwrap_or(last) {
            // Frames of one size are framed CBR, which states no lengths at all and so comes out
            // shorter. libopus's repacketizer picks the same shape, and the padded packet in the
            // golden fixture is one: three frames of 148 bytes, framed CBR by `opus_packet_pad`.
            return Ok(Self {
                count,
                lengths: &[],
                data,
            });
        }

        Ok(Self {
            count,
            lengths,
            data,
        })
    }

    /// The code 3 packet of §3.2.5: a frame count byte, the padding it states, then the frames.
    fn signalled(payload: &'a [u8]) -> Result<Self, PadError> {
        // [R6,R7]: a code 3 packet has at least two bytes, the second of them this one.
        let (&count_byte, rest) = payload.split_first().ok_or(PadError::MalformedToc)?;
        let count = count_byte & COUNT;

        // [R5]: M is above zero, and 120 ms of audio is 48 frames at the shortest frame there is.
        if count == 0 || count > MAX_FRAMES {
            return Err(PadError::MalformedToc);
        }

        let body = if count_byte & PADDED == 0 {
            rest
        } else {
            unpadded(rest)?
        };

        if count_byte & VBR == 0 {
            Self::cbr(count, body)
        } else {
            Self::vbr(count, body)
        }
    }
}

/// Reads the frame length §3.2.1 states at `at` in `body`, and says where the next one starts.
///
/// A first byte of 0...251 is the length itself, and 0 is a frame that was not transmitted at all.
/// A first byte of 252...255 says a second one follows, and together they state `second * 4 +
/// first` — up to the 1275 bytes `[R2]` holds a frame to.
fn frame_len(body: &[u8], at: usize) -> Result<(usize, usize), PadError> {
    let &first = body.get(at).ok_or(PadError::MalformedToc)?;

    if first < TWO_BYTE_LEN {
        return Ok((usize::from(first), at + 1));
    }

    let &second = body.get(at + 1).ok_or(PadError::MalformedToc)?;

    Ok((usize::from(second) * LEN_STEP + usize::from(first), at + 2))
}

/// Steps over the padding a code 3 packet states, and hands back what is left for its frames.
///
/// §3.2.5 puts the padding's length in the bytes behind the frame count byte: a value of 0...254
/// states that many padding bytes, on top of the byte stating it, and a value of 255 states 254 of
/// them plus whatever the next byte states — and `[R6,R7]` have that next byte be there. The
/// padding bytes themselves sit at the end of the packet, behind the frames.
fn unpadded(payload: &[u8]) -> Result<&[u8], PadError> {
    let mut rest = payload;
    let mut padding = 0;

    loop {
        let (&stated, tail) = rest.split_first().ok_or(PadError::MalformedToc)?;
        rest = tail;

        if stated != PAD_CONTINUES {
            padding += usize::from(stated);
            break;
        }

        // Every step of this reads a byte of a packet no longer than the target it is being padded
        // to, so the sum stays far inside a `usize`, 32-bit ones included.
        padding += PAD_MAX;
    }

    // [R6,R7]: the padding is part of the packet, so the bytes it states have to be in it.
    let frames = rest
        .len()
        .checked_sub(padding)
        .ok_or(PadError::MalformedToc)?;

    Ok(rest.get(..frames).unwrap_or_default())
}

/// Frames `frames` behind `toc` as the code 3 packet of §3.2.5, padded out to `target_len`.
fn padded(toc: u8, frames: &Frames<'_>, target_len: usize) -> Vec<u8> {
    let mut packet = Vec::with_capacity(target_len);

    // What the framing alone comes to. Framing a packet again costs at most the one byte the frame
    // count byte takes over the code the packet was in, and `pad_to` is only here for a target
    // past the packet's own length — so this never saturates.
    let minimum = FRAMING_LEN + frames.lengths.len() + frames.data.len();
    let padding = target_len.saturating_sub(minimum);
    let states_lengths = if frames.lengths.is_empty() { 0 } else { VBR };
    let states_padding = if padding == 0 { 0 } else { PADDED };

    // Code 3 is both of the code bits set, so stating it leaves the six bits above them — the
    // configuration and the mono/stereo flag — exactly as the packet stated them.
    packet.push(toc | CODE_SIGNALLED);
    packet.push(frames.count | states_lengths | states_padding);

    if padding != 0 {
        // Every length byte states at most PAD_MAX bytes and occupies one itself, so a run of them
        // covers the padding in steps of PAD_MAX + 1 and the last states what is left over. The
        // byte the run starts with is what the first of those bytes pays for.
        let stated = padding - 1;
        packet.extend(iter::repeat_n(PAD_CONTINUES, stated / (PAD_MAX + 1)));
        // A remainder is below what it was divided by, so this always converts.
        packet.push(u8::try_from(stated % (PAD_MAX + 1)).unwrap_or(PAD_CONTINUES));
    }

    packet.extend_from_slice(frames.lengths);
    packet.extend_from_slice(frames.data);
    // What is left of the target is the padding itself, which "MUST be set to zero by the encoder
    // to avoid creating a covert channel".
    packet.resize(target_len, 0);

    packet
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
    use crate::ogg::{BuildError, PageBuilder, PageView};
    use alloc::vec;
    use alloc::vec::Vec;

    const GOLDEN: &[u8] = include_bytes!("../tests/fixtures/golden-sine.taf");

    /// Where the golden file's first audio page starts: behind the two Opus header pages.
    const AUDIO_PAGE_AT: usize = 4608;

    /// A TOC byte in configuration 1, stereo, stating one frame — the low two bits are the code, so
    /// the packets below start from this and set their own.
    const TOC: u8 = 0b0000_1100;

    /// The bits of a TOC byte that are not the frame count code: the configuration and the
    /// mono/stereo flag, which padding carries over untouched.
    const TOC_KEPT: u8 = 0b1111_1100;

    /// The packets the golden file's first audio page carries: 895, 716, 743, 738 and 450 bytes,
    /// the last of them padded by teddycloud's own call to libopus's `opus_packet_pad`.
    fn golden_packets() -> Vec<&'static [u8]> {
        let view = PageView::parse(&GOLDEN[AUDIO_PAGE_AT..]).unwrap();
        let packets: Vec<&[u8]> = view.packets().take(256).collect();

        assert_eq!(
            packets
                .iter()
                .map(|packet| packet.len())
                .collect::<Vec<_>>(),
            [895, 716, 743, 738, 450]
        );

        packets
    }

    /// An Opus packet taken apart the way RFC 6716 §3.2 spells it out.
    ///
    /// This is a second reading of that section, written from the document's own wording rather
    /// than from the code above it, so that a test decoding what [`pad_to`] produced checks the
    /// implementation instead of agreeing with it. Every rule of §3.4 it can see is asserted as it
    /// goes.
    #[derive(Debug, PartialEq, Eq)]
    struct Rfc<'a> {
        /// The top five bits of the TOC byte (§3.1).
        config: u8,
        /// The mono/stereo flag behind them, "s" (§3.1).
        stereo: bool,
        /// The frame count code, "c" (§3.1).
        code: u8,
        /// Whether a code 3 packet's frame count byte states VBR, "v" (§3.2.5).
        vbr: bool,
        /// Whether it states padding, "p" (§3.2.5).
        padded: bool,
        /// "P" of `[R6]`: the bytes the padding occupies, the bytes stating its length counted in.
        padding: usize,
        /// The frames the packet carries, in order.
        frames: Vec<&'a [u8]>,
    }

    /// Reads the one- or two-byte frame length §3.2.1 states at the front of `bytes`, and hands
    /// back what follows it.
    fn rfc_frame_len(bytes: &[u8]) -> (usize, &[u8]) {
        assert!(
            !bytes.is_empty(),
            "a stated length needs a byte to state it"
        );

        let first = usize::from(bytes[0]);
        if first < 252 {
            // "1...251: Length of the frame in bytes", and 0 is a frame that is not there.
            return (first, &bytes[1..]);
        }

        // "252...255: A second byte is needed. The total length is (second_byte*4)+first_byte".
        assert!(bytes.len() >= 2, "a first byte of 252...255 needs a second");
        let total = usize::from(bytes[1]) * 4 + first;
        assert!(total <= 1275, "[R2] no frame is longer than 1275 bytes");

        (total, &bytes[2..])
    }

    fn rfc_decode(packet: &[u8]) -> Rfc<'_> {
        assert!(!packet.is_empty(), "[R1] packets are at least one byte");

        let toc = packet[0];
        let body = &packet[1..];
        let mut decoded = Rfc {
            config: toc >> 3,
            stereo: toc & 0b0000_0100 != 0,
            code: toc & 0b0000_0011,
            vbr: false,
            padded: false,
            padding: 0,
            frames: Vec::new(),
        };

        match decoded.code {
            // §3.2.2: "the TOC byte is immediately followed by N-1 bytes of compressed data for a
            // single frame".
            0 => decoded.frames.push(body),
            // §3.2.3: "(N-1)/2 bytes of compressed data for the first frame, followed by (N-1)/2
            // bytes ... for the second".
            1 => {
                assert!(
                    body.len().is_multiple_of(2),
                    "[R3] N-1 is even for code 1 packets"
                );
                decoded.frames.push(&body[..body.len() / 2]);
                decoded.frames.push(&body[body.len() / 2..]);
            }
            // §3.2.4: "the TOC byte is followed by a one- or two-byte sequence indicating the
            // length of the first frame ..., followed by N1 bytes of compressed data ... The
            // remaining ... bytes are the compressed data for the second frame".
            2 => {
                let (first, rest) = rfc_frame_len(body);
                assert!(
                    first <= rest.len(),
                    "[R4] N1 fits what is left of the packet"
                );
                decoded.frames.push(&rest[..first]);
                decoded.frames.push(&rest[first..]);
            }
            // §3.2.5.
            _ => rfc_signalled(packet, body, &mut decoded),
        }

        assert!(!decoded.frames.is_empty(), "every packet carries a frame");

        decoded
    }

    /// Takes apart the code 3 packet of §3.2.5 whose bytes behind the TOC byte are `body`.
    fn rfc_signalled<'a>(packet: &'a [u8], body: &'a [u8], decoded: &mut Rfc<'a>) {
        assert!(
            packet.len() >= 2,
            "[R6,R7] code 3 packets have at least 2 bytes"
        );

        // "The TOC byte is followed by a byte encoding the number of frames in the packet in bits 2
        // to 7 ..., with bit 1 indicating whether or not Opus padding is inserted ..., and bit 0
        // indicating VBR".
        let count_byte = body[0];
        let count = usize::from(count_byte & 0b0011_1111);
        decoded.vbr = count_byte & 0b1000_0000 != 0;
        decoded.padded = count_byte & 0b0100_0000 != 0;
        assert!(count > 0, "[R5] M MUST NOT be zero");
        assert!(count <= 48, "[R5] and no more than 120 ms of audio");

        let (count_bytes, zeros) = if decoded.padded {
            rfc_padding(&body[1..])
        } else {
            (0, 0)
        };
        let rest = &body[1 + count_bytes..];

        decoded.padding = count_bytes + zeros;
        assert!(
            decoded.padding <= packet.len() - 2,
            "[R6,R7] P is no more than N-2"
        );
        assert!(
            zeros <= rest.len(),
            "[R6,R7] the padding is inside the packet"
        );

        // "The additional padding bytes appear at the end of the packet and MUST be set to zero by
        // the encoder".
        let (content, padding) = rest.split_at(rest.len() - zeros);
        assert!(padding.iter().all(|&byte| byte == 0), "padding is zeroed");

        decoded.frames = if decoded.vbr {
            rfc_stated_frames(content, count)
        } else {
            rfc_equal_frames(content, count)
        };
    }

    /// Reads the run of padding length bytes §3.2.5 puts behind the frame count byte: how many
    /// bytes state the padding, and how many padding bytes they state between them.
    fn rfc_padding(bytes: &[u8]) -> (usize, usize) {
        let mut rest = bytes;
        let mut count_bytes = 0;
        let mut zeros = 0;

        loop {
            assert!(!rest.is_empty(), "[R6,R7] a padding length needs a byte");
            let stated = rest[0];
            rest = &rest[1..];
            count_bytes += 1;

            // "Values from 0...254 indicate that 0...254 bytes of padding are included, in addition
            // to the byte(s) used to indicate the size of the padding. If the value is 255, then
            // the size of the additional padding is 254 bytes, plus the padding value encoded in
            // the next byte."
            if stated == 255 {
                zeros += 254;
            } else {
                return (count_bytes, zeros + usize::from(stated));
            }
        }
    }

    /// The frames of a VBR code 3 packet: "the (optional) padding length is followed by M-1 frame
    /// lengths ... The compressed data for all M frames follows, ... with the final frame consuming
    /// any remaining bytes before the final padding".
    fn rfc_stated_frames(content: &[u8], count: usize) -> Vec<&[u8]> {
        let mut lengths = Vec::new();
        let mut rest = content;

        for _ in 1..count {
            let (length, tail) = rfc_frame_len(rest);
            lengths.push(length);
            rest = tail;
        }

        assert!(
            lengths.iter().sum::<usize>() <= rest.len(),
            "[R7] the stated lengths fit what is left"
        );

        let mut frames = Vec::new();
        for length in lengths {
            frames.push(&rest[..length]);
            rest = &rest[length..];
        }
        frames.push(rest);

        frames
    }

    /// The frames of a CBR code 3 packet: "let R=N-2-P be the number of bytes remaining in the
    /// packet after subtracting the (optional) padding. Then, the compressed length of each frame
    /// in bytes is equal to R/M."
    fn rfc_equal_frames(content: &[u8], count: usize) -> Vec<&[u8]> {
        assert!(
            content.len().is_multiple_of(count),
            "[R6] R is a multiple of the frame count"
        );

        let size = content.len() / count;

        (0..count)
            .map(|frame| &content[frame * size..(frame + 1) * size])
            .collect()
    }

    fn frame_lens(decoded: &Rfc<'_>) -> Vec<usize> {
        decoded.frames.iter().map(|frame| frame.len()).collect()
    }

    #[test]
    fn hands_back_a_packet_already_as_long_as_it_is_to_be_padded_to() {
        for packet in golden_packets() {
            assert_eq!(pad_to(packet, packet.len()).unwrap(), packet);
        }

        // Whatever code the packet is in, and however short it is. Re-framing either of these as a
        // code 3 packet would take a byte more than they have, so bytes coming back unchanged is
        // proof that nothing was re-framed.
        assert_eq!(pad_to(&[TOC], 1).unwrap(), [TOC]);
        assert_eq!(
            pad_to(&[TOC | 1, 7, 9], 3).unwrap(),
            [TOC | 1, 7, 9],
            "a code 1 packet of two one-byte frames"
        );
    }

    #[test]
    fn pads_a_real_opus_packet_to_ten_bytes_more_than_it_has() {
        let packet = golden_packets()[0];
        let padded = pad_to(packet, packet.len() + 10).unwrap();
        let before = rfc_decode(packet);
        let after = rfc_decode(&padded);

        assert_eq!(padded.len(), 905);
        assert_eq!(
            after.config, before.config,
            "the TOC's config bits are kept"
        );
        assert_eq!(after.stereo, before.stereo);
        assert_eq!(padded[0], packet[0]);
        assert_eq!(after.code, 3, "only code 3 packets carry padding");
        assert!(after.padded);
        assert_eq!(after.frames, before.frames, "the same frames, in order");
        assert_eq!(frame_lens(&after), [384, 260, 245]);

        // The packet was already code 3, VBR, three frames — so the frame count byte states the
        // same thing it did, with the padding bit set on top.
        assert!(after.vbr, "frames of three sizes state their lengths");
        assert_eq!(padded[1], 0b1100_0011);
        // One byte states the padding, and it counts itself out of the ten: nine zero bytes follow
        // the frames.
        assert_eq!(after.padding, 10);
        assert_eq!(padded[2], 9);
        assert_eq!(&padded[3..7], &packet[2..6], "the stated lengths are kept");
        assert!(padded[896..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn pads_a_one_byte_code_0_packet_to_a_hundred() {
        let padded = pad_to(&[TOC], 100).unwrap();
        let decoded = rfc_decode(&padded);

        assert_eq!(padded.len(), 100);
        assert_eq!(decoded.code, 3);
        assert_eq!(decoded.config, TOC >> 3);
        assert!(decoded.stereo);
        assert_eq!(decoded.frames, rfc_decode(&[TOC]).frames);
        assert_eq!(
            decoded.frames,
            [&[] as &[u8]],
            "the one frame is still empty"
        );
        assert!(!decoded.vbr, "one frame states no length");
        assert_eq!(
            decoded.padding, 98,
            "[R6] P is N-2 here, the most there is room for"
        );
        assert_eq!(&padded[..3], [TOC | 3, 0b0100_0001, 97]);
        assert!(padded[3..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn refuses_a_target_below_the_length_the_packet_already_has() {
        let packet = golden_packets()[0];

        assert_eq!(
            pad_to(packet, packet.len() - 1),
            Err(PadError::TargetTooSmall)
        );
        assert_eq!(pad_to(packet, 0), Err(PadError::TargetTooSmall));
        assert_eq!(pad_to(&[TOC], 0), Err(PadError::TargetTooSmall));
    }

    #[test]
    fn refuses_a_packet_of_no_bytes_at_all() {
        // [R1]: every packet starts with a TOC byte, so there is nothing here to pad — which is
        // answered before the target is looked at at all.
        assert_eq!(pad_to(&[], 100), Err(PadError::EmptyPacket));
        assert_eq!(pad_to(&[], 0), Err(PadError::EmptyPacket));
    }

    #[test]
    fn states_the_padding_length_in_the_run_of_bytes_the_rfc_spells_out() {
        // A one-byte packet frames as two bytes, so the target beyond that is the padding itself.
        // The RFC's own worked examples are the 255 and 256 rows: "to add 255 bytes to a packet,
        // set the padding bit, p, to 1, insert a single byte after the frame count byte with a
        // value of 254, and append 254 padding bytes with the value zero to the end of the packet.
        // To add 256 bytes to a packet, set the padding bit to 1, insert two bytes after the frame
        // count byte with the values 255 and 0, respectively, and append 254 padding bytes".
        for (padding, stated, zeros) in [
            (1_usize, vec![0_u8], 0_usize),
            (2, vec![1], 1),
            (254, vec![253], 253),
            (255, vec![254], 254),
            (256, vec![255, 0], 254),
            (257, vec![255, 1], 255),
            (510, vec![255, 254], 508),
            (511, vec![255, 255, 0], 508),
            (512, vec![255, 255, 1], 509),
        ] {
            let target = 2 + padding;
            let padded = pad_to(&[TOC], target).unwrap();
            let decoded = rfc_decode(&padded);

            assert_eq!(padded.len(), target, "padding {padding}");
            assert_eq!(decoded.padding, padding, "padding {padding}");
            assert_eq!(&padded[2..2 + stated.len()], &stated, "padding {padding}");
            assert_eq!(stated.len() + zeros, padding, "the row itself adds up");
            assert!(
                padded[2 + stated.len()..].iter().all(|&byte| byte == 0),
                "padding {padding}"
            );
            assert_eq!(
                padded.len() - (2 + stated.len()),
                zeros,
                "padding {padding}"
            );
        }
    }

    #[test]
    fn produces_the_packet_libopus_produced_in_the_golden_file() {
        // The last packet of the golden file's first audio page is what teddycloud handed to
        // libopus's `opus_packet_pad` to close the page: three frames of 148 bytes, framed CBR,
        // with one byte stating three bytes of padding behind them.
        let padded = golden_packets()[4];
        let decoded = rfc_decode(padded);

        assert_eq!(decoded.code, 3);
        assert!(decoded.padded && !decoded.vbr);
        assert_eq!(frame_lens(&decoded), [148, 148, 148]);
        assert_eq!(decoded.padding, 4);

        // The packet libopus was handed: the same frames, framed without padding. Padding that
        // back to 450 bytes has to come out as the bytes libopus wrote.
        let mut unpadded = vec![padded[0], 0b0000_0011];
        unpadded.extend_from_slice(&decoded.frames.concat());

        assert_eq!(unpadded.len(), 446);
        assert_eq!(pad_to(&unpadded, 450).unwrap(), padded);
    }

    #[test]
    fn frames_a_padded_packet_again_rather_than_padding_its_padding() {
        let padded = golden_packets()[4];
        let more = pad_to(padded, 460).unwrap();
        let decoded = rfc_decode(&more);

        assert_eq!(more.len(), 460);
        assert_eq!(decoded.frames, rfc_decode(padded).frames);
        // The padding is stated once, in the shortest run of bytes that states it — the four bytes
        // the packet came with are gone, not wrapped in ten more.
        assert_eq!(decoded.padding, 14);
        assert_eq!(more[2], 13);
        assert!(!decoded.vbr);
        assert_eq!(
            &more[3..447],
            &padded[3..447],
            "the frames sit where they sat"
        );
        assert!(more[447..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn reads_the_padding_run_of_a_packet_it_pads_again() {
        // A packet whose own padding takes a run of two bytes: 255 states 254 zero bytes and hands
        // on to the byte behind it, which states none. Five frame bytes behind them, one frame.
        let mut packet = vec![TOC | 3, 0b0100_0001, 255, 0, 1, 2, 3, 4, 5];
        packet.resize(263, 0);
        let decoded = rfc_decode(&packet);

        assert_eq!(
            decoded.padding, 256,
            "two bytes stating 254 zeros between them"
        );
        assert_eq!(decoded.frames, [&[1, 2, 3, 4, 5][..]]);

        // Padding it again reads that run, drops it, and states the new padding in a run of its
        // own: 293 bytes of it, which is one 255 and 37 more.
        let padded = pad_to(&packet, 300).unwrap();
        let after = rfc_decode(&padded);

        assert_eq!(padded.len(), 300);
        assert_eq!(after.frames, decoded.frames);
        assert_eq!(after.padding, 293);
        assert_eq!(&padded[..4], [TOC | 3, 0b0100_0001, 255, 37]);
        assert_eq!(&padded[4..9], [1, 2, 3, 4, 5]);
        assert!(padded[9..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn frames_a_packet_cbr_only_when_every_frame_is_one_size() {
        // Three frames whose two stated lengths match while the last one does not: the lengths have
        // to be stated, because CBR could not say this.
        let mut differ = vec![TOC | 3, 0b1000_0011, 4, 4];
        differ.extend_from_slice(&[1; 13]);
        let padded = pad_to(&differ, 20).unwrap();
        let decoded = rfc_decode(&padded);

        assert!(
            decoded.vbr,
            "frames of 4, 4 and 5 bytes state their lengths"
        );
        assert_eq!(frame_lens(&decoded), [4, 4, 5]);
        assert_eq!(&padded[..5], [TOC | 3, 0b1100_0011, 2, 4, 4]);

        // The same packet one byte shorter, which makes the last frame match the other two.
        let mut match_up = vec![TOC | 3, 0b1000_0011, 4, 4];
        match_up.extend_from_slice(&[1; 12]);
        let padded = pad_to(&match_up, 20).unwrap();
        let decoded = rfc_decode(&padded);

        assert!(!decoded.vbr, "three frames of 4 bytes state nothing");
        assert_eq!(frame_lens(&decoded), [4, 4, 4]);
        assert_eq!(&padded[..3], [TOC | 3, 0b0100_0011, 5]);
    }

    #[test]
    fn frames_the_48_frames_a_packet_may_hold() {
        // [R5]'s limit is a count it allows, not one it refuses.
        let mut packet = vec![TOC | 3, MAX_FRAMES];
        packet.extend_from_slice(&[7; 48]);
        let padded = pad_to(&packet, 60).unwrap();
        let decoded = rfc_decode(&padded);

        assert_eq!(padded.len(), 60);
        assert_eq!(decoded.frames.len(), 48);
        assert_eq!(frame_lens(&decoded), [1; 48]);
        assert_eq!(&padded[..3], [TOC | 3, 0b0111_0000, 9]);
    }

    #[test]
    fn frames_a_code_1_packet_as_the_code_3_packet_padding_needs() {
        // Two frames of four bytes, which code 1 states by halving what is behind the TOC byte.
        let mut packet = vec![TOC | 1];
        packet.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let padded = pad_to(&packet, 20).unwrap();
        let decoded = rfc_decode(&padded);

        assert_eq!(padded.len(), 20);
        assert_eq!(decoded.code, 3);
        assert_eq!(decoded.frames, rfc_decode(&packet).frames);
        assert_eq!(decoded.frames, [&[1, 2, 3, 4][..], &[5, 6, 7, 8][..]]);
        assert!(!decoded.vbr, "two frames of one size are framed CBR");
        assert_eq!(&padded[..3], [TOC | 3, 0b0100_0010, 9]);
        assert_eq!(decoded.padding, 10);
    }

    #[test]
    fn keeps_the_stated_length_of_a_code_2_packet_whose_frames_differ() {
        // A stated first frame of three bytes and a second of five.
        let mut packet = vec![TOC | 2, 3];
        packet.extend_from_slice(&[1, 2, 3, 9, 9, 9, 9, 9]);
        let padded = pad_to(&packet, 15).unwrap();
        let decoded = rfc_decode(&padded);

        assert_eq!(padded.len(), 15);
        assert_eq!(decoded.frames, rfc_decode(&packet).frames);
        assert_eq!(decoded.frames, [&[1, 2, 3][..], &[9, 9, 9, 9, 9][..]]);
        assert!(decoded.vbr, "frames of two sizes state their lengths");
        // Figure 7's order: the frame count byte, the padding length, then the frame lengths.
        assert_eq!(&padded[..4], [TOC | 3, 0b1100_0010, 3, 3]);
        assert_eq!(decoded.padding, 4);
    }

    #[test]
    fn reads_the_two_byte_length_a_long_frame_states() {
        // §3.2.1: a first byte of 252...255 says a second one follows, and together they state
        // `second * 4 + first` — so these two state 2 * 4 + 252 = 260. Reading them as anything
        // else makes the two frames look like frames of two sizes, and the packet comes out VBR
        // with the length restated rather than CBR with it dropped.
        let mut packet = vec![TOC | 2, 252, 2];
        packet.extend_from_slice(&[1; 260]);
        packet.extend_from_slice(&[2; 260]);
        let padded = pad_to(&packet, 527).unwrap();
        let decoded = rfc_decode(&padded);

        assert_eq!(packet.len(), 523);
        assert!(!decoded.vbr, "two frames of 260 bytes are one size");
        assert_eq!(frame_lens(&decoded), [260, 260]);
        assert_eq!(decoded.frames, rfc_decode(&packet).frames);
        assert_eq!(&padded[..3], [TOC | 3, 0b0100_0010, 4]);
        assert_eq!(decoded.padding, 5);
    }

    #[test]
    fn states_no_padding_when_framing_the_packet_again_lands_on_the_target() {
        // Framing this code 2 packet as a code 3 packet costs exactly the byte the frame count byte
        // takes, so there is nothing left over to pad: the padding bit stays clear and no length
        // byte follows the frame count byte.
        let mut packet = vec![TOC | 2, 3];
        packet.extend_from_slice(&[1, 2, 3, 9, 9, 9, 9, 9]);
        let padded = pad_to(&packet, packet.len() + 1).unwrap();
        let decoded = rfc_decode(&padded);

        assert_eq!(padded, [TOC | 3, 0b1000_0010, 3, 1, 2, 3, 9, 9, 9, 9, 9]);
        assert!(!decoded.padded);
        assert_eq!(decoded.padding, 0);
        assert_eq!(decoded.frames, rfc_decode(&packet).frames);
    }

    #[test]
    fn drops_the_stated_length_of_a_code_2_packet_whose_frames_match() {
        let mut packet = vec![TOC | 2, 3];
        packet.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        let padded = pad_to(&packet, 9).unwrap();
        let decoded = rfc_decode(&padded);

        // Two frames of one size need no stated length at all, so the byte that stated it pays for
        // the frame count byte: the packet grows by one byte and gains its padding bit for free.
        assert_eq!(padded, [TOC | 3, 0b0100_0010, 0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(decoded.frames, rfc_decode(&packet).frames);
        assert_eq!(decoded.frames, [&[1, 2, 3][..], &[4, 5, 6][..]]);
        assert_eq!(decoded.padding, 1, "one byte, stating no bytes behind it");
    }

    #[test]
    fn refuses_a_packet_it_cannot_take_apart() {
        for (why, packet) in [
            (
                "[R3] code 1 whose payload does not halve",
                vec![TOC | 1, 1, 2, 3],
            ),
            ("[R4] code 2 with no stated length", vec![TOC | 2]),
            (
                "[R4] code 2 whose stated length needs a second byte it does not have",
                vec![TOC | 2, 252],
            ),
            (
                "[R4] code 2 whose first frame is longer than what is left",
                vec![TOC | 2, 5, 1, 2],
            ),
            ("[R6,R7] code 3 with no frame count byte", vec![TOC | 3]),
            ("[R5] CBR code 3 stating no frames", vec![TOC | 3, 0, 1, 2]),
            (
                // Nothing else refuses this one: no length is stated for a frame set of none, so
                // the frame count byte is the only place it goes wrong.
                "[R5] VBR code 3 stating no frames",
                vec![TOC | 3, 0b1000_0000, 1, 2],
            ),
            (
                // A body of 49 bytes divides by 49, so this too is refused for the count alone.
                "[R5] code 3 stating more than 48 frames",
                [vec![TOC | 3, 49], vec![7; 49]].concat(),
            ),
            (
                "[R6] CBR code 3 whose bytes do not divide by its frame count",
                vec![TOC | 3, 3, 1, 2],
            ),
            (
                "[R7] VBR code 3 whose stated lengths overrun it",
                vec![TOC | 3, 0b1000_0010, 5, 1, 2],
            ),
            (
                "[R7] VBR code 3 that ends before its stated lengths",
                vec![TOC | 3, 0b1000_0010],
            ),
            (
                "[R6,R7] code 3 whose padding run never ends",
                vec![TOC | 3, 0b0100_0011, 255],
            ),
            (
                "[R6,R7] code 3 whose padding overruns it",
                vec![TOC | 3, 0b0100_0011, 9, 1, 2],
            ),
        ] {
            assert_eq!(pad_to(&packet, 100), Err(PadError::MalformedToc), "{why}");
        }
    }

    #[test]
    fn refuses_a_target_no_ogg_page_could_carry() {
        assert_eq!(
            pad_to(&[TOC], MAX_PADDED_LEN).unwrap().len(),
            MAX_PADDED_LEN
        );
        assert_eq!(
            pad_to(&[TOC], MAX_PADDED_LEN + 1),
            Err(PadError::TargetTooLarge)
        );
        assert_eq!(pad_to(&[TOC], usize::MAX), Err(PadError::TargetTooLarge));
        // A packet already past it is refused even at the length it has, so nothing that comes back
        // from here is longer than a page describes.
        assert_eq!(
            pad_to(&vec![TOC; MAX_PADDED_LEN + 1], MAX_PADDED_LEN + 1),
            Err(PadError::TargetTooLarge)
        );
        // ... but a target below the packet's own length is the other error, which is answered
        // first.
        assert_eq!(
            pad_to(&vec![TOC; MAX_PADDED_LEN + 2], MAX_PADDED_LEN + 1),
            Err(PadError::TargetTooSmall)
        );

        // The same number from the other side: 65 024 bytes is the longest packet an Ogg page's
        // 255 lacing values describe, which is where `PageBuilder` draws its own line.
        let mut page = PageBuilder::new(1, 0);
        assert_eq!(
            page.push_packet(&vec![0; MAX_PADDED_LEN + 1]),
            Err(BuildError::PacketTooLarge)
        );
        assert_eq!(
            page.push_packet(&vec![0; MAX_PADDED_LEN]),
            Err(BuildError::PageFull),
            "expressible, but no page has room for it"
        );
    }

    #[test]
    fn pads_every_packet_of_a_real_page_to_every_length_asked_for() {
        for packet in golden_packets() {
            let frames = rfc_decode(packet).frames;
            let len = packet.len();

            for added in [1, 2, 3, 63, 64, 254, 255, 256, 257, 1000] {
                let target = len + added;
                let padded = pad_to(packet, target).unwrap();
                let decoded = rfc_decode(&padded);

                assert_eq!(padded.len(), target, "{len} + {added}");
                assert_eq!(decoded.code, 3, "{len} + {added}");
                assert!(decoded.padded, "{len} + {added}");
                assert_eq!(decoded.frames, frames, "{len} + {added}");
                assert_eq!(
                    padded[0] & TOC_KEPT,
                    packet[0] & TOC_KEPT,
                    "{len} + {added}"
                );
            }
        }
    }

    #[test]
    fn every_error_says_what_went_wrong() {
        let rendered = [
            PadError::EmptyPacket,
            PadError::TargetTooSmall,
            PadError::TargetTooLarge,
            PadError::MalformedToc,
        ]
        .map(|error| alloc::format!("{error}"));

        assert_eq!(
            rendered,
            [
                "an Opus packet is at least one byte",
                "an Opus packet cannot be padded to fewer bytes than it has",
                "an Opus packet cannot be padded past the 65024 bytes an Ogg page carries",
                "an Opus packet does not divide into the frames its TOC byte states",
            ]
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn pad_error_is_a_standard_error() {
        let error: &dyn std::error::Error = &PadError::MalformedToc;

        assert_eq!(
            std::string::ToString::to_string(error),
            "an Opus packet does not divide into the frames its TOC byte states"
        );
    }
}
