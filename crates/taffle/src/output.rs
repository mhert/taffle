//! Where the file a conversion writes goes when the caller did not say.

use std::path::{Path, PathBuf};

use crate::ConvertJob;

/// What a converted file is called: the input's own name with `.taf` in the place of its
/// extension, in the input's own directory.
///
/// A name with no extension keeps all of itself and has the format added to it — `Book` becomes
/// `Book.taf` — and a name of several dots keeps every one of them but the last, so
/// `Book.Teil 2.m4b` becomes `Book.Teil 2.taf`. Which is the same rule either way: what a file is
/// called stays, and what it is stated to be is now a TAF.
///
/// Nothing is checked against the disk here. Whether that name is free, or in a directory that
/// exists at all, is a question for whoever creates the file.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use taffle::default_output_path;
///
/// assert_eq!(
///     default_output_path(Path::new("/books/Grimm und Möhrchen.m4b")),
///     Path::new("/books/Grimm und Möhrchen.taf")
/// );
/// ```
#[must_use]
pub fn default_output_path(first_input: &Path) -> PathBuf {
    first_input.with_extension("taf")
}

/// What piece `index` of `count` pieces of the conversion written to `output` is called: the
/// output's own name with the number of the piece behind a hyphen, in front of the extension.
///
/// Pieces are numbered from 1, in as many digits as the count has — `book-1.taf` of three,
/// `book-01.taf` of twelve — so a directory lists them in the order they play.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use taffle::piece_path;
///
/// assert_eq!(
///     piece_path(Path::new("/books/Grimm.taf"), 1, 3),
///     Path::new("/books/Grimm-2.taf")
/// );
/// ```
#[must_use]
pub fn piece_path(output: &Path, index: usize, count: usize) -> PathBuf {
    let width = count.to_string().len();
    let mut name = output.file_stem().unwrap_or_default().to_os_string();
    name.push(format!("-{:0width$}", index + 1));
    if let Some(extension) = output.extension() {
        name.push(".");
        name.push(extension);
    }

    output.with_file_name(name)
}

/// Every file `job` writes, in the order it writes them: its output, or one file per piece under
/// [`piece_path`] of that output — as stated, or as [`default_output_path`] of the first input.
///
/// A job of no inputs that states no output writes nothing, and is nothing to convert.
#[must_use]
pub fn output_paths(job: &ConvertJob) -> Vec<PathBuf> {
    let output = job
        .output
        .clone()
        .or_else(|| job.inputs.first().map(|first| default_output_path(first)));
    let count = job.piece_starts.len() + 1;

    match output {
        Some(output) if count > 1 => (0..count)
            .map(|index| piece_path(&output, index, count))
            .collect(),
        output => output.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{output_paths, piece_path};
    use crate::{Conversion, ConvertJob};

    /// A job reading `input`, writing where `output` says, cut at `piece_starts`.
    fn job(input: &str, output: Option<&str>, piece_starts: &[usize]) -> ConvertJob {
        ConvertJob {
            inputs: vec![PathBuf::from(input)],
            output: output.map(PathBuf::from),
            options: Conversion::default(),
            write_cover: false,
            piece_starts: piece_starts.to_vec(),
        }
    }

    #[test]
    fn piece_names_keep_the_name_and_gain_the_number() {
        let named = |output: &str, index, count| piece_path(Path::new(output), index, count);

        assert_eq!(named("x/book.taf", 0, 3), Path::new("x/book-1.taf"));
        assert_eq!(named("x/book.taf", 2, 3), Path::new("x/book-3.taf"));
        // From ten pieces up the number is as wide as the count, so the names sort as they play.
        assert_eq!(named("x/book.taf", 0, 10), Path::new("x/book-01.taf"));
        assert_eq!(named("x/book.taf", 9, 10), Path::new("x/book-10.taf"));
        assert_eq!(named("book.taf", 99, 100), Path::new("book-100.taf"));
        // A name of several dots keeps every one of them, and one with no extension gains none.
        assert_eq!(
            named("Book.Teil 2.taf", 0, 2),
            Path::new("Book.Teil 2-1.taf")
        );
        assert_eq!(named("x/Book", 1, 2), Path::new("x/Book-2"));
    }

    #[test]
    fn a_job_writes_its_output_or_one_file_per_piece() {
        assert_eq!(
            output_paths(&job("a.m4b", Some("out/b.taf"), &[])),
            [PathBuf::from("out/b.taf")]
        );
        assert_eq!(
            output_paths(&job("a.m4b", Some("out/b.taf"), &[3, 7])),
            ["out/b-1.taf", "out/b-2.taf", "out/b-3.taf"].map(PathBuf::from)
        );
        // An output nobody stated is the name derived from the first input, pieces and all.
        assert_eq!(
            output_paths(&job("x/a.m4b", None, &[1])),
            ["x/a-1.taf", "x/a-2.taf"].map(PathBuf::from)
        );
        // And a job with nothing to convert has nothing to write.
        let nothing = ConvertJob {
            inputs: Vec::new(),
            ..job("a.m4b", None, &[1])
        };
        assert!(output_paths(&nothing).is_empty());
    }
}
