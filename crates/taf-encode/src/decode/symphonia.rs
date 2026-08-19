//! [`AudioSource`] over symphonia: WAV, FLAC, MP3, AAC in MP4/M4B, and Vorbis in Ogg — every input
//! format the converter takes apart from Opus itself.
//!
//! # The format is decided by the bytes
//!
//! [`open_source`] hands the probe an empty [`Hint`]. A caller may have no file name to offer at
//! all — a stream, a buffer, an upload — and an extension that lies is worse than one that is
//! missing, so what the input starts with is what decides. That also means garbage is recognized
//! as garbage before anything tries to decode it.
//!
//! # Which errors are which
//!
//! symphonia reports the end of a stream as an unexpected end of file, so the same error type
//! carries both "there is no more audio", which is how every successful conversion finishes, and
//! "the input broke off", which no caller should be told is a clean end. The two functions at the
//! bottom of this module are that distinction, and they draw it differently in the two places it
//! matters: while opening, bytes that run out mean the input was never a format to begin with;
//! while decoding, they mean the stream is over.

use std::io::ErrorKind;

use symphonia::core::audio::{AudioBufferRef, SampleBuffer};
use symphonia::core::codecs::{CodecParameters, Decoder, DecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::default::{get_codecs, get_probe};

use super::{AudioSource, DecodeError, SourceMetadata, SourceSpec};

/// Opens whatever the reader holds, if this build can decode it.
///
/// The input's own bytes decide what it is: nothing here looks at a file name, and there is no way
/// to assert one.
///
/// # Errors
///
/// [`DecodeError::UnsupportedFormat`] when the input is not a container this build recognizes, or
/// holds a codec it cannot decode. [`DecodeError::NoAudioTrack`] when it is a container but states
/// no track of decodable audio. [`DecodeError::Io`] when the input itself cannot be read.
pub fn open_source(reader: Box<dyn MediaSource>) -> Result<Box<dyn AudioSource>, DecodeError> {
    let stream = MediaSourceStream::new(reader, MediaSourceStreamOptions::default());
    let probed = get_probe()
        .format(
            &Hint::new(),
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(open_error)?;

    let format = probed.format;
    let track = format.default_track().ok_or(DecodeError::NoAudioTrack)?;
    let track_id = track.id;
    let spec = source_spec(&track.codec_params).ok_or(DecodeError::NoAudioTrack)?;
    let decoder = get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(open_error)?;

    Ok(Box::new(SymphoniaSource {
        format,
        decoder,
        track_id,
        spec,
    }))
}

/// One track of one container, decoded packet by packet.
struct SymphoniaSource {
    /// The demuxer the packets come from.
    format: Box<dyn FormatReader>,
    /// The decoder for [`track_id`](Self::track_id)'s codec.
    decoder: Box<dyn Decoder>,
    /// Which track's packets to decode: a container may carry more than one, and the decoder only
    /// understands the codec of this one.
    track_id: u32,
    /// What the container declared the track's signal to be, settled when it was opened.
    spec: SourceSpec,
}

impl AudioSource for SymphoniaSource {
    fn spec(&self) -> SourceSpec {
        self.spec
    }

    /// Nothing, so far. A WAV states no chapters and carries no cover, and the metadata the other
    /// containers do carry is not read out of the demuxer here yet.
    fn metadata(&mut self) -> SourceMetadata {
        SourceMetadata::default()
    }

    fn next_block(&mut self) -> Result<Option<Vec<i16>>, DecodeError> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(err) => return stream_error(err).map_or(Ok(None), Err),
            };
            // A container may hand out the packets of several tracks in turn, and the decoder
            // understands one of them.
            if packet.track_id() != self.track_id {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(decoded) => decoded,
                Err(err) => return stream_error(err).map_or(Ok(None), Err),
            };

            return Ok(Some(interleaved(decoded)));
        }
    }
}

/// The shape of a track's signal, or `None` when the track does not describe decodable audio.
///
/// A container states a codec's sample rate and channel count before a single packet is decoded,
/// and that is where the source's spec comes from — a track that states neither is not audio this
/// can read, whatever else it may be.
fn source_spec(params: &CodecParameters) -> Option<SourceSpec> {
    Some(SourceSpec {
        sample_rate: params.sample_rate?,
        channels: u16::try_from(params.channels?.count()).ok()?,
    })
}

/// A decoded block in the one shape [`AudioSource`] hands out.
///
/// A codec decodes into whatever sample format and layout it works in — 8 to 32 bits, integer or
/// float, planar or interleaved — and symphonia's own [`SampleBuffer`] is the conversion of every
/// one of those into interleaved `i16`. It is built per block rather than kept: the block is
/// handed out owned in any case, and a codec may change how much it decodes at once mid-stream, so
/// a buffer sized for the block in hand is the one that always fits it.
fn interleaved(decoded: AudioBufferRef<'_>) -> Vec<i16> {
    let mut samples = SampleBuffer::<i16>::new(decoded.capacity() as u64, *decoded.spec());
    samples.copy_interleaved_ref(decoded);
    samples.samples().to_vec()
}

/// What a failure while opening an input means.
///
/// Everything the probe and the codec registry refuse is a format this build cannot read, and
/// bytes that run out are part of that: a header that breaks off half way never identified a
/// format either. A read that fails for some other reason is the input itself being unreadable,
/// which is the caller's file rather than the caller's audio — as far as it is told apart at all,
/// since a source that cannot be read even once looks exactly like one holding no format.
fn open_error(err: Error) -> DecodeError {
    match err {
        Error::IoError(err) if err.kind() != ErrorKind::UnexpectedEof => DecodeError::Io(err),
        _ => DecodeError::UnsupportedFormat,
    }
}

/// What a failure while decoding means, where `None` means the stream simply ended.
///
/// An unexpected end of file is how symphonia says a stream ran out, and by then the input has
/// already proven itself a format, so it is the end and not a failure. A read that fails for any
/// other reason is the input going away mid-decode, which would silently truncate a book if it
/// were reported as the end; anything else is data that does not decode.
fn stream_error(err: Error) -> Option<DecodeError> {
    match err {
        Error::IoError(err) if err.kind() == ErrorKind::UnexpectedEof => None,
        Error::IoError(err) => Some(DecodeError::Io(err)),
        err => Some(DecodeError::Decode(err)),
    }
}
