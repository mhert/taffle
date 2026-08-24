//! Thin CLI frontend around the `taffle` library: argument parsing, progress rendering,
//! and error reporting.
//!
//! # What a run comes to
//!
//! A file that was written is said on stdout and the code is 0. A failure is said on stderr as the
//! whole chain of what went wrong, every layer of it on one line, and the code is 1. A command line
//! that is no command line is clap's own refusal, which is 2.

mod cli;
mod convert_cmd;
mod info_cmd;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::{Cli, Command};

fn main() -> ExitCode {
    let cli = Cli::parse();

    let outcome = match cli.command {
        Some(Command::Info { files }) => info_cmd::run(&files),
        None => convert_cmd::run(cli.convert),
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");

            ExitCode::FAILURE
        }
    }
}
