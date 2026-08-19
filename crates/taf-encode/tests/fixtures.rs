//! The audio the decode tests run on, built here byte by byte.
//!
//! A WAV file is a RIFF container, and the smallest one that any decoder accepts is 44 bytes of
//! header — `RIFF`/`WAVE`, a 16-byte `fmt ` chunk, a `data` chunk header — followed by the samples
//! themselves, little-endian and interleaved. That is little enough to write out by hand, which is
//! what this module does: the tests depend on no encoder, no tool, and no file on disk, and the
//! bytes they decode are described in full right here.
//!
//! The tone is a 440 Hz sine, two seconds of it, at 44 100 Hz and 16 bits — a sample rate that is
//! *not* the 48 kHz a TAF ends up at, so nothing downstream can quietly assume the source already
//! arrives at the output rate. The two channels carry the same wave at different peaks
//! ([`LEFT_PEAK`] and [`RIGHT_PEAK`]): a decoder that swaps or collapses the channels, or that
//! reads planar samples as interleaved, gets the peaks wrong in a way a test can see.
//!
//! [`broken_adpcm_wav`] is the same idea turned around: a file that opens and then does not
//! decode.

// Every cast below is on a compile-time constant that fits its target, or on a sine bounded by the
// peak it was scaled with.
#![allow(clippy::cast_possible_truncation)]

use std::f64::consts::TAU;

/// The fixture's sample rate, in Hz.
pub const SAMPLE_RATE: u32 = 44_100;

/// The fixture's channel count.
pub const CHANNELS: u16 = 2;

/// How long the fixture plays, in seconds.
pub const DURATION_SECS: u32 = 2;

/// The frequency of the tone, in Hz.
pub const TONE_HZ: f64 = 440.0;

/// What the left channel peaks at: 0.6 × [`i16::MAX`], rounded down.
pub const LEFT_PEAK: i16 = 19_660;

/// What the right channel peaks at: 0.3 × [`i16::MAX`], rounded down — half of [`LEFT_PEAK`], so
/// the two channels stay apart wherever they are compared.
pub const RIGHT_PEAK: i16 = 9_830;

/// Frames in the fixture, one sample per channel each.
pub const FRAMES: u32 = SAMPLE_RATE * DURATION_SECS;

/// Samples in the fixture: [`FRAMES`] × [`CHANNELS`].
pub const SAMPLES: usize = FRAMES as usize * CHANNELS as usize;

/// Bytes per sample: 16-bit PCM.
const BYTES_PER_SAMPLE: u32 = 2;

/// The whole fixture as the bytes of a WAV file.
pub fn sine_wav() -> Vec<u8> {
    let data_len = SAMPLES as u32 * BYTES_PER_SAMPLE;
    let block_align = CHANNELS * BYTES_PER_SAMPLE as u16;

    let mut wav = Vec::with_capacity(44 + data_len as usize);

    // The RIFF header, whose length covers everything behind it.
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");

    // The format chunk: 16 bytes of PCM description.
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes()); // format tag 1: uncompressed PCM
    wav.extend_from_slice(&CHANNELS.to_le_bytes());
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * u32::from(block_align)).to_le_bytes()); // bytes per second
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&(BYTES_PER_SAMPLE as u16 * 8).to_le_bytes()); // bits per sample

    // The data chunk header, and behind it the samples.
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for frame in 0..FRAMES {
        let value = (TAU * TONE_HZ * f64::from(frame) / f64::from(SAMPLE_RATE)).sin();
        wav.extend_from_slice(&scaled(value, LEFT_PEAK).to_le_bytes());
        wav.extend_from_slice(&scaled(value, RIGHT_PEAK).to_le_bytes());
    }

    wav
}

/// One sample of the sine: its value in −1.0..=1.0, at the peak the channel is written with.
fn scaled(value: f64, peak: i16) -> i16 {
    (value * f64::from(peak)).round() as i16
}

/// A WAV whose header is sound and whose audio is not: one mono IMA ADPCM block that no decoder
/// can expand.
///
/// Every ADPCM block opens with a preamble of a predictor and a step index, and the step index
/// selects one of the 89 step sizes the codec is defined over — so a step index past those 89 is
/// data that cannot be decoded, while everything a demuxer looks at stays valid. That is a file
/// that gets as far as handing out a packet and then fails on it, which is the one thing a stream
/// of intact PCM can never do.
pub fn broken_adpcm_wav() -> Vec<u8> {
    // A block of 36 bytes is a 4-byte preamble and 32 bytes of nibble pairs: 64 encoded frames
    // plus the one the preamble's predictor carries.
    const BLOCK_ALIGN: u16 = 36;
    const FRAMES_PER_BLOCK: u16 = 65;
    // Past the 88 the codec's step table ends at.
    const INVALID_STEP_INDEX: u8 = 0xff;

    let data_len = u32::from(BLOCK_ALIGN);
    let fmt_len = 20_u32;

    let mut wav = Vec::with_capacity(12 + 8 + fmt_len as usize + 8 + data_len as usize);

    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(4 + (8 + fmt_len) + (8 + data_len)).to_le_bytes());
    wav.extend_from_slice(b"WAVE");

    // The format chunk: 16 bytes of description and the 4-byte extension IMA ADPCM requires.
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&fmt_len.to_le_bytes());
    wav.extend_from_slice(&0x0011_u16.to_le_bytes()); // format tag 0x11: IMA ADPCM
    wav.extend_from_slice(&1_u16.to_le_bytes()); // one channel
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(
        &(SAMPLE_RATE * u32::from(BLOCK_ALIGN) / u32::from(FRAMES_PER_BLOCK)).to_le_bytes(),
    ); // bytes per second
    wav.extend_from_slice(&BLOCK_ALIGN.to_le_bytes());
    wav.extend_from_slice(&4_u16.to_le_bytes()); // four bits per encoded sample
    wav.extend_from_slice(&2_u16.to_le_bytes()); // two bytes of extension follow
    wav.extend_from_slice(&FRAMES_PER_BLOCK.to_le_bytes());

    // The data chunk: one block, whose preamble names a step size the codec does not have.
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(&0_u16.to_le_bytes()); // predictor
    wav.push(INVALID_STEP_INDEX);
    wav.push(0); // reserved
    wav.resize(wav.len() + usize::from(BLOCK_ALIGN) - 4, 0);

    wav
}
