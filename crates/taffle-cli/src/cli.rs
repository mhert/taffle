//! The command line as it is typed: what `taffle` takes, and what every part of it means.
//!
//! A bare command line converts, and the one subcommand there is reads files back — so the
//! arguments of a conversion sit next to the subcommand rather than under one of their own, and
//! stating both is refused rather than half-obeyed.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use taffle::duration::Seconds;

/// Converts audiobooks into the Tonie Audio Format.
#[derive(Debug, Parser)]
#[command(
    name = "taffle",
    version,
    about,
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
pub struct Cli {
    /// What is done instead of converting, where anything is.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// The conversion a bare command line asks for.
    #[command(flatten)]
    pub convert: ConvertArgs,
}

/// What `taffle` does with files that are already TAFs.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Inspect and validate TAF files
    ///
    /// Every file is read through the way a box reads one — the header block, then the audio
    /// region one block at a time, hashed on the way — and what it holds is printed once it has
    /// been found to hold it: the audio id, how long it plays, the bytes its audio occupies, and
    /// the block every chapter starts at, which is what a box seeks on. A file that is not the one
    /// its header describes is said on stderr instead and the code is 1; the files behind it are
    /// read all the same.
    Info {
        /// The files to read.
        #[arg(value_name = "FILE.taf", required = true)]
        files: Vec<PathBuf>,
    },
}

/// A conversion, as it was asked for.
#[derive(Debug, Args)]
pub struct ConvertArgs {
    /// The audio files to convert, in the order they play
    ///
    /// Several of them are one book: they are concatenated in the order they are named, and each
    /// of them begins a chapter. A single input keeps the chapter marks it carries.
    #[arg(value_name = "INPUT", required = true)]
    pub inputs: Vec<PathBuf>,

    /// Output .taf. Default: first input's name + .taf
    ///
    /// Whatever is already at that path is emptied and written over — the conversion writes into
    /// the file it made room for, whether that name held a TAF, a book or anything else.
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Drop N seconds from the very start (e.g. 4.4)
    ///
    /// Taken off in front of everything else, so a trim of chapter 1 begins where this ended.
    #[arg(long, value_name = "SECONDS", default_value = "0")]
    pub skip_leading: Seconds,

    /// Trim leading silence at the start of chapter 1 (applied after --skip-leading)
    #[arg(long)]
    pub trim_pause_leading: bool,

    /// Trim leading silence at the start of every chapter (implies chapter 1 too)
    #[arg(long)]
    pub trim_pause_each_chapter: bool,

    /// Insert silence at the start of chapter 1 (after any trimming)
    ///
    /// Stacks with --add-pause-each-chapter: chapter 1 is given both, one behind the other, since
    /// each of them states what it puts in and neither takes the other's place.
    #[arg(long, value_name = "SECONDS", default_value = "0")]
    pub add_pause_leading: Seconds,

    /// Insert silence at the start of every chapter
    ///
    /// Put in after that chapter's own trimming. Chapter 1 is given this as well as
    /// --add-pause-leading, so at the start of the book the two are added together.
    #[arg(long, value_name = "SECONDS", default_value = "0")]
    pub add_pause_each_chapter: Seconds,

    /// Override chapter marks ("0:00,12:34,1:02:10.5")
    ///
    /// Times from the start of the converted audio, in the formats SS(.ms), MM:SS(.ms) and
    /// HH:MM:SS(.ms). Overrides everything an input carries, and every entry has to lie behind the
    /// one in front of it and inside the audio.
    #[arg(long, value_name = "LIST", value_delimiter = ',')]
    pub chapters: Option<Vec<Seconds>>,

    /// Don't extract embedded cover art
    ///
    /// The cover of the first input that carries any goes beside the TAF under the output's own
    /// name, with .jpg or .png in the place of .taf — overwriting the file already at that name,
    /// if there is one.
    #[arg(long)]
    pub no_cover: bool,
}
