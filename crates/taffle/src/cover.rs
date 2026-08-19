//! The cover art beside the file: where it goes, and why it sometimes does not.

use std::fs;
use std::path::{Path, PathBuf};

use taf_encode::Cover;

/// Writes `cover` beside the TAF at `taf`, under that file's own name and the extension the
/// picture's type calls for, and states where it went.
///
/// A cover travels out of an input in the encoding it was stored in, so what is written here is
/// those bytes and nothing else — the name is the only thing that says what they are. Which is why
/// only the two types a name can be given for are written at all: a JPEG becomes `.jpg` and a PNG
/// becomes `.png`, and anything else would be a file whose name lies about its contents.
///
/// # Errors
///
/// Why no cover was written, in words a frontend can show: the type of a picture nothing here
/// writes, or the file that could not be written and what stopped it. Neither is a failure of the
/// conversion — see [`crate::run_convert`].
pub(crate) fn write_beside(taf: &Path, cover: &Cover) -> Result<PathBuf, String> {
    let Some(extension) = extension(&cover.mime) else {
        return Err(format!(
            "cover art of type '{}' was left out: a cover is written as image/jpeg or image/png \
             and as nothing else",
            cover.mime
        ));
    };

    let path = taf.with_extension(extension);
    fs::write(&path, &cover.bytes)
        .map_err(|error| format!("cannot write cover {}: {error}", path.display()))?;

    Ok(path)
}

/// What a picture of `mime` is called on disk, where anything here calls it something.
fn extension(mime: &str) -> Option<&'static str> {
    match mime {
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        _ => None,
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::fs;

    use taf_encode::Cover;
    use tempfile::TempDir;

    use super::write_beside;

    /// A cover of `mime`, with bytes nothing here reads.
    fn cover(mime: &str) -> Cover {
        Cover {
            bytes: vec![1, 2, 3],
            mime: mime.to_owned(),
        }
    }

    #[test]
    fn a_jpeg_cover_is_written_beside_the_file_under_the_file_s_own_name() {
        let dir = TempDir::new().expect("a directory of its own");
        let taf = dir.path().join("Book Name.taf");

        let written = write_beside(&taf, &cover("image/jpeg")).expect("the cover is written");

        assert_eq!(written, dir.path().join("Book Name.jpg"));
        assert_eq!(fs::read(&written).expect("the file is there"), [1, 2, 3]);
    }

    #[test]
    fn a_png_cover_is_written_as_a_png() {
        let dir = TempDir::new().expect("a directory of its own");
        let taf = dir.path().join("Book Name.taf");

        let written = write_beside(&taf, &cover("image/png")).expect("the cover is written");

        assert_eq!(written, dir.path().join("Book Name.png"));
    }

    #[test]
    fn a_cover_of_any_other_type_is_left_out_and_says_which_type_that_was() {
        let dir = TempDir::new().expect("a directory of its own");
        let taf = dir.path().join("Book Name.taf");

        let why = write_beside(&taf, &cover("image/webp")).expect_err("nothing writes a webp here");

        assert!(why.contains("image/webp"), "{why}");
        assert_eq!(
            fs::read_dir(dir.path())
                .expect("the directory reads")
                .count(),
            0,
            "a cover nothing writes left a file behind"
        );
    }

    #[test]
    fn a_cover_that_cannot_be_written_says_where_it_was_going_and_what_stopped_it() {
        let dir = TempDir::new().expect("a directory of its own");
        // A directory where the cover's file should be is a name that cannot be written to, on
        // every system this runs on.
        let taf = dir.path().join("Book Name.taf");
        fs::create_dir(dir.path().join("Book Name.png")).expect("something else is in the way");

        let why = write_beside(&taf, &cover("image/png")).expect_err("a directory takes no bytes");

        assert!(why.contains("Book Name.png"), "{why}");
    }
}
