//! What audio enters the converter through: [`AudioSource`], a stream of PCM pulled one block at a
//! time, and whatever the container said about the recording it came from.
//!
//! # One shape of samples
//!
//! Everything behind this module — resampling, silence processing, Opus encoding — works on
//! interleaved 16-bit samples, so this is where every input format converges on them. A source
//! keeps the sample *rate* and channel count it was authored at, which is what [`SourceSpec`]
//! states: resampling to the 48 kHz stereo a TAF carries is a later step's job, and it needs to
//! know what it is resampling from. The sample *format* is not kept: a 24-bit FLAC, a float WAV
//! and an AAC track all arrive here as `i16`.
//!
//! # A block at a time
//!
//! [`AudioSource::next_block`] hands out as many samples as the container's next packet held — a
//! few thousand, typically. Audiobooks are hours long, so nothing here reads a whole input into
//! memory, and no caller has to. The block length is not a promise: it differs between formats and
//! within one stream, and the only thing the trait states about it is where the stream *ends*,
//! which is `Ok(None)`.
//!
//! # Metadata is what the file says, not what the conversion decides
//!
//! [`SourceMetadata`] carries what the demuxer found and nothing that was inferred: chapter marks
//! and cover art, exactly as authored. Chapter starts are in samples *at the source's own rate*,
//! because before resampling that is the only frame of reference there is — what those marks
//! become in the output is settled once the pipeline knows what it did to the samples around them.
//!
//! # Which backend reads an input, and what decides
//!
//! There are two: one decodes the Opus an Ogg file carries through libopus, and one reads
//! everything else through symphonia. What an input *is* decides, and it is decided by the input's
//! own bytes — [`open_source`] is handed a reader and never a name, so no extension can send a
//! file down the wrong path, and one that lies about its contents is no worse off than one that
//! says nothing.
//!
//! The Opus sniff runs first, and has to: symphonia demuxes Ogg without being able to decode Opus,
//! so an Ogg-Opus file offered to its probe is claimed and only then refused — with the bytes the
//! other backend needs already read. An Ogg file of any other codec fails that sniff by carrying
//! another codec's magic where `OpusHead` would be, and goes on to symphonia as it always did.

mod aac_config;
mod mp4_chapters;
mod opus_input;
mod symphonia;

use ::symphonia::core::io::MediaSource;

/// Opens whatever the reader holds, if this build can decode it.
///
/// The input's own bytes decide what it is and which backend reads it: nothing here looks at a
/// file name, and there is no way to assert one.
///
/// # Errors
///
/// [`DecodeError::UnsupportedFormat`] when the input is not a format this build recognizes, or
/// holds a codec it cannot decode — which includes one whose shape nothing states until a packet
/// of it has been decoded, and an Opus stream whose channels take more decoders than one.
/// [`DecodeError::NoAudioTrack`] when it is a container but states no track of decodable audio.
/// [`DecodeError::Io`] when the input itself cannot be read, which includes an input that says it
/// can be rewound and then cannot.
pub fn open_source(mut reader: Box<dyn MediaSource>) -> Result<Box<dyn AudioSource>, DecodeError> {
    if opus_input::sniff(&mut *reader)? {
        return opus_input::open(reader);
    }

    symphonia::open(reader)
}

/// The shape of the signal a source decodes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SourceSpec {
    /// Samples per second per channel, as the container declares it.
    pub sample_rate: u32,
    /// How many channels each frame carries, and therefore how the samples of a block interleave.
    pub channels: u16,
}

/// A chapter mark the input carried.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SourceChapter {
    /// Where the chapter starts, counted in frames from the start of the stream and at the
    /// source's own sample rate — the rate [`SourceSpec::sample_rate`] states.
    pub start_sample: u64,
    /// What the input called the chapter, when it called it anything.
    pub title: Option<String>,
}

/// Cover art the input carried, in the encoding it was stored in.
///
/// Nothing here decodes or re-encodes the image: the bytes travel as they were found, and the MIME
/// type is what tells a consumer what they are.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cover {
    /// The image itself.
    pub bytes: Vec<u8>,
    /// The image's MIME type, such as `image/jpeg`.
    pub mime: String,
}

/// Everything a source knows about the recording that is not the samples themselves.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SourceMetadata {
    /// The chapter marks the input carried, in the order it carried them. Empty when it carried
    /// none — which is not the same as a book of one chapter, a decision that is made elsewhere.
    pub chapters: Vec<SourceChapter>,
    /// The cover art the input carried, if any.
    pub cover: Option<Cover>,
}

/// An audio input, decoded on demand.
pub trait AudioSource {
    /// The shape of the signal every block of this source is in.
    fn spec(&self) -> SourceSpec;

    /// What the container said about the recording, taken out of the demuxer.
    ///
    /// This takes `&mut self` because a demuxer may only reach its metadata by reading, and it may
    /// only reach all of it once the stream has been read through; a source is free to answer
    /// differently before and after [`next_block`](AudioSource::next_block) has run out.
    fn metadata(&mut self) -> SourceMetadata;

    /// The next block of interleaved samples, or `Ok(None)` at the end of the stream.
    ///
    /// # Errors
    ///
    /// Whatever went wrong reading or decoding the input. The end of the stream is not one of
    /// those: it is `Ok(None)`, and it is the only way a stream ends without something being
    /// wrong.
    fn next_block(&mut self) -> Result<Option<Vec<i16>>, DecodeError>;
}

/// Why an input could not be decoded.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DecodeError {
    /// The input is not a container this build recognizes, or holds a codec it cannot decode.
    #[error("unrecognized or unsupported audio format")]
    UnsupportedFormat,
    /// The input is a container, but states no track of decodable audio.
    #[error("no audio track in input")]
    NoAudioTrack,
    /// The input is a format this build reads, but the data in it does not decode.
    ///
    /// The source is the failure as the demuxer or the codec that ran into it stated it, and which
    /// of them that was is not part of what this says: an input holds audio that does not decode
    /// or it does not, and a caller that wants the detail can read the source or downcast it.
    #[error("decode failed")]
    Decode(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The input itself could not be read.
    #[error("i/o error reading input")]
    Io(#[from] std::io::Error),
}
