//! Thin GUI frontend around the `taffle` library: the Qt boot, the smoke self-test, and
//! nothing else — everything the chrome does lives behind `bridge`.

mod bridge;

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Set when the QML root component fails to load. Qt reports that through a signal and then
/// carries on, leaving an engine with no window behind — the event loop would run forever with
/// nothing to show — so the boot ends on this flag instead. It is a static because the handler
/// that sets it may borrow nothing local: cxx-qt requires those closures to be `'static`.
static ROOT_LOAD_FAILED: AtomicBool = AtomicBool::new(false);

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
    // Kept alive across the load below: dropping the guard disconnects the handler.
    let _load_failure = engine.as_mut().map(|engine| {
        engine.on_object_creation_failed(|_, _| ROOT_LOAD_FAILED.store(true, Ordering::Relaxed))
    });
    if let Some(engine) = engine.as_mut() {
        engine.load(&QUrl::from("qrc:/qt/qml/Taffle/qml/Main.qml"));
    }
    // A component compiled into the binary's own resources loads synchronously, so the failure
    // has already been signalled by the time the load returns.
    if ROOT_LOAD_FAILED.load(Ordering::Relaxed) {
        return ExitCode::FAILURE;
    }
    let code = app.as_mut().map_or(1, |app| app.exec());
    u8::try_from(code).map_or(ExitCode::FAILURE, ExitCode::from)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::process::Stdio;
    use std::time::{Duration, Instant};

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
    /// the test; a run that never finishes fails it too, at the deadline below.
    ///
    /// Lives in the binary's unit tests because the cxx-qt C++ archive references bridge
    /// symbols that only the binary target carries — a `tests/` integration target cannot
    /// link against it.
    #[test]
    fn smoke_boots_the_chrome_headless() {
        /// The whole budget one smoke run gets before it is killed and reported as stuck. The
        /// boot it waits on takes 0.05 s offscreen on the machine this was written on, so the
        /// rest is headroom for a cold, loaded CI box rather than time the work needs. What
        /// the value has to clear is any honest boot; what it has to stay far below is the CI
        /// job timeout, so a run that will never finish is reported here instead of being
        /// swallowed when the job is killed.
        const DEADLINE: Duration = Duration::from_secs(30);
        /// How often the run is checked for having finished: short enough to add nothing
        /// measurable to a boot counted in tens of milliseconds, long enough to leave the CPU
        /// alone while waiting.
        const POLL: Duration = Duration::from_millis(10);

        // target/<profile>/deps/taffle_gui-<hash> → target/<profile>/taffle-gui
        let mut binary = std::env::current_exe().expect("test binary path");
        binary.pop();
        if binary.ends_with("deps") {
            binary.pop();
        }
        // The build below has to land in the very directory this test then launches from, so
        // the profile is read back off that directory rather than assumed: cargo builds its
        // `dev` profile into `debug`, and every other profile into a directory of its own name.
        let dir = binary
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .expect("the profile directory the test binary sits in");
        let profile = if dir == "debug" { "dev" } else { dir };

        // `cargo test` builds this test's own harness binary, which is a separate artifact
        // from the plain bin launched below — so a green run does not by itself prove that bin
        // was rebuilt. Driving a real `cargo build` uses cargo's dependency graph instead of an
        // mtime heuristic, so an edit to the QML, this crate, or a dependency cannot leave a
        // stale binary in place and keep this test passing.
        let build = std::process::Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "taffle-gui",
                "--bin",
                "taffle-gui",
                "--profile",
                profile,
            ])
            .output()
            .expect("running cargo build -p taffle-gui");
        assert!(
            build.status.success(),
            "build failed:\n{}",
            String::from_utf8_lossy(&build.stderr)
        );

        binary.push("taffle-gui");
        assert!(
            binary.is_file(),
            "expected the taffle-gui binary at {}",
            binary.display()
        );

        let mut run = std::process::Command::new(&binary)
            .arg("--smoke")
            // No display server needed, and a software scene graph, so the run is the same
            // on a bare CI machine as on a desktop.
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("QT_QUICK_BACKEND", "software")
            // Qt sends its messages to the systemd journal instead of stderr whenever it is
            // built against libsystemd and started from a journal-backed session, which would
            // leave the scan below reading an empty stream.
            .env("QT_FORCE_STDERR_LOGGING", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("running taffle-gui --smoke");

        // Waiting on the run without a deadline turns every way of not finishing — a root
        // component that fails to load, a quit that never comes — into a hang that reports
        // nothing at all until CI kills the whole job.
        let deadline = Instant::now() + DEADLINE;
        let finished = loop {
            match run.try_wait().expect("polling the smoke run") {
                Some(status) => break Some(status),
                None if Instant::now() >= deadline => {
                    run.kill()
                        .expect("killing a smoke run that would not finish");
                    break None;
                }
                None => std::thread::sleep(POLL),
            }
        };
        let output = run.wait_with_output().expect("the smoke run's output");
        let stderr = String::from_utf8_lossy(&output.stderr);

        let Some(status) = finished else {
            panic!("the smoke run had not finished after {DEADLINE:?} and was killed:\n{stderr}");
        };
        assert!(status.success(), "smoke exited {status:?}:\n{stderr}");
        // A QML diagnostic always carries "<file>.qml:<line>" — unlike the loaded URL, Qt's own
        // logging categories, and anything console.log prints, none of which mean a failure.
        let diagnostic = stderr.lines().find(|line| line.contains(".qml:"));
        assert_eq!(
            diagnostic, None,
            "QML diagnostics in the smoke run:\n{stderr}"
        );
    }
}
