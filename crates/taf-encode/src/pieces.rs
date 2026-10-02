//! Where a book is cut into pieces, settled before any of it is converted: [`plan_pieces`] takes
//! what the inputs state about themselves and names the chapter every piece begins at.
//!
//! # A plan names chapters, and a length only chooses between them
//!
//! How long an input plays is not known until it has been read to the end, and a conversion reads
//! it once. What is known in front of the audio is what the containers *state*: a length, and the
//! chapter marks. That is enough to choose which chapter lies nearest to an even split — and the
//! choice is all the stated numbers are used for. The cut itself is made where that chapter
//! begins in the audio, so a header that is a second off moves no cut by a frame.
//!
//! # The rule
//!
//! What is split is the audio between the leading skip and the trailing one. The places a cut may
//! fall are the chapter starts strictly inside it, each place once however many chapters begin
//! there. Cut `i` of the `n - 1` there are aims at `i / n` of the way through and goes to the
//! place nearest to that — the earlier of two that are equally near — among those behind the cut
//! in front of it that still leave a place for every cut to come.
//!
//! So a book with at least as many chapters as pieces comes out as exactly that many pieces, the
//! cuts strictly increase, and one very long chapter cannot draw two cuts onto one place.
//!
//! # What the lengths in a plan are worth
//!
//! They are the distances between the cuts as the headers state them. A silence that is trimmed
//! and a pause that is put in are not in them, and a header's length is a claim: they are what a
//! frontend shows as an estimate, and the report of the conversion is what the pieces came to.
//!
//! # The chapters are the conversion's own
//!
//! The plan is made of the same chapters a conversion makes, by the same code: a list the caller
//! stated, the marks of one input put in order, or one chapter per input. So chapter `k` here is
//! chapter `k` there, which is what lets [`PiecePlan::starts`] be handed to
//! [`convert_pieces`](crate::convert_pieces()) as it stands.

use std::num::NonZeroUsize;

use crate::chapters::ChapterMode;
use crate::convert::Conversion;
use crate::produce::{authored, stated, Chapter};

/// What one input states about itself in its headers, in frames at 48 kHz.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Layout {
    /// How long the input states it plays.
    pub frames: u64,
    /// Where the chapter marks it carries begin, in the order it states them.
    pub marks: Vec<u64>,
}

/// One piece of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedPiece {
    /// The chapter the piece begins at, counted over the conversion's whole chapter plan. The
    /// first piece begins at chapter 0.
    pub first_chapter: usize,
    /// How many chapters the piece holds, counting the chapters that begin in one place once.
    pub chapters: usize,
    /// How long the headers say the piece plays, in frames at 48 kHz.
    pub frames: u64,
}

/// Where a book is cut: its pieces, in the order they play.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiecePlan {
    /// The pieces. Never empty.
    pub pieces: Vec<PlannedPiece>,
}

impl PiecePlan {
    /// The chapters that begin a piece behind the first: what
    /// [`convert_pieces`](crate::convert_pieces()) cuts at.
    #[must_use]
    pub fn starts(&self) -> Vec<usize> {
        self.pieces
            .iter()
            .skip(1)
            .map(|piece| piece.first_chapter)
            .collect()
    }
}

/// Why a book cannot be cut into the pieces asked for.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PieceError {
    /// There are fewer places to cut than cuts to make.
    #[error("{pieces} pieces asked for, but the book has {chapters} {}", noun(*chapters))]
    TooFewChapters {
        /// How many pieces were asked for.
        pieces: usize,
        /// How many chapters there are to make pieces of.
        chapters: usize,
    },
    /// The skips leave no audio between them.
    #[error("nothing is left to split once the leading and trailing skips are taken off")]
    NothingLeft,
}

/// What `chapters` of them are called.
fn noun(chapters: usize) -> &'static str {
    if chapters == 1 {
        "chapter"
    } else {
        "chapters"
    }
}

/// Plans `pieces` pieces of the conversion `opts` states over inputs that state `layouts`.
///
/// # Errors
///
/// [`PieceError::NothingLeft`] where the skips of `opts` leave nothing between them, and
/// [`PieceError::TooFewChapters`] where there are fewer chapters in what they leave than pieces
/// were asked for.
pub fn plan_pieces(
    layouts: &[Layout],
    opts: &Conversion,
    pieces: NonZeroUsize,
) -> Result<PiecePlan, PieceError> {
    let total = layouts
        .iter()
        .fold(0_u64, |sum, layout| sum.saturating_add(layout.frames));
    let start = opts.silence.skip_leading;
    let end = total.saturating_sub(opts.skip_trailing);
    if end <= start {
        return Err(PieceError::NothingLeft);
    }

    let places = places(&chapter_starts(layouts, &opts.chapter_mode), start, end);
    let pieces = pieces.get();
    if places.len() + 1 < pieces {
        return Err(PieceError::TooFewChapters {
            pieces,
            chapters: places.len() + 1,
        });
    }

    Ok(laid_out(
        &chosen(&places, start, end, pieces),
        places.len(),
        start,
        end,
    ))
}

/// Where every chapter of the conversion begins, by its place in the chapter plan.
fn chapter_starts(layouts: &[Layout], mode: &ChapterMode) -> Vec<u64> {
    let plan = match (mode, layouts) {
        (ChapterMode::Explicit(offsets), _) => stated(offsets),
        (ChapterMode::Auto, [only]) => authored(
            only.marks
                .iter()
                .map(|offset| Chapter {
                    offset: *offset,
                    title: None,
                })
                .collect(),
        ),
        // One chapter per input, beginning where the inputs in front of it add up to.
        (ChapterMode::Auto, several) => {
            return several
                .iter()
                .scan(0_u64, |behind, layout| {
                    let begins = *behind;
                    *behind = behind.saturating_add(layout.frames);

                    Some(begins)
                })
                .collect()
        }
    };

    plan.iter().map(|chapter| chapter.offset).collect()
}

/// The places a cut may fall: the chapter starts strictly inside what is split, each as the
/// chapter that begins there and where that is.
///
/// Chapters that begin in one place are one place, under the last of them: a chapter with no
/// length of its own has no audio to begin a piece with, and the one behind it does.
fn places(chapters: &[u64], start: u64, end: u64) -> Vec<(usize, u64)> {
    let mut places: Vec<(usize, u64)> = Vec::new();
    let inside = chapters
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, offset)| start < *offset && *offset < end);

    for (index, offset) in inside {
        match places.last_mut() {
            Some(last) if last.1 == offset => last.0 = index,
            _ => places.push((index, offset)),
        }
    }

    places
}

/// The cuts: for each, which place it is, the chapter that begins there, and where that is.
fn chosen(
    places: &[(usize, u64)],
    start: u64,
    end: u64,
    pieces: usize,
) -> Vec<(usize, usize, u64)> {
    let mut chosen = Vec::with_capacity(pieces - 1);
    let mut from = 0;

    for cut in 1..pieces {
        let even = even_point(start, end, cut, pieces);
        // The places behind the last cut that still leave one for every cut to come.
        let open = places.len() - (pieces - cut) + 1 - from;
        let nearest = places
            .iter()
            .enumerate()
            .skip(from)
            .take(open)
            .min_by_key(|(_, (_, offset))| offset.abs_diff(even));

        chosen.extend(nearest.map(|(at, (index, offset))| (at, *index, *offset)));
        from = nearest.map_or(from, |(at, _)| at + 1);
    }

    chosen
}

/// Where cut `cut` of a split into `pieces` aims: that many even shares into what is split.
fn even_point(start: u64, end: u64, cut: usize, pieces: usize) -> u64 {
    let into = u128::from(end - start) * cut as u128 / pieces as u128;

    start + u64::try_from(into).unwrap_or(u64::MAX)
}

/// The pieces the cuts make of what is split, where `places` is how many places there were to
/// cut at — one chapter each, behind the one the book opens with.
fn laid_out(cuts: &[(usize, usize, u64)], places: usize, start: u64, end: u64) -> PiecePlan {
    let mut pieces = Vec::with_capacity(cuts.len() + 1);
    // The piece being laid out: the chapter it begins at, where that is, and how many chapters
    // lie in front of it.
    let (mut first_chapter, mut begins, mut behind) = (0, start, 0);

    for (at, index, offset) in cuts.iter().copied() {
        pieces.push(PlannedPiece {
            first_chapter,
            chapters: at + 1 - behind,
            frames: offset - begins,
        });
        (first_chapter, begins, behind) = (index, offset, at + 1);
    }
    pieces.push(PlannedPiece {
        first_chapter,
        chapters: places + 1 - behind,
        frames: end - begins,
    });

    PiecePlan { pieces }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::num::NonZeroUsize;

    use super::{plan_pieces, Layout, PieceError, PlannedPiece};
    use crate::{ChapterMode, Conversion, SilenceOpts};

    /// One second, in the frames everything here is counted in.
    const SECOND: u64 = 48_000;

    /// One input playing `seconds`, with a mark at each of `marks` seconds.
    fn book(seconds: u64, marks: &[u64]) -> Vec<Layout> {
        vec![Layout {
            frames: seconds * SECOND,
            marks: marks.iter().map(|mark| mark * SECOND).collect(),
        }]
    }

    /// One input per entry of `seconds`, none of them carrying a mark.
    fn files(seconds: &[u64]) -> Vec<Layout> {
        seconds
            .iter()
            .map(|seconds| Layout {
                frames: seconds * SECOND,
                marks: Vec::new(),
            })
            .collect()
    }

    /// The plan for `pieces` pieces, as (first chapter, chapters, seconds) per piece.
    fn planned(layouts: &[Layout], opts: &Conversion, pieces: usize) -> Vec<(usize, usize, u64)> {
        plan_pieces(layouts, opts, NonZeroUsize::new(pieces).unwrap())
            .expect("a plan")
            .pieces
            .iter()
            .map(|piece| (piece.first_chapter, piece.chapters, piece.frames / SECOND))
            .collect()
    }

    /// Why there is no plan for `pieces` pieces.
    fn refused(layouts: &[Layout], opts: &Conversion, pieces: usize) -> PieceError {
        plan_pieces(layouts, opts, NonZeroUsize::new(pieces).unwrap()).expect_err("no plan")
    }

    #[test]
    fn a_book_is_cut_at_the_chapters_nearest_to_even() {
        // Sixteen chapters over 1:04:12. A third of that is 21:24 and two thirds are 42:48; the
        // chapter starts nearest to those are 22:50 and 42:37.
        let marks = [
            0, 195, 398, 626, 836, 1052, 1370, 1594, 1793, 2119, 2315, 2557, 2876, 3094, 3283, 3504,
        ];
        let layouts = book(3852, &marks);

        assert_eq!(
            planned(&layouts, &Conversion::default(), 3),
            [(0, 6, 1370), (6, 5, 1187), (11, 5, 1295)]
        );
        let plan = plan_pieces(
            &layouts,
            &Conversion::default(),
            NonZeroUsize::new(3).unwrap(),
        );
        assert_eq!(plan.unwrap().starts(), [6, 11]);
    }

    #[test]
    fn one_piece_is_the_whole_book() {
        let plan = plan_pieces(
            &book(100, &[0, 40, 60]),
            &Conversion::default(),
            NonZeroUsize::MIN,
        )
        .expect("a plan");

        assert_eq!(
            plan.pieces,
            [PlannedPiece {
                first_chapter: 0,
                chapters: 3,
                frames: 100 * SECOND
            }]
        );
        assert!(plan.starts().is_empty());
    }

    #[test]
    fn a_tie_goes_to_the_earlier_chapter() {
        // Half of 100 is 50, and 40 and 60 are as near to it as each other.
        assert_eq!(
            planned(&book(100, &[0, 40, 60]), &Conversion::default(), 2),
            [(0, 1, 40), (1, 2, 60)]
        );
    }

    #[test]
    fn a_cut_leaves_a_chapter_for_every_cut_behind_it() {
        // 20 is nearer to a third of 1000 than 10 is, but taking it would leave the second cut
        // nowhere to go.
        assert_eq!(
            planned(&book(1000, &[0, 10, 20]), &Conversion::default(), 3),
            [(0, 1, 10), (1, 1, 10), (2, 1, 980)]
        );
    }

    #[test]
    fn a_middle_cut_leaves_a_chapter_for_every_cut_behind_it_too() {
        // Pins the window of a cut that is neither the first nor the last: the places before the
        // cut in front of it are closed, and the places the cuts behind it need are kept. Cut 2
        // aims at 500, but with 40 held for the last cut only 30 is open to it.
        assert_eq!(
            planned(&book(1000, &[0, 10, 20, 30, 40]), &Conversion::default(), 4),
            [(0, 2, 20), (2, 1, 10), (3, 1, 10), (4, 1, 960)]
        );
    }

    #[test]
    fn a_cut_goes_behind_the_one_in_front_of_it() {
        // Both even points lie in the one long chapter, and the nearest start to both is 900.
        assert_eq!(
            planned(&book(1000, &[0, 900, 950]), &Conversion::default(), 3),
            [(0, 1, 900), (1, 1, 50), (2, 1, 50)]
        );
    }

    #[test]
    fn several_inputs_are_cut_where_one_of_them_ends() {
        assert_eq!(
            planned(&files(&[20, 10, 10]), &Conversion::default(), 2),
            [(0, 1, 20), (1, 2, 20)]
        );
    }

    #[test]
    fn chapters_in_one_place_are_one_place_to_cut_and_the_last_of_them_is_cut_at() {
        // The input in the middle states no length at all, so it begins where the one behind it
        // does — and it is the one behind it that has audio to begin a piece with.
        let layouts = files(&[10, 0, 10]);

        assert_eq!(
            planned(&layouts, &Conversion::default(), 2),
            [(0, 1, 10), (2, 1, 10)]
        );
        assert_eq!(
            refused(&layouts, &Conversion::default(), 3),
            PieceError::TooFewChapters {
                pieces: 3,
                chapters: 2
            }
        );
    }

    #[test]
    fn the_skips_move_what_is_split_and_take_the_chapters_inside_them_out() {
        let skipping = Conversion {
            silence: SilenceOpts {
                skip_leading: 20 * SECOND,
                ..SilenceOpts::default()
            },
            skip_trailing: 20 * SECOND,
            ..Conversion::default()
        };
        // What is split runs from 20 to 80. The chapters at 10 and 20 begin where the skip ends,
        // with the one the book opens with; the ones at 80 and 90 are left off.
        let layouts = book(100, &[0, 10, 20, 30, 40, 50, 60, 70, 80, 90]);

        assert_eq!(planned(&layouts, &skipping, 2), [(0, 3, 30), (5, 3, 30)]);
    }

    #[test]
    fn a_typed_chapter_list_is_the_plan_with_the_chapter_the_book_opens_with_in_front() {
        let stated = Conversion {
            chapter_mode: ChapterMode::Explicit(vec![30 * SECOND, 60 * SECOND]),
            ..Conversion::default()
        };

        // The list does not begin at the start, so the opening chapter is chapter 0 and what was
        // typed is chapters 1 and 2 — whatever the two inputs it runs across carry. Half of 90 is
        // as near to 30 as to 60, and the earlier of them is cut at.
        assert_eq!(
            planned(&files(&[45, 45]), &stated, 2),
            [(0, 1, 30), (1, 2, 60)]
        );
    }

    #[test]
    fn the_marks_of_one_input_are_put_in_order_before_they_are_counted() {
        // As a container states them: out of order, and one of them twice.
        assert_eq!(
            planned(&book(100, &[50, 0, 50]), &Conversion::default(), 2),
            [(0, 1, 50), (1, 1, 50)]
        );
    }

    #[test]
    fn fewer_chapters_than_pieces_is_refused_with_both_counts() {
        let one = refused(&book(100, &[]), &Conversion::default(), 3);
        assert_eq!(
            one,
            PieceError::TooFewChapters {
                pieces: 3,
                chapters: 1
            }
        );
        assert_eq!(
            one.to_string(),
            "3 pieces asked for, but the book has 1 chapter"
        );
        assert_eq!(
            refused(&book(100, &[0, 50]), &Conversion::default(), 3).to_string(),
            "3 pieces asked for, but the book has 2 chapters"
        );
    }

    #[test]
    fn nothing_left_between_the_skips_is_refused() {
        let skipping = |leading: u64, trailing: u64| Conversion {
            silence: SilenceOpts {
                skip_leading: leading * SECOND,
                ..SilenceOpts::default()
            },
            skip_trailing: trailing * SECOND,
            ..Conversion::default()
        };

        for (leading, trailing) in [(6, 4), (0, 11), (10, 0)] {
            let refusal = refused(&book(10, &[0, 5]), &skipping(leading, trailing), 2);
            assert_eq!(refusal, PieceError::NothingLeft, "{leading} and {trailing}");
        }
        assert_eq!(
            PieceError::NothingLeft.to_string(),
            "nothing is left to split once the leading and trailing skips are taken off"
        );
        // And no inputs at all are nothing to split either.
        assert_eq!(
            refused(&[], &Conversion::default(), 2),
            PieceError::NothingLeft
        );
    }
}
