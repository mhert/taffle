//! The two ways a set of conversions would write over audio somebody wanted kept: a job writing
//! into a file it reads, and two jobs writing into one file.
//!
//! An output is created by emptying whatever is at its name, so a job whose output is one of its
//! inputs reads the audio out of the file it has just emptied, and two jobs that write one name
//! leave whichever finished last with the other one's hour of encoding gone. Both are settled with
//! every job in hand and nothing on the disk to undo yet, which is the only moment either of them
//! is still a refusal rather than a loss.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::{default_output_path, ConvertJob};

/// Why a set of jobs cannot all run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CollisionError {
    /// A job would write into a file it reads.
    #[error("the output {} is one of the inputs: converting it would write over the audio being read", output.display())]
    OutputIsInput {
        /// The file that is both read and written, as the caller named it.
        output: PathBuf,
    },
    /// Two jobs would write the same file.
    #[error("the output {} is stated by more than one conversion: they would write over each other", output.display())]
    DuplicateOutput {
        /// The file more than one job would write, as it was named or as it was derived.
        output: PathBuf,
    },
}

/// Refuses jobs that would write over what they read, or over each other.
///
/// Outputs are resolved the way the jobs will resolve them: as stated, or as
/// [`default_output_path`] of the first input. A job of no inputs resolves to no output here —
/// having nothing to convert is the engine's refusal, not a collision. Paths are compared as
/// they were typed, exactly as the single-job check always did: two names for one file are two
/// names here, and the conversion runs.
///
/// # Errors
///
/// [`CollisionError::OutputIsInput`] where any job's output is any job's input, and
/// [`CollisionError::DuplicateOutput`] where two jobs resolve to one output.
pub fn refuse_collisions(jobs: &[ConvertJob]) -> Result<(), CollisionError> {
    let outputs: Vec<PathBuf> = jobs
        .iter()
        .filter_map(|job| {
            job.output
                .clone()
                .or_else(|| job.inputs.first().map(|first| default_output_path(first)))
        })
        .collect();

    let inputs: HashSet<&PathBuf> = jobs.iter().flat_map(|job| &job.inputs).collect();
    if let Some(clash) = outputs.iter().find(|output| inputs.contains(output)) {
        return Err(CollisionError::OutputIsInput {
            output: clash.clone(),
        });
    }

    let mut seen: HashSet<&PathBuf> = HashSet::new();
    if let Some(duplicate) = outputs.iter().find(|output| !seen.insert(output)) {
        return Err(CollisionError::DuplicateOutput {
            output: duplicate.clone(),
        });
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::path::PathBuf;

    use super::refuse_collisions;
    use crate::{Conversion, ConvertJob};

    /// A job reading `inputs` and writing where `output` says, or where its first input leads.
    fn job(inputs: &[&str], output: Option<&str>) -> ConvertJob {
        ConvertJob {
            inputs: inputs.iter().map(PathBuf::from).collect(),
            output: output.map(PathBuf::from),
            options: Conversion::default(),
            write_cover: false,
        }
    }

    #[test]
    fn an_output_that_is_an_input_is_refused() {
        let job = job(&["a.mp3", "b.mp3"], Some("a.mp3"));
        let error = refuse_collisions(std::slice::from_ref(&job)).expect_err("a collision");
        assert_eq!(
            error.to_string(),
            "the output a.mp3 is one of the inputs: converting it would write over the audio being read"
        );
    }

    #[test]
    fn two_jobs_writing_one_output_are_refused() {
        // The second job states no output, so its collision is with the first job's *derived* name.
        let jobs = [
            job(&["x/book.m4b"], Some("x/book.taf")),
            job(&["x/book.m4b"], None),
        ];
        let error = refuse_collisions(&jobs).expect_err("a collision");
        assert_eq!(
            error.to_string(),
            "the output x/book.taf is stated by more than one conversion: they would write over each other"
        );
    }

    #[test]
    fn distinct_jobs_pass() {
        let jobs = [job(&["a.mp3"], None), job(&["b.mp3"], None)];
        assert!(refuse_collisions(&jobs).is_ok());
    }
}
