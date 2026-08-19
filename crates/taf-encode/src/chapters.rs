//! Which chapters a conversion has: [`resolve_chapters`] turns what its inputs carry — the marks
//! an m4b was authored with, the boundaries between the files themselves — or what the caller
//! asked for instead into the one list of offsets everything behind it works from.
//!
//! # Frames at 48 kHz, and nothing scaled here
//!
//! Every count that goes in and every offset that comes out is a frame at 48 kHz: the unit
//! [`Pcm48::scale_samples`](crate::Pcm48::scale_samples) answers in, and the one
//! [`SilenceProcessor`](crate::SilenceProcessor) takes its chapter starts in. A mark comes out of
//! a container counted at the rate that container was authored at, and bringing it and the length
//! of the input it came from to 48 kHz is the caller's, in front of this. What is left here is
//! arithmetic on counts, which is why this is a function and not a stage.
//!
//! # The three ways a plan comes about
//!
//! 1. **The caller states it.** [`ChapterMode::Explicit`] is what `--chapters` parsed to, and it
//!    overrides everything: neither the marks an input carries nor the boundaries between the
//!    inputs are consulted.
//! 2. **One input, and its own marks.** [`ChapterMode::Auto`] over a single input takes the marks
//!    that input carried — an m4b's chapter atom, and whatever else a container states.
//! 3. **More than one input, one chapter each.** [`ChapterMode::Auto`] over several inputs puts a
//!    chapter where each of them begins, and does not look at the marks they carry.
//!
//! # Why several files are the chapters and their own marks are not
//!
//! Somebody who hands a converter twelve files has stated what the chapters are by handing over
//! twelve files. Reading the marks inside them as well would put chapters where nobody asked for
//! any — and it could only ever be a mix of the two, since a set where one file carries marks and
//! eleven carry none is the ordinary case rather than the odd one. So the boundaries win whole:
//! that is one rule instead of a per-file lottery, and the way to have a file's own marks used is
//! to convert that file on its own or to state the plan outright.
//!
//! # What every plan holds
//!
//! A plan begins at offset 0, its offsets strictly increase, and every offset behind the first
//! lies in front of the end of the audio. The first two are what a chapter table *is* — a TAF's
//! first chapter begins where its audio does, and two chapters in one place are one chapter — and
//! the third is what makes every offset a place where there is something to play.
//!
//! What a later stage then does with the plan is its own: the silence operations can move two
//! chapters onto the same frame by trimming everything between them away, and that is theirs to
//! state. A plan comes out of here strictly increasing.
//!
//! # A mark is advisory, an offset the caller states is not
//!
//! An offset that a plan cannot hold — one at or behind the end of the audio, one that does not
//! lie behind the offset in front of it — is dropped where it came out of a file and refused where
//! the caller stated it. What separates them is what the answer is worth to whoever gets it: a
//! container's chapter atom is not something its owner can correct, so a book whose marks are half
//! nonsense is still a book to convert, with the chapters of it that do make sense; an explicit
//! plan is what somebody typed a moment ago, so an offset in it that cannot be a chapter is a
//! mistake to state plainly rather than to quietly leave out.
//!
//! Refused means the first offset that cannot be a chapter, read left to right as the caller wrote
//! them: one at or behind the end of the audio is [`ChapterError::OutOfRange`] and names itself,
//! one that does not lie behind the offset in front of it is [`ChapterError::NotSorted`]. No
//! offset is ever both — an offset out of order is at most the offset in front of it, which was in
//! range already.
//!
//! # The chapter a book has whatever is in it
//!
//! Offset 0 is in range even where there is no audio at all, because the first chapter of a TAF is
//! not something a plan chooses: it begins where the file's audio begins, and a file whose audio is
//! empty still has that one chapter. So an input that decodes to nothing resolves to `[0]` under
//! either mode, rather than to an error about a chapter at 0 lying behind an end at 0. Every other
//! offset has to be a place there is audio at — which is also why an input of no length begins no
//! chapter of its own in a set of them: the chapter it would begin is the one the file behind it
//! begins, and a plan holds a place once.

/// How the chapters of a conversion are decided.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ChapterMode {
    /// The marks the input carried, where there is one input; one chapter per input, where there
    /// is more than one.
    #[default]
    Auto,
    /// The offsets the caller states, in frames at 48 kHz from the start of the conversion's
    /// audio — what `--chapters` parsed to. Nothing an input carries is consulted.
    Explicit(Vec<u64>),
}

/// The chapter plan of a conversion: where every chapter of its output begins.
///
/// `inputs` is one entry per input file, in the order they are concatenated: how many frames that
/// input decoded to, and the chapter marks it carried, counted from its own start. Both are frames
/// at 48 kHz, so both have been through [`Pcm48::scale_samples`](crate::Pcm48::scale_samples)
/// already. That the marks are per input and the plan is absolute makes no difference to anything:
/// the only mode that reads them has a single input, whose start is the start of the audio.
///
/// The plan that comes back begins at offset 0, strictly increases, and states nothing at or
/// behind the end of the audio.
///
/// # Errors
///
/// [`ChapterError::Empty`] where there are no inputs at all, which is nothing to convert.
/// [`ChapterError::OutOfRange`] and [`ChapterError::NotSorted`] where the offsets of
/// [`ChapterMode::Explicit`] are no plan. A mark an input carried is dropped rather than refused,
/// so [`ChapterMode::Auto`] fails only on inputs that are not there.
pub fn resolve_chapters(
    inputs: &[(u64, Vec<u64>)],
    mode: &ChapterMode,
) -> Result<Vec<u64>, ChapterError> {
    if inputs.is_empty() {
        return Err(ChapterError::Empty);
    }
    let total = inputs
        .iter()
        .fold(0_u64, |total, (frames, _)| total.saturating_add(*frames));

    match (mode, inputs) {
        (ChapterMode::Explicit(offsets), _) => checked(offsets, total),
        (ChapterMode::Auto, [(_, marks)]) => Ok(plan(marks.iter().copied(), total)),
        (ChapterMode::Auto, _) => Ok(plan(starts(inputs), total)),
    }
}

/// Where each input of a conversion begins, in frames from the start of the first of them.
///
/// The end of the last input is not among them: this states where audio begins, and there is none
/// behind the end of it.
fn starts(inputs: &[(u64, Vec<u64>)]) -> impl Iterator<Item = u64> + '_ {
    inputs.iter().scan(0_u64, |start, (frames, _)| {
        let begins = *start;
        *start = start.saturating_add(*frames);

        Some(begins)
    })
}

/// The plan `candidates` make, with every one of them a plan cannot hold left out: one at or
/// behind the end of the audio, and one that does not lie behind the candidate kept in front of
/// it.
///
/// The plan begins at offset 0 whether the candidates do or not, so a candidate of 0 is one the
/// plan holds already.
fn plan(candidates: impl Iterator<Item = u64>, total: u64) -> Vec<u64> {
    let mut chapters = vec![0];
    let mut last = 0;
    for candidate in candidates {
        if candidate > last && candidate < total {
            chapters.push(candidate);
            last = candidate;
        }
    }

    chapters
}

/// The plan `offsets` make, or why they are none.
///
/// They are read left to right and the first offset that cannot be a chapter is the one stated:
/// one at or behind the end of the audio, or one that does not lie behind the offset in front of
/// it. Offset 0 is in range whatever the length is — the first chapter of a TAF begins where its
/// audio does, and a book with no audio in it still has that chapter — and is the one offset the
/// plan holds without having been stated.
fn checked(offsets: &[u64], total: u64) -> Result<Vec<u64>, ChapterError> {
    let mut previous = None;
    for offset in offsets.iter().copied() {
        if offset >= total && offset != 0 {
            return Err(ChapterError::OutOfRange { offset, total });
        }
        if previous.is_some_and(|earlier| offset <= earlier) {
            return Err(ChapterError::NotSorted);
        }
        previous = Some(offset);
    }

    let mut chapters = Vec::with_capacity(offsets.len() + 1);
    if offsets.first() != Some(&0) {
        chapters.push(0);
    }
    chapters.extend_from_slice(offsets);

    Ok(chapters)
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::{resolve_chapters, ChapterMode};

    /// An input of `frames` frames that carries no marks.
    fn plain(frames: u64) -> (u64, Vec<u64>) {
        (frames, Vec::new())
    }

    /// An input of `frames` frames that carries `marks`.
    fn marked(frames: u64, marks: &[u64]) -> (u64, Vec<u64>) {
        (frames, marks.to_vec())
    }

    /// The plan a mode makes of inputs, held against the three rules every plan holds to: it
    /// begins at offset 0, its offsets strictly increase, and every offset behind the first lies
    /// in front of the end of the audio.
    fn plan_of(inputs: &[(u64, Vec<u64>)], mode: &ChapterMode) -> Vec<u64> {
        let total = inputs
            .iter()
            .fold(0_u64, |total, (frames, _)| total.saturating_add(*frames));
        let chapters = resolve_chapters(inputs, mode).unwrap();

        assert_eq!(
            chapters.first(),
            Some(&0),
            "a plan begins where the audio does: {chapters:?}"
        );
        assert!(
            chapters.is_sorted_by(|earlier, later| earlier < later),
            "a plan's offsets strictly increase: {chapters:?}"
        );
        assert!(
            chapters.iter().skip(1).all(|offset| *offset < total),
            "every chapter behind the first begins in front of the end of {total}: {chapters:?}"
        );

        chapters
    }

    /// Why a mode was refused, in the words the refusal states it in — which is what a caller of
    /// the converter is given, and so what these tests hold it to.
    fn refusal(inputs: &[(u64, Vec<u64>)], mode: &ChapterMode) -> String {
        resolve_chapters(inputs, mode).unwrap_err().to_string()
    }

    #[test]
    fn the_marks_a_single_input_carries_are_the_chapters_it_has() {
        let inputs = [marked(480_000, &[0, 120_000, 300_000])];

        assert_eq!(plan_of(&inputs, &ChapterMode::Auto), [0, 120_000, 300_000]);
    }

    #[test]
    fn a_single_input_that_carries_no_marks_is_one_chapter() {
        let inputs = [plain(480_000)];

        assert_eq!(plan_of(&inputs, &ChapterMode::Auto), [0]);
    }

    #[test]
    fn marks_that_do_not_begin_at_the_start_are_given_the_start() {
        let inputs = [marked(480_000, &[120_000, 300_000])];

        assert_eq!(plan_of(&inputs, &ChapterMode::Auto), [0, 120_000, 300_000]);
    }

    #[test]
    fn every_input_of_a_conversion_that_has_more_than_one_begins_a_chapter() {
        let inputs = [plain(100), plain(250), plain(40)];

        assert_eq!(plan_of(&inputs, &ChapterMode::Auto), [0, 100, 350]);
    }

    #[test]
    fn the_marks_an_input_carries_are_not_read_where_there_is_more_than_one_input() {
        let inputs = [plain(100), marked(250, &[0, 60, 120]), plain(40)];

        assert_eq!(plan_of(&inputs, &ChapterMode::Auto), [0, 100, 350]);
    }

    #[test]
    fn a_mark_that_cannot_be_a_chapter_is_dropped_rather_than_refused() {
        // A mark where the audio ended and one behind it: there is nothing there to begin.
        let beyond = [marked(1_000, &[200, 1_000, 4_000])];
        assert_eq!(plan_of(&beyond, &ChapterMode::Auto), [0, 200]);

        // A mark repeated, and one that goes backwards: a place already marked is marked once.
        let backwards = [marked(1_000, &[200, 200, 150, 300])];
        assert_eq!(plan_of(&backwards, &ChapterMode::Auto), [0, 200, 300]);

        // And marks that are nothing else: the book is still one chapter rather than an error.
        let nonsense = [marked(1_000, &[1_000, 0, 1_000])];
        assert_eq!(plan_of(&nonsense, &ChapterMode::Auto), [0]);
    }

    #[test]
    fn an_input_with_no_audio_in_it_begins_no_chapter() {
        // The chapter it would begin is the one the file behind it begins, held once.
        let between = [plain(100), plain(0), plain(250)];
        assert_eq!(plan_of(&between, &ChapterMode::Auto), [0, 100]);

        // One at the front of the set does not move the first chapter anywhere.
        let leading = [plain(0), plain(250)];
        assert_eq!(plan_of(&leading, &ChapterMode::Auto), [0]);

        // And one at the end of it would begin where the audio ended, which is no chapter either.
        let trailing = [plain(250), plain(0)];
        assert_eq!(plan_of(&trailing, &ChapterMode::Auto), [0]);
    }

    #[test]
    fn an_explicit_plan_that_does_not_begin_at_the_start_is_given_the_start() {
        let inputs = [plain(1_000)];

        assert_eq!(
            plan_of(&inputs, &ChapterMode::Explicit(vec![250, 700])),
            [0, 250, 700]
        );
        // One that does begin there is taken as it is, rather than given a second one.
        assert_eq!(
            plan_of(&inputs, &ChapterMode::Explicit(vec![0, 250])),
            [0, 250]
        );
        // And an explicit plan of no offsets at all is the one chapter every plan has.
        assert_eq!(plan_of(&inputs, &ChapterMode::Explicit(Vec::new())), [0]);
    }

    #[test]
    fn an_explicit_plan_overrides_the_marks_and_the_file_boundaries_both() {
        let inputs = [marked(600, &[0, 200]), marked(400, &[0, 100])];

        assert_eq!(
            plan_of(&inputs, &ChapterMode::Explicit(vec![0, 500])),
            [0, 500]
        );
    }

    #[test]
    fn explicit_offsets_that_do_not_strictly_increase_are_refused() {
        let inputs = [plain(1_000)];

        for offsets in [
            vec![250, 100],
            vec![250, 250],
            vec![0, 0, 250],
            vec![0, 250, 250],
        ] {
            assert_eq!(
                refusal(&inputs, &ChapterMode::Explicit(offsets.clone())),
                "chapter offsets must be strictly increasing",
                "{offsets:?} is no plan"
            );
        }
    }

    #[test]
    fn an_explicit_offset_at_or_behind_the_end_of_the_audio_is_refused() {
        let inputs = [plain(600), plain(400)];

        // The last frame there is can begin a chapter.
        assert_eq!(
            plan_of(&inputs, &ChapterMode::Explicit(vec![999])),
            [0, 999]
        );
        // Where the audio ended cannot, and neither can anything behind it.
        for beyond in [1_000, 1_001, u64::MAX] {
            assert_eq!(
                refusal(&inputs, &ChapterMode::Explicit(vec![beyond])),
                format!("explicit chapter at {beyond} beyond total length 1000")
            );
        }
    }

    #[test]
    fn the_first_offset_that_cannot_be_a_chapter_is_the_one_stated() {
        let inputs = [plain(1_000)];

        // Two offsets behind the end of the audio: the one written first is the one named.
        assert_eq!(
            refusal(&inputs, &ChapterMode::Explicit(vec![100, 2_000, 3_000])),
            "explicit chapter at 2000 beyond total length 1000"
        );

        // And a plan that goes backwards in front of where it runs off the end is out of order:
        // the offsets read left to right, and the reading stops where it first cannot go on.
        assert_eq!(
            refusal(&inputs, &ChapterMode::Explicit(vec![100, 90, 5_000])),
            "chapter offsets must be strictly increasing"
        );
    }

    #[test]
    fn a_conversion_with_no_audio_in_it_still_begins_with_a_chapter() {
        let silent = [plain(0)];

        assert_eq!(plan_of(&silent, &ChapterMode::Auto), [0]);
        // The offset every plan has is in range whatever the length is; one behind it is not.
        assert_eq!(plan_of(&silent, &ChapterMode::Explicit(vec![0])), [0]);
        assert_eq!(
            refusal(&silent, &ChapterMode::Explicit(vec![1])),
            "explicit chapter at 1 beyond total length 0"
        );
    }

    #[test]
    fn a_conversion_with_no_inputs_has_no_plan() {
        for mode in [ChapterMode::Auto, ChapterMode::Explicit(vec![0])] {
            assert_eq!(refusal(&[], &mode), "no inputs");
        }
    }

    #[test]
    fn a_conversion_longer_than_the_count_can_hold_does_not_wrap_around() {
        // Nothing reaches this — it is twelve million years of audio — but a total that wrapped
        // would put chapters in front of an end that came out shorter than the files in front of
        // it, which is a plan of places that are not there.
        let inputs = [plain(u64::MAX), plain(2), plain(5)];

        assert_eq!(plan_of(&inputs, &ChapterMode::Auto), [0]);
    }

    #[test]
    fn the_mode_of_a_conversion_that_states_none_reads_the_inputs() {
        let inputs = [marked(1_000, &[400])];

        assert_eq!(plan_of(&inputs, &ChapterMode::default()), [0, 400]);
    }
}
