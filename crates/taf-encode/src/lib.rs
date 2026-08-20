//! The conversion engine: audio in, TAF out — decode, resample, silence-process, and
//! Opus-encode into `taf`'s writer.
//!
//! [`convert`] is the whole of it: the inputs, the chapter plan and the silence operations go in,
//! and a finished file comes out of the other end. Everything else here is one stage of that, and
//! public because a stage is worth having on its own.

pub mod chapters;
pub mod convert;
pub mod decode;
mod encode;
pub mod pcm;

pub use chapters::{ChapterError, ChapterMode};
pub use convert::{
    convert, ChapterOut, Conversion, ConversionReport, ConvertError, Input, Progress,
};
pub use decode::{
    open_source, AudioSource, Cover, DecodeError, SourceChapter, SourceMetadata, SourceSpec,
};
pub use pcm::{Pcm48, PcmError, SilenceOpts, SilenceProcessor, SILENCE_THRESHOLD};
