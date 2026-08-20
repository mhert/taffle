//! What a conversion is asked to make its chapters of, and why what it was asked for could be no
//! plan at all: [`ChapterMode`] and [`ChapterError`].
//!
//! The rules themselves live where the plan is made, which is [`convert`](mod@crate::convert) — a plan
//! is settled as the audio runs, since half of what one is held to is a length nothing knows until
//! the last frame is out. What is here is the request and the refusal, which are the two ends a
//! caller sees.
//!
//! # Frames at 48 kHz, and nothing scaled here
//!
//! Every offset stated in either type is a frame at 48 kHz: the unit
//! [`Pcm48::scale_samples`](crate::Pcm48::scale_samples) answers in, and the one
//! [`SilenceProcessor`](crate::SilenceProcessor) takes its chapter starts in. A mark comes out of a
//! container counted at the rate that container was authored at, and bringing it to 48 kHz is the
//! business of the stage that knows what it did to the samples around it.

/// How the chapters of a conversion are decided.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ChapterMode {
    /// The marks the input carried, where there is one input; one chapter per input, where there
    /// is more than one.
    #[default]
    Auto,
    /// The offsets the caller states, in frames at 48 kHz from the start of the conversion's
    /// audio — what `--chapters` parsed to. Nothing an input carries is consulted.
    Explicit(Vec<u64>),
}

/// Why a conversion has no chapter plan.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ChapterError {
    /// An explicit offset lies at or behind the end of the audio, where no chapter of it could
    /// begin.
    #[error("explicit chapter at {offset} beyond total length {total}")]
    OutOfRange {
        /// The offset that lies too far out, in frames at 48 kHz.
        offset: u64,
        /// How many frames the conversion's audio holds altogether.
        total: u64,
    },
    /// Two explicit offsets are in the same place, or the later of them lies in front of the
    /// earlier.
    #[error("chapter offsets must be strictly increasing")]
    NotSorted,
    /// There is nothing to convert: the conversion states no inputs at all.
    #[error("no inputs")]
    Empty,
}
