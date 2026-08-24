//! What an input *says* it plays, read off its headers alone: no packet is decoded, so probing a
//! shelf of books costs file opens and not conversions. The number is the container's own claim —
//! a percent drawn from it is an estimate, and the report a conversion hands back stays the truth.

use std::num::NonZeroU32;
use std::time::Duration;

use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

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
    /// The input could not be read at all. The probe itself never states this — it is here for a
    /// caller that opens the file it probes, so that failing to open one and failing to read a
    /// length out of it are the same error.
    #[error("reading the input failed")]
    Io(#[from] std::io::Error),
}

/// The length the container claims, without decoding any of it.
///
/// # Errors
///
/// [`ProbeError::Unrecognized`] for bytes no demuxer here claims, which is what an input that
/// cannot be read at all comes back as too: a source that gives nothing up looks exactly like one
/// holding no format. [`ProbeError::NoDuration`] where a container was read and states no length —
/// no track of audio in it, no frame count, or no rate to count the frames at.
pub fn probe_duration(source: Box<dyn MediaSource>) -> Result<Duration, ProbeError> {
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

    // The track a conversion would decode, found the way a conversion finds it: the first one this
    // build has a decoder for. A container may lead with something that is not the book — a track
    // of video, or the cover picture and chapter text an m4b carries as tracks of their own — and a
    // length is a length of the audio those stand in front of.
    let track = audio_track(probed.format.tracks()).ok_or(ProbeError::NoDuration)?;
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
