//! Thin GUI frontend around the `taffle` library: the Qt boot, the smoke self-test, and
//! nothing else — everything the chrome does lives behind `bridge`.

mod bridge;

use std::process::ExitCode;
use std::sync::OnceLock;

use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QUrl};

const HELP: &str = "\
taffle-gui (Qt frontend)

USAGE: taffle-gui [--smoke]

  --smoke   boot the whole chrome offscreen-capably and exit (self-test)
";

/// Parsed command-line arguments.
#[derive(Debug, Default, PartialEq, Eq)]
struct Cli {
    smoke: bool,
    help: bool,
}

impl Cli {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut parsed = Self::default();
        for arg in args {
            match arg.as_str() {
                "--smoke" => parsed.smoke = true,
                "--help" | "-h" => parsed.help = true,
                other => return Err(format!("unknown argument {other} (try --help)")),
            }
        }
        Ok(parsed)
    }
}

/// The parsed CLI, for the bridge's construction: QML instantiates the app object, so a
/// constructor argument cannot carry these.
static CLI: OnceLock<Cli> = OnceLock::new();

/// The process's parsed CLI arguments.
pub(crate) fn cli() -> &'static Cli {
    CLI.get_or_init(Cli::default)
}

fn main() -> ExitCode {
    let cli = match Cli::parse(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(message) => {
            eprintln!("error: {message}");
            return ExitCode::from(2);
        }
    };
    if cli.help {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    let _ = CLI.set(cli);

    let mut app = QGuiApplication::new();
    let mut engine = QQmlApplicationEngine::new();
    if let Some(engine) = engine.as_mut() {
        engine.load(&QUrl::from("qrc:/qt/qml/Taffle/qml/Main.qml"));
    }
    let code = app.as_mut().map_or(1, |app| app.exec());
    u8::try_from(code).map_or(ExitCode::FAILURE, ExitCode::from)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn args(list: &[&str]) -> Result<Cli, String> {
        Cli::parse(list.iter().map(ToString::to_string))
    }

    #[test]
    fn the_only_arguments_a_run_takes_are_smoke_and_help() {
        assert_eq!(args(&[]).unwrap(), Cli::default());
        assert!(args(&["--smoke"]).unwrap().smoke);
        assert!(args(&["--help"]).unwrap().help);
        assert!(args(&["-h"]).unwrap().help);
        assert!(args(&["--frobnicate"]).is_err(), "unknown flag");
    }

    /// End-to-end QML smoke test: boots the real binary under Qt's `offscreen` platform in
    /// `--smoke` mode, which instantiates the whole chrome, then exits. Any QML error (bad
    /// property, missing type, broken binding) surfaces as a diagnostic on stderr and fails
    /// the test.
    ///
    /// Lives in the binary's unit tests because the cxx-qt C++ archive references bridge
    /// symbols that only the binary target carries — a `tests/` integration target cannot
    /// link against it.
    #[test]
    fn smoke_boots_the_chrome_headless() {
        // `cargo test` builds this test's own harness binary, which is a separate artifact
        // from `target/debug/taffle-gui`, the plain bin launched below — so a green run does
        // not by itself prove that bin was rebuilt. Driving a real `cargo build` uses cargo's
        // dependency graph instead of an mtime heuristic, so an edit to the QML, this crate,
        // or a dependency cannot leave a stale binary in place and keep this test passing.
        let build = std::process::Command::new(env!("CARGO"))
            .args(["build", "-p", "taffle-gui", "--bin", "taffle-gui"])
            .output()
            .expect("running cargo build -p taffle-gui");
        assert!(
            build.status.success(),
            "build failed:\n{}",
            String::from_utf8_lossy(&build.stderr)
        );

        // target/debug/deps/taffle_gui-<hash> → target/debug/taffle-gui
        let mut binary = std::env::current_exe().expect("test binary path");
        binary.pop();
        if binary.ends_with("deps") {
            binary.pop();
        }
        binary.push("taffle-gui");

        let output = std::process::Command::new(&binary)
            .arg("--smoke")
            // No display server needed, and a software scene graph, so the run is the same
            // on a bare CI machine as on a desktop.
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("QT_QUICK_BACKEND", "software")
            // Qt sends its messages to the systemd journal instead of stderr whenever it is
            // built against libsystemd and started from a journal-backed session, which would
            // leave the scan below reading an empty stream.
            .env("QT_FORCE_STDERR_LOGGING", "1")
            .output()
            .expect("running taffle-gui --smoke");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "smoke exited {:?}:\n{stderr}",
            output.status
        );
        assert!(
            !stderr.to_lowercase().contains("qml"),
            "QML diagnostics in the smoke run:\n{stderr}"
        );
    }
}
