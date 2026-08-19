//! The conversion engine: audio in, TAF out — decode, resample, silence-process, and
//! Opus-encode into `taf`'s writer.

pub mod chapters;
pub mod decode;
pub mod pcm;

pub use chapters::{resolve_chapters, ChapterError, ChapterMode};
pub use decode::{
    open_source, AudioSource, Cover, DecodeError, SourceChapter, SourceMetadata, SourceSpec,
};
pub use pcm::{Pcm48, PcmError, SilenceOpts, SilenceProcessor, SILENCE_THRESHOLD};
