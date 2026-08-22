//! The conversion engine: audio in, TAF out — decode, resample, silence-process, and
//! Opus-encode into `taf`'s writer.
//!
//! [`convert()`] is the whole of it: the inputs, the chapter plan and the silence operations go in,
//! and a finished file comes out of the other end. Everything else here is one stage of that, and
//! public because a stage is worth having on its own.
//!
//! # The crates this one's types are made of
//!
//! An [`Input`] holds a `symphonia` media source, [`ConvertError::Encode`] is an `opus` error and
//! [`PcmError::Resample`] a `rubato` one — so those three crates are re-exported here, and a caller
//! reaches them through this crate rather than depending on them again at a version that need not
//! be the one these types came from.
//!
//! An input's reader is built out of the re-exported [`symphonia::core::io`] types: a
//! [`File`](std::fs::File) and a [`Cursor`](std::io::Cursor) over bytes in hand are media sources
//! as they stand, and anything else that reads goes through
//! [`ReadOnlySource`](symphonia::core::io::ReadOnlySource) — which cannot be seeked, and so cannot
//! carry the chapter marks an m4b states behind its audio.

pub mod chapters;
mod chunk;
pub mod convert;
pub mod decode;
mod encode;
pub mod pcm;
mod produce;

pub use opus;
pub use rubato;
pub use symphonia;

pub use chapters::{ChapterError, ChapterMode};
pub use convert::{
    convert, ChapterOut, Conversion, ConversionReport, ConvertError, Input, Progress,
};
pub use decode::{
    open_source, AudioSource, Cover, DecodeError, SourceChapter, SourceMetadata, SourceSpec,
};
pub use pcm::{Pcm48, PcmError, SilenceOpts, SilenceProcessor, SILENCE_THRESHOLD};
