//! [`AudioSource`] over symphonia: WAV, FLAC, MP3, AAC in MP4/M4B, and Vorbis in Ogg — every input
//! format the converter takes apart from Opus itself, which is decoded a module over.
//!
//! # The format is decided by the bytes
//!
//! [`open`] hands the probe an empty [`Hint`]. A caller may have no file name to offer at all — a
//! stream, a buffer, an upload — and an extension that lies is worse than one that is missing, so
//! what the input starts with is what decides. That also means garbage is recognized as garbage
//! before anything tries to decode it.
//!
//! # What this build reads, and what it happens to
//!
//! The formats above are what the crate's manifest asks symphonia for by name, and what the tests
//! pin. symphonia's own defaults are on as well, so the probe also accepts the MKV and `WebM`
//! containers they bring — untested here, and neither refused nor promised. What the manifest does
//! *not* leave to a default is a decoder the tests depend on: the PCM and ADPCM ones a WAV needs
//! are asked for by name, so a file that decodes today does not stop decoding because a dependency
//! changed what it considers standard.
//!
//! # Which errors are which
//!
//! symphonia reports the end of a stream as an unexpected end of file, so the same error type
//! carries both "there is no more audio", which is how every successful conversion finishes, and
//! "the input broke off", which no caller should be told is a clean end. The two functions at the
//! bottom of this module are that distinction, and they draw it differently in the two places it
//! matters: while opening, bytes that run out mean the input was never a format to begin with;
//! while decoding, they mean the stream is over.
//!
//! # What an encoder added is not what an author recorded
//!
//! Both lossy codecs here start a stream with frames the encoder needed to get going and end it
//! with frames it needed to fill the last block, and both write down how many. [`open`] asks for
//! those to be trimmed ([`FormatOptions::enable_gapless`]), because an audiobook is converted from
//! files an author's chapters were cut into, and silence the encoder added at every cut would end
//! up in the middle of the book. The fixtures pin what that means today: an MP3 of ten authored
//! seconds decodes to exactly ten seconds' worth of frames, while symphonia's MP4 demuxer does not
//! implement the option and hands out the AAC priming frames whatever it is set to — under 25 ms
//! of them, which is also the margin a chapter mark in an m4b can land within.
//!
//! # The shape of a stream, and who knows it
//!
//! A container states the sample rate of its audio, and most of them state the channel count too;
//! an MP4 does not, because AAC keeps that in the codec's own configuration. Neither does the AAC
//! decoder pass it back out — but it builds the buffer it decodes into from it, and that buffer is
//! reachable before it holds anything. So the shape a source reports is what the container states,
//! and where the container states nothing, the shape the decoder set itself up for. It is the same
//! buffer every block is copied out of, so nothing else could be a better answer.

use std::borrow::Cow;
use std::io::ErrorKind;

use symphonia::core::audio::{AudioBufferRef, SampleBuffer, SignalSpec};
use symphonia::core::codecs::{CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_AAC};
use symphonia::core::errors::Error;
use symphonia::core::formats::{FormatOptions, FormatReader, Track};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::{Metadata, MetadataOptions, MetadataRevision};
use symphonia::core::probe::Hint;
use symphonia::default::{get_codecs, get_probe};

use super::aac_config;
use super::mp4_chapters::{self, Mp4Chapter};
use super::{AudioSource, Cover, DecodeError, SourceChapter, SourceMetadata, SourceSpec};

/// Opens whatever the reader holds, if this build can decode it.
///
/// The input's own bytes decide what it is: nothing here looks at a file name, and there is no way
/// to assert one.
///
/// # Errors
///
/// [`DecodeError::UnsupportedFormat`] when the input is not a container this build recognizes, or
/// holds a codec it cannot decode — which includes one whose shape nothing states until a packet
/// of it has been decoded. [`DecodeError::NoAudioTrack`] when it is a container but states no
/// track of decodable audio. [`DecodeError::Io`] when the input itself cannot be read, which
/// includes an input that says it can be rewound and then cannot.
pub(super) fn open(mut reader: Box<dyn MediaSource>) -> Result<Box<dyn AudioSource>, DecodeError> {
    // The chapter marks are read off the raw input, before a demuxer that does not read them owns
    // it. What comes back is in the container's own units, and stays that way until the rate the
    // stream is decoded at is settled below.
    let marks = mp4_chapters::read(&mut *reader)?;

    let stream = MediaSourceStream::new(reader, MediaSourceStreamOptions::default());
    let mut probed = get_probe()
        .format(
            &Hint::new(),
            stream,
            &FormatOptions {
                enable_gapless: true,
                ..FormatOptions::default()
            },
            &MetadataOptions::default(),
        )
        .map_err(open_error)?;

    let mut format = probed.format;
    let (track_id, spec, decoder) = {
        let track = audio_track(format.tracks()).ok_or(DecodeError::NoAudioTrack)?;
        // Building a decoder reads nothing but the codec's own configuration, which the container
        // stated and the probe already got through: whatever it refuses, it refuses about the
        // codec and not about the input's bytes.
        let decoder = get_codecs()
            .make(&decodable(&track.codec_params), &DecoderOptions::default())
            .map_err(|_| DecodeError::UnsupportedFormat)?;
        // A track whose shape neither the container nor the decoder states is audio all right —
        // it just cannot be described without decoding it, which is not what a source promises.
        let spec = source_spec(&track.codec_params, *decoder.last_decoded().spec())
            .ok_or(DecodeError::UnsupportedFormat)?;
        (track.id, spec, decoder)
    };

    // An ID3 tag in front of an MP3's audio is read by the probe rather than by the demuxer behind
    // it, and an MP4's metadata by the demuxer rather than by the probe. Both are asked, so that
    // neither format's cover art depends on which of the two found it.
    let cover = cover_of(probed.metadata.get().as_ref().and_then(Metadata::current))
        .or_else(|| cover_of(format.metadata().current()));

    Ok(Box::new(SymphoniaSource {
        format,
        decoder,
        track_id,
        spec,
        metadata: SourceMetadata {
            chapters: marks
                .into_iter()
                .map(|mark| at_rate(mark, spec.sample_rate))
                .collect(),
            cover,
        },
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
    /// What the track's signal was settled to be when it was opened.
    spec: SourceSpec,
    /// What the container said about the recording, read while it was opened.
    metadata: SourceMetadata,
}

impl AudioSource for SymphoniaSource {
    fn spec(&self) -> SourceSpec {
        self.spec
    }

    /// Everything found while opening the input, which for these containers is everything there
    /// is: an MP4 states its chapters and its cover art in the header the demuxer needs anyway,
    /// and an MP3 carries its tag in front of the audio.
    fn metadata(&mut self) -> SourceMetadata {
        self.metadata.clone()
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

/// The first track of a container this build can decode as audio.
///
/// symphonia's codec registry holds nothing but audio codecs, so a track it has a decoder for is
/// an audio track and a track it does not know is not one — the cover picture and the chapter text
/// an m4b carries as tracks of their own among them. Taking the first such track rather than the
/// container's first track is what keeps those from being mistaken for the book.
fn audio_track(tracks: &[Track]) -> Option<&Track> {
    tracks
        .iter()
        .find(|track| get_codecs().get_codec(track.codec_params.codec).is_some())
}

/// The parameters to build a decoder from: the track's own, unless its codec configuration states
/// something no decoder can read and this is the one case where what it meant is beyond doubt.
///
/// Nothing but AAC is touched, and of AAC only what [`aac_config::repaired`] describes: the
/// configuration every audiobook here carries, which announces a core coder dependency in a
/// configuration too short to describe one. What the file says is otherwise what gets used, down
/// to the byte.
fn decodable(params: &CodecParameters) -> Cow<'_, CodecParameters> {
    if params.codec != CODEC_TYPE_AAC {
        return Cow::Borrowed(params);
    }
    let Some(config) = params.extra_data.as_deref().and_then(aac_config::repaired) else {
        return Cow::Borrowed(params);
    };

    let mut repaired = params.clone();
    repaired.with_extra_data(config.into_boxed_slice());

    Cow::Owned(repaired)
}

/// The shape of a track's signal, or `None` when nothing states how many channels it carries.
///
/// The container is asked first and the decoder's own buffer second, which is the only thing that
/// knows an AAC track's channel count before a packet is decoded. A stream whose channel count
/// neither of them states is one whose shape could only be learned by decoding it, and a source
/// states its shape before it decodes anything.
fn source_spec(params: &CodecParameters, decoded: SignalSpec) -> Option<SourceSpec> {
    let channels = params.channels.unwrap_or(decoded.channels).count();

    Some(SourceSpec {
        sample_rate: params.sample_rate.unwrap_or(decoded.rate),
        channels: u16::try_from(channels).ok().filter(|count| *count > 0)?,
    })
}

/// Where a chapter mark falls in a stream of `sample_rate` frames a second.
///
/// The mark is the container's own timestamp, which counts from the first frame the file was
/// authored with. What a decoder hands out may begin a little before that — see the encoder's
/// priming frames, above — and nothing in the file says by how much, so the mark is passed on as
/// stated. A start further out than a stream of a hundred million years could reach is pinned at
/// what fits.
fn at_rate(mark: Mp4Chapter, sample_rate: u32) -> SourceChapter {
    /// How many of the units `chpl` counts in go into a second.
    const PER_SECOND: u64 = 10_000_000;

    SourceChapter {
        start_sample: mark.start_100ns.saturating_mul(u64::from(sample_rate)) / PER_SECOND,
        title: mark.title,
    }
}

/// The cover art a revision of metadata holds, in the encoding it was stored in.
///
/// A revision holds every picture the container carried, in the order it carried them, and an
/// audiobook's cover is the first of them: both an MP4's `covr` atom and an ID3 tag's picture
/// frame put the front cover there. Nothing here re-encodes it — the bytes and the media type are
/// what the file stated, so a consumer gets exactly the image the file carried.
fn cover_of(revision: Option<&MetadataRevision>) -> Option<Cover> {
    let visual = revision?.visuals().first()?;

    Some(Cover {
        bytes: visual.data.to_vec(),
        mime: visual.media_type.clone(),
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

/// What a failure while probing an input means.
///
/// Everything the probe refuses is a format this build cannot read, and bytes that run out are
/// part of that: a header that breaks off half way never identified a format either. A read that
/// fails for some other reason is the input itself being unreadable, which is the caller's file
/// rather than the caller's audio — as far as it is told apart at all, since a source that cannot
/// be read even once looks exactly like one holding no format.
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
        err => Some(DecodeError::Decode(Box::new(err))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use symphonia::core::codecs::{CodecType, CODEC_TYPE_AAC, CODEC_TYPE_MP3};

    use super::{decodable, CodecParameters};

    /// An AAC configuration stating a core coder dependency it has no room to describe.
    const VESTIGIAL: [u8; 2] = [0x12, 0x12];

    /// The same configuration with nothing to repair.
    const CANONICAL: [u8; 2] = [0x12, 0x10];

    /// A track of `codec` whose codec configuration is `config`.
    fn track_of(codec: CodecType, config: &[u8]) -> CodecParameters {
        let mut params = CodecParameters::new();
        params
            .for_codec(codec)
            .with_extra_data(config.to_vec().into_boxed_slice());

        params
    }

    #[test]
    fn an_aac_configuration_that_cannot_be_read_is_repaired_before_it_is_used() {
        let params = track_of(CODEC_TYPE_AAC, &VESTIGIAL);

        let decodable = decodable(&params);

        assert_eq!(decodable.extra_data.as_deref(), Some(&CANONICAL[..]));
    }

    #[test]
    fn the_configuration_of_another_codec_is_left_alone() {
        // The same bytes mean something else entirely behind another codec, and are not this
        // module's to rewrite.
        let params = track_of(CODEC_TYPE_MP3, &VESTIGIAL);

        let decodable = decodable(&params);

        assert_eq!(decodable.extra_data.as_deref(), Some(&VESTIGIAL[..]));
    }

    #[test]
    fn a_track_stating_no_configuration_at_all_is_left_alone() {
        let params = CodecParameters::new();

        let decodable = decodable(&params);

        assert!(decodable.extra_data.is_none());
    }
}
