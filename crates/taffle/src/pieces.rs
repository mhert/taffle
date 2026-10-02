//! The pieces of a job, planned off the files it names: what each of them states about itself,
//! and the plan the engine makes of that.
//!
//! The engine plans from layouts and knows no path; a frontend has paths and wants to say which
//! of them could not be read. That is the whole of what is here.

use std::fs::File;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use taf_encode::{Layout, PieceError, PiecePlan, ProbeError};

use crate::duration::RATE;
use crate::{probe_duration, ConvertJob};

/// Why the pieces of a job could not be planned.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PlanError {
    /// One of the inputs states nothing to plan from: it could not be read, is no format this
    /// build reads, or states no length.
    #[error("no length could be read off {}, so the pieces cannot be planned", path.display())]
    NoLength {
        /// The input, as the job names it.
        path: PathBuf,
    },
    /// The book cannot be cut into the pieces asked for.
    #[error(transparent)]
    Pieces(#[from] PieceError),
}

/// What the file at `path` states about itself: how long it plays and where its chapter marks
/// begin, read off its headers without decoding any of it.
///
/// # Errors
///
/// What [`probe_duration`] makes of a file that cannot be opened or states no length, and
/// [`ProbeError::Unrecognized`] where its marks cannot be read.
pub fn probe_layout(path: &Path) -> Result<Layout, ProbeError> {
    let frames = frames_48k(probe_duration(path)?);
    let marks = taf_encode::probe_marks(Box::new(File::open(path)?))?;

    Ok(Layout { frames, marks })
}

/// Plans `pieces` pieces of `job`, where `layouts` is what each of its inputs states, in the
/// order the job names them — [`None`] for one that states nothing.
///
/// # Errors
///
/// [`PlanError::NoLength`] naming the first input there is no layout for, and
/// [`PlanError::Pieces`] where the engine refuses the plan.
pub fn plan_pieces(
    job: &ConvertJob,
    layouts: &[Option<Layout>],
    pieces: NonZeroUsize,
) -> Result<PiecePlan, PlanError> {
    let stated: Vec<Layout> = job
        .inputs
        .iter()
        .enumerate()
        .map(|(at, path)| {
            layouts
                .get(at)
                .cloned()
                .flatten()
                .ok_or_else(|| PlanError::NoLength { path: path.clone() })
        })
        .collect::<Result<_, _>>()?;

    Ok(taf_encode::plan_pieces(&stated, &job.options, pieces)?)
}

/// How many frames at 48 kHz `length` comes to, with what is short of a frame dropped.
fn frames_48k(length: Duration) -> u64 {
    let rate = u64::from(RATE);
    let whole = length.as_secs().saturating_mul(rate);

    whole.saturating_add(u64::from(length.subsec_nanos()) * rate / 1_000_000_000)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::num::NonZeroUsize;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{frames_48k, plan_pieces, probe_layout, PlanError};
    use crate::{Conversion, ConvertJob, Layout, ProbeError};

    /// The fixture `name`, where `taf-encode` keeps the committed ones.
    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../taf-encode/tests/fixtures")
            .join(name)
    }

    /// A job over `inputs` with nothing else asked of it.
    fn job(inputs: &[&str]) -> ConvertJob {
        ConvertJob {
            inputs: inputs.iter().map(PathBuf::from).collect(),
            output: None,
            options: Conversion::default(),
            write_cover: false,
            piece_starts: Vec::new(),
        }
    }

    /// Two pieces.
    fn two() -> NonZeroUsize {
        NonZeroUsize::MIN.saturating_add(1)
    }

    #[test]
    fn a_length_is_counted_in_frames_to_the_frame() {
        assert_eq!(frames_48k(Duration::ZERO), 0);
        assert_eq!(frames_48k(Duration::from_secs(2)), 96_000);
        assert_eq!(frames_48k(Duration::from_millis(1_500)), 72_000);
        // A frame is 20 833 and a third nanoseconds, and what is short of one is none.
        assert_eq!(frames_48k(Duration::from_nanos(20_833)), 0);
        assert_eq!(frames_48k(Duration::from_nanos(20_834)), 1);
    }

    #[test]
    fn a_book_states_its_length_and_its_marks_without_being_converted() {
        let layout = probe_layout(&fixture("tiny.m4b")).expect("the book states both");

        // 10 s and 23 219 954 ns as the container counts it, which is 481 114 frames; and the
        // two marks it was authored with, five seconds apart.
        assert_eq!(
            layout,
            Layout {
                frames: 481_114,
                marks: vec![0, 240_000]
            }
        );
    }

    #[test]
    fn a_path_that_is_not_there_states_no_layout() {
        let refusal = probe_layout(Path::new("/nowhere/at/all.m4b")).expect_err("no file");

        assert!(matches!(refusal, ProbeError::Io(_)), "{refusal:?}");
    }

    #[test]
    fn a_job_is_planned_from_what_its_inputs_state() {
        let layouts = [Some(Layout {
            frames: 480_000,
            marks: vec![0, 240_000],
        })];
        let plan = plan_pieces(&job(&["a.m4b"]), &layouts, two()).expect("a plan");

        assert_eq!(plan.starts(), [1]);
    }

    #[test]
    fn an_input_no_length_could_be_read_off_is_named() {
        let layouts = [
            Some(Layout {
                frames: 480_000,
                marks: Vec::new(),
            }),
            None,
        ];

        for layouts in [&layouts[..], &layouts[..1]] {
            let refusal =
                plan_pieces(&job(&["x/01.mp3", "x/02.mp3"]), layouts, two()).expect_err("no plan");
            assert!(
                matches!(&refusal, PlanError::NoLength { path } if path == Path::new("x/02.mp3"))
            );
            assert_eq!(
                refusal.to_string(),
                "no length could be read off x/02.mp3, so the pieces cannot be planned"
            );
        }
    }

    #[test]
    fn what_the_engine_refuses_is_said_as_the_engine_says_it() {
        let layouts = [Some(Layout {
            frames: 480_000,
            marks: Vec::new(),
        })];
        let refusal = plan_pieces(&job(&["a.mp3"]), &layouts, two()).expect_err("no plan");

        assert_eq!(
            refusal.to_string(),
            "2 pieces asked for, but the book has 1 chapter"
        );
    }
}
