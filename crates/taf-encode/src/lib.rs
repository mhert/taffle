//! The conversion engine: audio in, TAF out — decode, resample, silence-process, and
//! Opus-encode into `taf`'s writer.

pub mod decode;

pub use decode::{
    open_source, AudioSource, Cover, DecodeError, SourceChapter, SourceMetadata, SourceSpec,
};
