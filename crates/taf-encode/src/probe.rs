//! What an input *says* it plays, read off its headers alone: no packet is decoded, so probing a
//! shelf of books costs file opens and not conversions. The number is the container's own claim —
//! a percent drawn from it is an estimate, and the report a conversion hands back stays the truth.

use std::num::NonZeroU32;
use std::time::Duration;

use symphonia::core::codecs::CODEC_TYPE_OPUS;
use symphonia::core::formats::{FormatOptions, Track};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::decode::opus_input::sniff;
use crate::decode::symphonia::audio_track;

/// Why no duration could be stated.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProbeError {
    /// The bytes are no container this build recognizes.
    #[error("the input is not a format this build recognizes")]
    Unrecognized,
    /// A recognized container that states no length.
    #[error("the container states no duration")]
    NoDuration,
    /// The input itself is what went wrong rather than anything about its audio: it says it can be
    /// rewound and then cannot, or — for a caller that opens the file it probes — it could not be
    /// opened at all. Which is the failure a conversion of that same input reports too.
    #[error("reading the input failed")]
    Io(#[from] std::io::Error),
}

/// The length the container claims, without decoding any of it.
///
/// # Errors
///
/// [`ProbeError::Unrecognized`] for bytes no demuxer here claims, which is what an input whose
/// bytes cannot be read comes back as too: a source that gives nothing up looks exactly like one
/// holding no format. [`ProbeError::NoDuration`] where a container was read and states no length —
/// no track in it a conversion would read, no frame count, or no rate to count the frames at.
/// [`ProbeError::Io`] where the input says it can be rewound and then cannot, which leaves its
/// bytes somewhere nothing can read a container from.
pub fn probe_duration(mut source: Box<dyn MediaSource>) -> Result<Duration, ProbeError> {
    // Which backend a conversion would read this input with, asked of the same sniff and with the
    // input left where it was found. What it answers settles, further down, which track of the
    // container the length is counted from.
    let opus = sniff(&mut *source)?;
    let stream = MediaSourceStream::new(source, MediaSourceStreamOptions::default());
    // The setup a decoder opens an input with: the bytes decide what it is, and the padding an
    // encoder added is trimmed where the demuxer trims it — so what is stated here and what a
    // conversion goes on to report are a length of the same audio.
    let probed = symphonia::default::get_probe()
        .format(
            &Hint::new(),
            stream,
            &FormatOptions {
                enable_gapless: true,
                ..FormatOptions::default()
            },
            &MetadataOptions::default(),
        )
        .map_err(|_| ProbeError::Unrecognized)?;

    let track = book_track(probed.format.tracks(), opus).ok_or(ProbeError::NoDuration)?;
    let frames = track.codec_params.n_frames.ok_or(ProbeError::NoDuration)?;
    // Not every container states the rate of the audio it carries, and a rate of 0 is a rate
    // nothing plays at — which the arithmetic below would divide by. Both are a track there is no
    // counting frames at.
    let rate = track
        .codec_params
        .sample_rate
        .and_then(NonZeroU32::new)
        .ok_or(ProbeError::NoDuration)?;
    let rate = u64::from(rate.get());

    // Exact integer clockwork: whole seconds, then the frames left over scaled to nanoseconds.
    // Those are fewer than one second's worth, so scaling them stays inside u64 and what comes of
    // it fits u32 by construction.
    #[allow(clippy::cast_possible_truncation)]
    let nanos = ((frames % rate) * 1_000_000_000 / rate) as u32;

    Ok(Duration::new(frames / rate, nanos))
}

/// Which track of a container a conversion would decode, where `opus` is what the sniff made of the
/// input — the one thing that decides which backend reads it.
///
/// On the libopus route that is the Opus stream: symphonia demuxes Ogg without having a decoder for
/// what an Opus stream holds, so no track of such a file is one *it* would decode, and going by its
/// registry would state no length for a book a conversion reads start to finish. The stream libopus
/// reads is the one the file opens with, which the sniff has just found carrying an Opus head — so
/// the Opus track the demuxer lists is that stream.
///
/// On symphonia's route it is the first track there is a decoder for, since a container may lead
/// with something that is not the book: a track of video, or the cover picture and chapter text an
/// m4b carries as tracks of their own.
fn book_track(tracks: &[Track], opus: bool) -> Option<&Track> {
    if opus {
        return tracks
            .iter()
            .find(|track| track.codec_params.codec == CODEC_TYPE_OPUS);
    }

    audio_track(tracks)
}
