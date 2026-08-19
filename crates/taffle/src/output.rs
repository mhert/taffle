//! Where the file a conversion writes goes when the caller did not say.

use std::path::{Path, PathBuf};

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
