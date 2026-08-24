//! The one bridge between QML and Rust: every `QObject` the chrome talks to.
//!
//! The chrome is one object. What a person is typing lives in [`plan::Panel`], what they have
//! queued lives in [`Book`]s, and everything QML shows is read back off those through invokables
//! that take a row number — rather than through a `QAbstractListModel`, because the queue is
//! dozens of rows at most and a bumped `revision` is what makes the delegates re-read.
//!
//! Nothing here converts, parses or schedules anything: reading what was typed is [`plan`]'s, and
//! running a batch is [`worker`]'s. This file is where those two meet a window.

// The bridge macro generates FFI declarations the compiler calls unsafe; the exceptions are
// scoped to the bridge module and each carries its SAFETY reasoning where it stands.
#![allow(unsafe_code)]
// C++ owns every object declared below, so the macro hands each one over boxed. Clippy reads
// that as a needless box while the state behind it is still smaller than a pointerful of
// fields, and it is not ours to change. Both attributes have to be file-scoped: cxx_qt::bridge
// rejects any outer attribute on its module.
#![allow(clippy::unnecessary_box_returns)]

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::QString;
use taffle::duration::{clock, RATE};

use crate::{plan, worker};

/// How far the bar of a running conversion goes: not quite full, because what a conversion writes
/// is not what its inputs stated it would be — every pause it adds is audio no probe ever saw — so
/// the last hundredth is filled in by the conversion saying it is done rather than by arithmetic.
const NEARLY_DONE: f64 = 0.99;

/// The fraction a row shows where there is no length to count against. Negative on purpose: a
/// percent cannot be negative, so it is the one answer that cannot be mistaken for one, and the
/// row shows a stripe instead.
const NO_LENGTH: f64 = -1.0;

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        /// From cxx-qt-lib.
        type QString = cxx_qt_lib::QString;
    }

    unsafe extern "RustQt" {
        #[qobject]
        #[qml_element]
        #[qproperty(i32, revision)]
        #[qproperty(bool, converting)]
        #[qproperty(i32, file_count, cxx_name = "fileCount")]
        #[qproperty(i32, book_count, cxx_name = "bookCount")]
        #[qproperty(QString, panel_error, cxx_name = "panelError")]
        #[qproperty(QString, chapter_warning, cxx_name = "chapterWarning")]
        #[qproperty(bool, smoke_mode, cxx_name = "smokeMode")]
        /// The QML-facing application object.
        type TaffleApp = super::TaffleAppRust;

        /// Adds every file `paths` names — one filesystem path per line, which is what a file
        /// dialog's selection and a drop come to — to the book being edited, in that order.
        #[qinvokable]
        #[cxx_name = "addFiles"]
        fn add_files(self: Pin<&mut Self>, paths: &QString);

        /// Takes the file at `index` out of the book being edited.
        #[qinvokable]
        #[cxx_name = "removeFile"]
        fn remove_file(self: Pin<&mut Self>, index: i32);

        /// Moves the file at `from` to `to`. The inputs play in the order they are listed and
        /// each of them begins a chapter, so this is what the order of a book is edited with.
        #[qinvokable]
        #[cxx_name = "moveFile"]
        fn move_file(self: Pin<&mut Self>, from: i32, to: i32);

        /// The file at `index`, as it was named.
        #[qinvokable]
        #[cxx_name = "fileAt"]
        fn file_at(self: &Self, index: i32) -> QString;

        /// Where the TAF goes. Empty is the name derived from the first input.
        #[qinvokable]
        #[cxx_name = "setOutput"]
        fn set_output(self: Pin<&mut Self>, text: &QString);

        /// The chapter marks that override whatever the inputs carry, separated by commas.
        #[qinvokable]
        #[cxx_name = "setChapters"]
        fn set_chapters(self: Pin<&mut Self>, text: &QString);

        /// How much is dropped from the very start.
        #[qinvokable]
        #[cxx_name = "setSkipLeading"]
        fn set_skip_leading(self: Pin<&mut Self>, text: &QString);

        /// Whether the silence the first chapter begins with is dropped.
        #[qinvokable]
        #[cxx_name = "setTrimLeading"]
        fn set_trim_leading(self: Pin<&mut Self>, on: bool);

        /// Whether the silence every chapter begins with is dropped.
        #[qinvokable]
        #[cxx_name = "setTrimEachChapter"]
        fn set_trim_each_chapter(self: Pin<&mut Self>, on: bool);

        /// How much silence goes in front of the first chapter.
        #[qinvokable]
        #[cxx_name = "setAddPauseLeading"]
        fn set_add_pause_leading(self: Pin<&mut Self>, text: &QString);

        /// How much silence goes in front of every chapter.
        #[qinvokable]
        #[cxx_name = "setAddPauseEach"]
        fn set_add_pause_each(self: Pin<&mut Self>, text: &QString);

        /// Whether the cover art an input carries is written beside the TAF.
        #[qinvokable]
        #[cxx_name = "setExtractCover"]
        fn set_extract_cover(self: Pin<&mut Self>, on: bool);

        /// What the output field holds.
        #[qinvokable]
        #[cxx_name = "outputText"]
        fn output_text(self: &Self) -> QString;

        /// Where the TAF goes while nobody has typed an output: what the field stands empty for,
        /// and nothing at all while the book holds no file to derive a name from.
        #[qinvokable]
        #[cxx_name = "derivedOutput"]
        fn derived_output(self: &Self) -> QString;

        /// What the chapter field holds.
        #[qinvokable]
        #[cxx_name = "chaptersText"]
        fn chapters_text(self: &Self) -> QString;

        /// What the skip-leading field holds.
        #[qinvokable]
        #[cxx_name = "skipLeadingText"]
        fn skip_leading_text(self: &Self) -> QString;

        /// Whether the leading silence is dropped.
        #[qinvokable]
        #[cxx_name = "trimLeading"]
        fn trim_leading(self: &Self) -> bool;

        /// Whether every chapter's leading silence is dropped.
        #[qinvokable]
        #[cxx_name = "trimEachChapter"]
        fn trim_each_chapter(self: &Self) -> bool;

        /// What the leading-pause field holds.
        #[qinvokable]
        #[cxx_name = "addPauseLeadingText"]
        fn add_pause_leading_text(self: &Self) -> QString;

        /// What the every-chapter-pause field holds.
        #[qinvokable]
        #[cxx_name = "addPauseEachText"]
        fn add_pause_each_text(self: &Self) -> QString;

        /// Whether the cover is written beside the TAF.
        #[qinvokable]
        #[cxx_name = "extractCover"]
        fn extract_cover(self: &Self) -> bool;

        /// Queues the book being edited and empties the panel; `false` where what was typed is
        /// no conversion or would write over another book, which `panelError` then says.
        #[qinvokable]
        #[cxx_name = "addToBatch"]
        fn add_to_batch(self: Pin<&mut Self>) -> bool;

        /// Moves the queued book at `index` back into the editing area, exactly as it was typed.
        #[qinvokable]
        #[cxx_name = "reopenRow"]
        fn reopen_row(self: Pin<&mut Self>, index: i32);

        /// Takes the queued book at `index` out of the queue.
        #[qinvokable]
        #[cxx_name = "removeRow"]
        fn remove_row(self: Pin<&mut Self>, index: i32);

        /// What the book at `index` is called.
        #[qinvokable]
        #[cxx_name = "bookTitle"]
        fn book_title(self: &Self, index: i32) -> QString;

        /// How many files the book at `index` holds and how long they play.
        #[qinvokable]
        #[cxx_name = "bookMeta"]
        fn book_meta(self: &Self, index: i32) -> QString;

        /// What the book at `index` is doing: `ready`, `converting`, `done`, `failed` or
        /// `cancelled`.
        #[qinvokable]
        #[cxx_name = "bookStateName"]
        fn book_state_name(self: &Self, index: i32) -> QString;

        /// How much of the book at `index` has been converted, from 0 to 1 — or a negative
        /// fraction where the book states no length, which is a stripe rather than a percent.
        #[qinvokable]
        #[cxx_name = "bookProgress"]
        fn book_progress(self: &Self, index: i32) -> f64;

        /// What the book at `index` is doing or came to, in words.
        #[qinvokable]
        #[cxx_name = "bookResult"]
        fn book_result(self: &Self, index: i32) -> QString;

        /// Converts every book that is waiting, the one being edited included; `false` where
        /// there is nothing to convert or the set was refused, which `panelError` then says.
        #[qinvokable]
        fn convert(self: Pin<&mut Self>) -> bool;

        /// Stops the running batch: the books converting stop between chunks, and the ones
        /// waiting never start.
        #[qinvokable]
        fn cancel(self: Pin<&mut Self>);

        /// Empties the queue and the book being edited.
        #[qinvokable]
        #[cxx_name = "clearAll"]
        fn clear_all(self: Pin<&mut Self>);

        /// Puts a made-up book through a made-up conversion, so that the self-test run draws the
        /// chrome a batch draws. Nothing is converted, read or written.
        #[qinvokable]
        #[cxx_name = "smokeDrill"]
        fn smoke_drill(self: Pin<&mut Self>);
    }

    impl cxx_qt::Threading for TaffleApp {}
}

impl qobject::TaffleApp {
    /// Adds every file `paths` names to the book being edited.
    pub fn add_files(mut self: Pin<&mut Self>, paths: &QString) {
        let added: Vec<PathBuf> = paths
            .to_string()
            .lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect();
        // Each file is asked how long it plays as it lands, so a row states the length of what is
        // about to be converted before any of it has been.
        let lengths = probed(&added);
        {
            let mut rust = self.as_mut().rust_mut();
            rust.panel.files.extend(added);
            rust.panel_durations.extend(lengths);
        }
        self.as_mut().refresh();
    }

    /// Takes the file at `index` out of the book being edited.
    pub fn remove_file(mut self: Pin<&mut Self>, index: i32) {
        let Some(at) = row_of(index, self.rust().panel.files.len()) else {
            return;
        };
        {
            let mut rust = self.as_mut().rust_mut();
            rust.panel.files.remove(at);
            // The lengths are index-aligned with the files, so the check above is the check for
            // both and one leaves with the other.
            rust.panel_durations.remove(at);
        }
        self.as_mut().refresh();
    }

    /// Moves the file at `from` to `to`.
    pub fn move_file(mut self: Pin<&mut Self>, from: i32, to: i32) {
        let count = self.rust().panel.files.len();
        let (Some(from), Some(to)) = (row_of(from, count), row_of(to, count)) else {
            return;
        };
        if from == to {
            return;
        }
        {
            let mut rust = self.as_mut().rust_mut();
            let moved = rust.panel.files.remove(from);
            rust.panel.files.insert(to, moved);
            // Both rows were in range, so taking one out leaves the other one still in range.
            let length = rust.panel_durations.remove(from);
            rust.panel_durations.insert(to, length);
        }
        self.as_mut().refresh();
    }

    /// The file at `index`, as it was named.
    pub fn file_at(&self, index: i32) -> QString {
        let named = usize::try_from(index)
            .ok()
            .and_then(|at| self.rust().panel.files.get(at))
            .map(|path| path.display().to_string())
            .unwrap_or_default();

        QString::from(named.as_str())
    }

    /// Where the TAF goes.
    pub fn set_output(mut self: Pin<&mut Self>, text: &QString) {
        self.as_mut().rust_mut().panel.output_text = text.to_string();
        self.as_mut().refresh();
    }

    /// The chapter marks that override whatever the inputs carry.
    pub fn set_chapters(mut self: Pin<&mut Self>, text: &QString) {
        self.as_mut().rust_mut().panel.chapters_text = text.to_string();
        self.as_mut().refresh();
    }

    /// How much is dropped from the very start.
    pub fn set_skip_leading(mut self: Pin<&mut Self>, text: &QString) {
        self.as_mut().rust_mut().panel.skip_leading_text = text.to_string();
        self.as_mut().refresh();
    }

    /// Whether the silence the first chapter begins with is dropped.
    pub fn set_trim_leading(mut self: Pin<&mut Self>, on: bool) {
        self.as_mut().rust_mut().panel.trim_leading = on;
        self.as_mut().refresh();
    }

    /// Whether the silence every chapter begins with is dropped.
    pub fn set_trim_each_chapter(mut self: Pin<&mut Self>, on: bool) {
        self.as_mut().rust_mut().panel.trim_each_chapter = on;
        self.as_mut().refresh();
    }

    /// How much silence goes in front of the first chapter.
    pub fn set_add_pause_leading(mut self: Pin<&mut Self>, text: &QString) {
        self.as_mut().rust_mut().panel.add_pause_leading_text = text.to_string();
        self.as_mut().refresh();
    }

    /// How much silence goes in front of every chapter.
    pub fn set_add_pause_each(mut self: Pin<&mut Self>, text: &QString) {
        self.as_mut().rust_mut().panel.add_pause_each_text = text.to_string();
        self.as_mut().refresh();
    }

    /// Whether the cover art an input carries is written beside the TAF.
    pub fn set_extract_cover(mut self: Pin<&mut Self>, on: bool) {
        self.as_mut().rust_mut().panel.extract_cover = on;
        self.as_mut().refresh();
    }

    /// What the output field holds.
    pub fn output_text(&self) -> QString {
        QString::from(self.rust().panel.output_text.as_str())
    }

    /// Where the TAF goes while nobody has typed an output.
    pub fn derived_output(&self) -> QString {
        let derived = plan::derived_output(&self.rust().panel)
            .map(|path| path.display().to_string())
            .unwrap_or_default();

        QString::from(derived.as_str())
    }

    /// What the chapter field holds.
    pub fn chapters_text(&self) -> QString {
        QString::from(self.rust().panel.chapters_text.as_str())
    }

    /// What the skip-leading field holds.
    pub fn skip_leading_text(&self) -> QString {
        QString::from(self.rust().panel.skip_leading_text.as_str())
    }

    /// Whether the leading silence is dropped.
    pub fn trim_leading(&self) -> bool {
        self.rust().panel.trim_leading
    }

    /// Whether every chapter's leading silence is dropped.
    pub fn trim_each_chapter(&self) -> bool {
        self.rust().panel.trim_each_chapter
    }

    /// What the leading-pause field holds.
    pub fn add_pause_leading_text(&self) -> QString {
        QString::from(self.rust().panel.add_pause_leading_text.as_str())
    }

    /// What the every-chapter-pause field holds.
    pub fn add_pause_each_text(&self) -> QString {
        QString::from(self.rust().panel.add_pause_each_text.as_str())
    }

    /// Whether the cover is written beside the TAF.
    pub fn extract_cover(&self) -> bool {
        self.rust().panel.extract_cover
    }

    /// Queues the book being edited and leaves a fresh panel behind.
    pub fn add_to_batch(mut self: Pin<&mut Self>) -> bool {
        let plan = match plan::capture(&self.rust().panel) {
            Ok(plan) => plan,
            Err(error) => {
                self.as_mut().refuse(&error.to_string());
                return false;
            }
        };

        // The book joining the queue is held against every book already in it, while there is
        // still nothing on the disk to undo.
        let refused = {
            let rust = self.rust();
            let mut plans = rust.plans(&rust.waiting());
            plans.push(&plan);
            let jobs: Vec<taffle::ConvertJob> = plans.iter().map(|plan| plan.job.clone()).collect();

            taffle::refuse_collisions(&jobs)
                .err()
                .map(|error| collision_refusal(&plans, &error))
        };
        if let Some(why) = refused {
            self.as_mut().refuse(&why);
            return false;
        }

        let probed = stated_length(&self.rust().panel_durations);
        {
            let mut rust = self.as_mut().rust_mut();
            rust.books.push(Book {
                plan,
                probed,
                state: BookState::Ready,
            });
            // What is left behind is a panel at its own defaults, the cover switch included —
            // adding a book starts the next one rather than leaving the last one's settings on.
            rust.panel = plan::Panel::default();
            rust.panel_durations.clear();
        }
        self.as_mut().refresh();

        true
    }

    /// Moves the queued book at `index` back into the editing area.
    pub fn reopen_row(mut self: Pin<&mut Self>, index: i32) {
        // A running batch names its books by where they sit in the queue, so nothing moves in it
        // while one runs.
        if *self.as_ref().converting() {
            return;
        }
        let Some(at) = row_of(index, self.rust().books.len()) else {
            return;
        };
        // A book that has run is a result to read and not a book to edit; only one still waiting
        // comes back.
        if !matches!(
            self.rust().books.get(at).map(|book| &book.state),
            Some(BookState::Ready)
        ) {
            return;
        }

        // The row leaves the queue with its panel: a book being edited again is being edited, not
        // queued twice — and a second row for it would collide with the first over the one file
        // both of them write. What comes back is what was typed, down to `12:34` rather than the
        // frames it was read as.
        let panel = self.as_mut().rust_mut().books.remove(at).plan.panel;
        // Only the sum of the files' lengths was kept with the book, and it is the per-file
        // lengths a panel needs, so they are read off the files again.
        let lengths = probed(&panel.files);
        {
            let mut rust = self.as_mut().rust_mut();
            rust.panel = panel;
            rust.panel_durations = lengths;
        }
        self.as_mut().refresh();
    }

    /// Takes the queued book at `index` out of the queue.
    pub fn remove_row(mut self: Pin<&mut Self>, index: i32) {
        // See `reopen_row`: the queue does not move under a running batch.
        if *self.as_ref().converting() {
            return;
        }
        let Some(at) = row_of(index, self.rust().books.len()) else {
            return;
        };
        self.as_mut().rust_mut().books.remove(at);
        self.as_mut().refresh();
    }

    /// What the book at `index` is called.
    pub fn book_title(&self, index: i32) -> QString {
        self.about(index, |book| book.plan.title.clone())
    }

    /// How many files the book at `index` holds and how long they play.
    pub fn book_meta(&self, index: i32) -> QString {
        self.about(index, Book::meta)
    }

    /// What the book at `index` is doing.
    pub fn book_state_name(&self, index: i32) -> QString {
        self.about(index, |book| book.state.name().to_owned())
    }

    /// How much of the book at `index` has been converted.
    pub fn book_progress(&self, index: i32) -> f64 {
        // A row that is no longer there has no length either, and a stripe is what a row that
        // states none shows.
        self.book(index).map_or(NO_LENGTH, Book::progress)
    }

    /// What the book at `index` is doing or came to, in words.
    pub fn book_result(&self, index: i32) -> QString {
        self.about(index, Book::result)
    }

    /// Converts every book that is waiting, the one being edited included.
    pub fn convert(mut self: Pin<&mut Self>) -> bool {
        // A second batch would take the queue over from the one already running, and the run
        // that is on is stopped rather than joined.
        if *self.as_ref().converting() {
            return false;
        }
        // The book being edited converts with the rest, captured exactly as adding it would
        // capture it: the flow that converts one book never has to press Add.
        if !self.rust().panel.files.is_empty() && !self.as_mut().add_to_batch() {
            return false;
        }

        let batch = self.rust().waiting();
        if batch.is_empty() {
            return false;
        }
        let (jobs, refused) = {
            let rust = self.rust();
            let plans = rust.plans(&batch);
            let jobs: Vec<taffle::ConvertJob> = plans.iter().map(|plan| plan.job.clone()).collect();
            // Every book was held against the queue as it stood when it was added; this is the
            // whole set as it will actually run, held against itself once more.
            let refused = taffle::refuse_collisions(&jobs)
                .err()
                .map(|error| collision_refusal(&plans, &error));

            (jobs, refused)
        };
        if let Some(why) = refused {
            self.as_mut().refuse(&why);
            return false;
        }

        let cancel = {
            let mut rust = self.as_mut().rust_mut();
            rust.batch = batch;
            // A run begins with the flag down, whatever the run before it left it as.
            rust.cancel.store(false, Ordering::SeqCst);
            Arc::clone(&rust.cancel)
        };
        let qt = self.as_ref().qt_thread();
        // The batch runs off the GUI thread and hands every word about itself to the Qt thread's
        // own queue, which is what puts it back on the thread the rows are drawn on.
        std::thread::spawn(move || {
            worker::run_batch(
                &jobs,
                worker::concurrency_cap(),
                &cancel,
                taffle::run_convert,
                |event| {
                    // A queue that finds nobody home is a window that has already gone.
                    let _ = qt.queue(move |app| app.apply(event));
                },
            );
        });
        self.as_mut().set_converting(true);
        self.as_mut().refresh();

        true
    }

    /// Stops the running batch.
    pub fn cancel(self: Pin<&mut Self>) {
        // Nothing is said here about what the rows are doing: the batch reports every book it was
        // started with, stopped ones included, and each row hears about itself then.
        self.rust().cancel.store(true, Ordering::SeqCst);
    }

    /// Empties the queue and the book being edited.
    pub fn clear_all(mut self: Pin<&mut Self>) {
        // See `reopen_row`: the queue does not move under a running batch.
        if *self.as_ref().converting() {
            return;
        }
        {
            let mut rust = self.as_mut().rust_mut();
            rust.books.clear();
            rust.panel = plan::Panel::default();
            rust.panel_durations.clear();
        }
        self.as_mut().refresh();
    }

    /// Puts a made-up batch of books through made-up conversions.
    ///
    /// The self-test boots the whole window and leaves on the first frame it draws, and an empty
    /// window draws none of what a batch draws: no row, no bar, no line saying what a book came
    /// to. So the drill queues one book for every way a batch can leave one — converted, failed,
    /// stopped — and hands a whole batch's worth of words to the very [`Self::apply`] a running
    /// batch reports through. The frame that is drawn is a frame of a batch that has just
    /// finished: three rows, each saying what became of its book in the colour that goes with it,
    /// and the next book already being filled in underneath.
    ///
    /// Nothing is converted, read or written: the paths name files that are not there, and what
    /// the conversions came to is fabricated.
    pub fn smoke_drill(mut self: Pin<&mut Self>) {
        /// What the drill's books are named after: one of them per state a book that has run can
        /// be left in, each named after the state it is driven to.
        const STEM: &str = "taffle-smoke-drill";
        /// How long the fabricated conversion says it wrote: a second past the minute every drill
        /// book states it plays, which is what a conversion that adds a pause writes.
        const WRITTEN: Duration = Duration::from_secs(61);

        // Only ever the self-test's. The drill sits on the object the shipped window holds, which
        // is the only place QML could reach it from, and a window that ran it would be showing
        // books nobody queued.
        if !*self.as_ref().smoke_mode() {
            return;
        }

        let converted = format!("{STEM}-converted");
        let failed = format!("{STEM}-failed");
        let queued = [
            drill_book(&converted),
            drill_book(&failed),
            drill_book(&format!("{STEM}-stopped")),
        ];
        {
            let mut rust = self.as_mut().rust_mut();
            let first = rust.books.len();
            rust.books.extend(queued);
            // The jobs of this batch are the books just queued, in the order they were queued,
            // wherever they landed.
            rust.batch = (first..rust.books.len()).collect();
        }
        // A run is on from where it is started until it says it is done, so the drill's is too:
        // what turns it off again is `BatchDone` going through `apply`, the way a real one's does.
        self.as_mut().set_converting(true);

        let second = |seconds: u64| seconds * u64::from(RATE);
        for event in [
            worker::Event::Started { index: 0 },
            // A quarter of the book, then half of it, then three quarters: every step leaves the
            // bar somewhere a bar can be read.
            worker::Event::Progress {
                index: 0,
                samples_done: second(15),
            },
            worker::Event::Progress {
                index: 0,
                samples_done: second(30),
            },
            worker::Event::Progress {
                index: 0,
                samples_done: second(45),
            },
            worker::Event::Finished {
                index: 0,
                result: Ok(taffle::JobOutcome {
                    taf_path: drill_path(&converted, "taf"),
                    cover_path: None,
                    cover_error: None,
                    report: taffle::ConversionReport {
                        chapters: vec![],
                        duration: WRITTEN,
                        cover: None,
                        audio_id: taffle::AudioId::new(1),
                    },
                }),
            },
            worker::Event::Started { index: 1 },
            worker::Event::Finished {
                index: 1,
                // The layer a conversion gives up at and what was said under it, joined the way
                // the batch joins a chain. The input really is not there, so this is where a run
                // of this job would give up; the words under it are made up, because asking the
                // filesystem for the real ones is the reading the drill does not do.
                result: Err(worker::BookFailure::Failed(format!(
                    "cannot open input {}: the file is not there",
                    drill_path(&failed, "m4b").display()
                ))),
            },
            // The last book is reported without ever starting, which is what a batch somebody
            // stopped here would say about the one still waiting.
            worker::Event::Finished {
                index: 2,
                result: Err(worker::BookFailure::Cancelled),
            },
            worker::Event::BatchDone,
        ] {
            self.as_mut().apply(event);
        }

        // The editing area is drawn as well, holding the book that is filled in next — which is
        // what stands in it once a batch is over. The file goes in behind `add_files`, which would
        // read a length off a file that is not there.
        {
            let mut rust = self.as_mut().rust_mut();
            rust.panel
                .files
                .push(drill_path(&format!("{STEM}-next"), "m4b"));
            // The lengths are index-aligned with the files, and nothing was probed.
            rust.panel_durations.push(None);
        }
        self.as_mut().refresh();
    }

    /// Adopts one word from the running batch onto the GUI thread, where it was queued by
    /// [`Threading`] — never exposed to QML.
    fn apply(mut self: Pin<&mut Self>, event: worker::Event) {
        // The last word of a batch is about the run rather than about a row, and the property
        // that says a run is on is Qt's.
        if matches!(event, worker::Event::BatchDone) {
            self.as_mut().set_converting(false);
        }
        self.as_mut().rust_mut().adopt(event);
        self.as_mut().refresh();
    }

    /// Recomputes everything QML reads off the panel and the queue, and bumps the revision the
    /// delegates re-read on.
    fn refresh(mut self: Pin<&mut Self>) {
        let (files, books, error, warning) = {
            let rust = self.rust();
            let (error, warning) = match plan::capture(&rust.panel) {
                // A panel nobody has put a file in yet is not a mistake, it is a panel being
                // filled in — so nothing is said about it here. Having nothing to convert is the
                // answer where a book is added, and that is where it is said.
                Err(plan::CaptureError::NoFiles) => (String::new(), String::new()),
                Err(error) => (error.to_string(), String::new()),
                Ok(plan) => (
                    String::new(),
                    plan::chapter_warning(&plan.job.options.chapter_mode).unwrap_or_default(),
                ),
            };

            (
                counted(rust.panel.files.len()),
                counted(rust.books.len()),
                error,
                warning,
            )
        };
        self.as_mut().set_file_count(files);
        self.as_mut().set_book_count(books);
        self.as_mut().set_panel_error(QString::from(error.as_str()));
        self.as_mut()
            .set_chapter_warning(QString::from(warning.as_str()));
        // What a delegate reads the revision for is that it is not what it was, so a bump that
        // wrapped is still a bump — and it is one number rather than an arithmetic overflow.
        let bumped = self.as_ref().revision().wrapping_add(1);
        self.as_mut().set_revision(bumped);
    }

    /// Says `why` about the panel, over what the panel says about itself.
    ///
    /// Refreshing first and putting the refusal on top is what makes it stand: it is the answer
    /// until the panel changes, and the next thing typed recomputes what the panel says.
    fn refuse(mut self: Pin<&mut Self>, why: &str) {
        self.as_mut().refresh();
        self.as_mut().set_panel_error(QString::from(why));
    }

    /// What `say` says about the book at `index`, and nothing at all where there is no such book:
    /// a delegate can outlive the row it was drawn for by a frame, and nothing draws nothing.
    fn about(&self, index: i32, say: impl Fn(&Book) -> String) -> QString {
        let said = self.book(index).map(say).unwrap_or_default();

        QString::from(said.as_str())
    }

    /// The book at `index`, where there is one.
    fn book(&self, index: i32) -> Option<&Book> {
        self.rust().books.get(usize::try_from(index).ok()?)
    }
}

/// The state behind [`qobject::TaffleApp`]: the book being edited, the queue it joins, and the
/// batch that runs it.
pub struct TaffleAppRust {
    /// Bumped whenever anything a delegate reads may have changed; QML re-reads on the bump.
    revision: i32,
    /// True while a batch is running: the chrome locks the editing side and Convert is Cancel.
    converting: bool,
    /// How many files the book being edited holds.
    file_count: i32,
    /// How many books are queued.
    book_count: i32,
    /// Why what was typed is no conversion, or nothing at all where it is one.
    panel_error: QString,
    /// That the chapter plan is longer than a box plays, or nothing at all. Not a refusal.
    chapter_warning: QString,
    /// The book being edited, as it is typed.
    panel: plan::Panel,
    /// Probed length per panel file, index-aligned; a probe that failed is None and the book
    /// shows no length (and a stripe, not a percent).
    panel_durations: Vec<Option<Duration>>,
    /// The queue, in the order it was added to.
    books: Vec<Book>,
    /// Which book each job of the running batch belongs to, by its place in the queue: a batch is
    /// the books that were waiting when Convert was pressed, and the rows that already ran stay
    /// where they are — so a job's number is not a row's. Only ever meaningful while `converting`,
    /// because Convert rewrites it before the batch it describes can say a word.
    batch: Vec<usize>,
    /// Raised to stop the running batch; lowered again where the next one starts.
    cancel: Arc<AtomicBool>,
    /// True when the process was started with `--smoke`: the window reads it and runs the drill.
    smoke_mode: bool,
}

impl Default for TaffleAppRust {
    fn default() -> Self {
        Self {
            revision: 0,
            converting: false,
            file_count: 0,
            book_count: 0,
            panel_error: QString::default(),
            chapter_warning: QString::default(),
            panel: plan::Panel::default(),
            panel_durations: Vec::new(),
            books: Vec::new(),
            batch: Vec::new(),
            cancel: Arc::new(AtomicBool::new(false)),
            // QML instantiates this object, so a constructor argument cannot carry the flag;
            // it arrives through the process-wide parsed CLI instead.
            smoke_mode: crate::cli().smoke,
        }
    }
}

impl TaffleAppRust {
    /// Where one word from the running batch leaves the queue.
    ///
    /// Which row a job is about is looked up rather than assumed — see [`TaffleAppRust::batch`] —
    /// and a job number that names no row is a batch nobody is showing any more, which is nothing
    /// to do rather than something to report.
    fn adopt(&mut self, event: worker::Event) {
        let (job, state) = match event {
            worker::Event::Started { index } => (index, BookState::Converting { samples_done: 0 }),
            worker::Event::Progress {
                index,
                samples_done,
            } => (index, BookState::Converting { samples_done }),
            worker::Event::Finished { index, result } => (index, finished(result)),
            // The run's own last word, which is the caller's: see `qobject::TaffleApp::apply`.
            worker::Event::BatchDone => return,
        };

        let Some(&at) = self.batch.get(job) else {
            return;
        };
        if let Some(book) = self.books.get_mut(at) {
            book.state = state;
        }
    }

    /// Where the books that are still waiting sit in the queue, in the order they will run.
    fn waiting(&self) -> Vec<usize> {
        self.books
            .iter()
            .enumerate()
            .filter(|(_, book)| matches!(book.state, BookState::Ready))
            .map(|(at, _)| at)
            .collect()
    }

    /// The plans of the books at `rows`, in the order the rows are named.
    fn plans(&self, rows: &[usize]) -> Vec<&plan::BookPlan> {
        rows.iter()
            .filter_map(|at| self.books.get(*at))
            .map(|book| &book.plan)
            .collect()
    }
}

/// A book in the queue: what it will convert to, how long it says it plays, and where it is.
struct Book {
    /// The conversion it was captured as, with the panel it was typed in kept beside it.
    plan: plan::BookPlan,
    /// How long the book states it plays: the sum of what its files state, where every one of
    /// them stated one. See [`stated_length`].
    probed: Option<Duration>,
    /// What the book is doing, or what it came to.
    state: BookState,
}

impl Book {
    /// What the row says under the title: how many files the book holds, and how long they state
    /// they play where they state it at all.
    fn meta(&self) -> String {
        let files = self.plan.job.inputs.len();
        let held = if files == 1 {
            "1 file".to_owned()
        } else {
            format!("{files} files")
        };

        match self.probed {
            Some(length) => format!("{held} · {}", clock(length)),
            None => held,
        }
    }

    /// What the row says the book is doing or came to, and nothing at all for one that has not
    /// run: a book that is waiting has the queue's own meta line to show.
    fn result(&self) -> String {
        match &self.state {
            BookState::Ready => String::new(),
            // The seconds are counted the way the command line counts them: the one being encoded
            // is not counted until it has been.
            BookState::Converting { samples_done } => {
                let played = Duration::from_secs(samples_done / u64::from(RATE));
                format!("{} encoded", clock(played))
            }
            BookState::Done { result_line } => result_line.clone(),
            BookState::Failed { message } => message.clone(),
            BookState::Cancelled => removed_note("the conversion was stopped"),
        }
    }

    /// How much of the book has been converted, or [`NO_LENGTH`] where there is nothing to count
    /// against.
    fn progress(&self) -> f64 {
        let Some(probed) = self.probed else {
            return NO_LENGTH;
        };
        let total = probed.as_secs_f64() * f64::from(RATE);
        // A file that states it plays no time at all states no length to count against either,
        // and dividing by it is no fraction.
        if total <= 0.0 {
            return NO_LENGTH;
        }
        // A book is done when the conversion says so, so the count never has to reach the total —
        // and the frames a person could sit through are far inside the integers a float holds
        // exactly anyway.
        #[allow(clippy::cast_precision_loss)]
        let done = self.state.samples_done() as f64;

        (done / total).clamp(0.0, NEARLY_DONE)
    }
}

/// Where a book is: waiting, running, or what it came to.
enum BookState {
    /// Queued, and nothing has been done to it.
    Ready,
    /// Converting right now.
    Converting {
        /// How much audio has gone into the file, in frames of one channel at 48 kHz.
        samples_done: u64,
    },
    /// Converted. Holds the line the command line prints for exactly this outcome.
    Done {
        /// What was written, how long it plays and how many chapters it holds.
        result_line: String,
    },
    /// Did not convert.
    Failed {
        /// The rendered failure chain, and the note that nothing half-written was left behind.
        message: String,
    },
    /// Stopped, before or during the conversion.
    Cancelled,
}

impl BookState {
    /// What the chrome calls this state.
    fn name(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Converting { .. } => "converting",
            Self::Done { .. } => "done",
            Self::Failed { .. } => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// How much audio has gone into the file so far, and none for a book that is not running: a
    /// row only counts against a total while it is converting.
    fn samples_done(&self) -> u64 {
        match self {
            Self::Converting { samples_done } => *samples_done,
            _ => 0,
        }
    }
}

/// What a job that is over leaves its row in.
fn finished(result: Result<taffle::JobOutcome, worker::BookFailure>) -> BookState {
    match result {
        Ok(outcome) => BookState::Done {
            result_line: report_line(&outcome),
        },
        Err(worker::BookFailure::Failed(chain)) => BookState::Failed {
            message: removed_note(&chain),
        },
        // Being stopped is the one failure somebody asked for, and the row says so rather than
        // rendering a chain nobody needs to read.
        Err(worker::BookFailure::Cancelled) => BookState::Cancelled,
    }
}

/// What a converted book says: the line the command line prints for it, and the cover beside it.
///
/// The cover is a file beside the file, so it is a line of its own the way the command line writes
/// it — either the picture that was written, or why the book stands without one.
fn report_line(outcome: &taffle::JobOutcome) -> String {
    let chapters = outcome.report.chapters.len();
    let plural = if chapters == 1 { "chapter" } else { "chapters" };
    let mut lines = vec![format!(
        "wrote {} ({}, {chapters} {plural})",
        outcome.taf_path.display(),
        clock(outcome.report.duration),
    )];

    if let Some(cover) = &outcome.cover_path {
        lines.push(format!("wrote {}", cover.display()));
    }
    if let Some(why) = &outcome.cover_error {
        lines.push(format!("no cover was written: {why}"));
    }

    lines.join("\n")
}

/// `reason`, and what goes with every book that did not make it: the batch removes the file a
/// conversion had only half written, so a row that did not convert says nothing was left behind.
fn removed_note(reason: &str) -> String {
    format!("{reason}; the unfinished file was removed")
}

/// The made-up book the smoke drill queues under `stem`: one input named after it, an output
/// beside it, and a length it states.
///
/// Built rather than captured: `plan::capture` answers with a `Result`, and a drill that could
/// quietly decline to run would leave the self-test passing while proving nothing. Neither of the
/// two paths is ever opened.
fn drill_book(stem: &str) -> Book {
    /// How long a drill book states it plays. A book that states none shows a stripe instead of a
    /// percent, and a bar counting a real fraction is the one worth drawing.
    const STATED: Duration = Duration::from_secs(60);

    let panel = plan::Panel {
        files: vec![drill_path(stem, "m4b")],
        ..plan::Panel::default()
    };

    Book {
        plan: plan::BookPlan {
            // A captured book takes its title from its first input, so a hand-built one says the
            // same.
            title: stem.to_owned(),
            job: taffle::ConvertJob {
                inputs: panel.files.clone(),
                output: Some(drill_path(stem, "taf")),
                options: taffle::Conversion::default(),
                write_cover: panel.extract_cover,
            },
            panel,
        },
        probed: Some(STATED),
        state: BookState::Ready,
    }
}

/// What a drill file named `stem` would be called, if anything ever wrote one: a name under the
/// temp directory, so that a path which did escape the drill could not land in anybody's work.
fn drill_path(stem: &str, extension: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{stem}.{extension}"))
}

/// How long each of `files` states it plays, index-aligned with them.
///
/// Every way a probe can come to nothing — a file that cannot be read, a container this build does
/// not know, one that states no length — is the same answer to a row: no stated length. So none of
/// them is told apart here.
fn probed(files: &[PathBuf]) -> Vec<Option<Duration>> {
    files
        .iter()
        .map(|path| taffle::probe_duration(path).ok())
        .collect()
}

/// How long a book states it plays: the sum of what its files state, and nothing at all where any
/// one of them states none.
///
/// A sum that is missing a file is not the length of the book, and a row showing it would state a
/// length that is wrong — with a percent counted against it that is wrong by the same amount.
fn stated_length(lengths: &[Option<Duration>]) -> Option<Duration> {
    lengths.iter().copied().sum()
}

/// What a refused set of conversions reads as beside a queue of titles: what the check said,
/// behind the name of the book it is about.
///
/// A collision names a path and a person is looking at names, so the title of the book that writes
/// that path goes in front of it. Every plan resolves its own output where it is captured, so the
/// book is found by the path it states rather than by deriving one here.
fn collision_refusal(plans: &[&plan::BookPlan], error: &taffle::CollisionError) -> String {
    let written = match error {
        taffle::CollisionError::OutputIsInput { output }
        | taffle::CollisionError::DuplicateOutput { output } => Some(output),
        // The check may come to refuse something that names no file; it still reads as what it is.
        _ => None,
    };
    let culprit = written.and_then(|output| {
        plans
            .iter()
            .find(|plan| plan.job.output.as_deref() == Some(output.as_path()))
    });

    match culprit {
        Some(plan) => format!("{}: {error}", plan.title),
        None => error.to_string(),
    }
}

/// The row `index` names, where it names one of `count` rows — the checked number a row is taken
/// out of a list by, which reading one only needs `get` for.
///
/// QML counts rows in `i32` and a delegate can outlive its row by a frame, so a row number that is
/// negative or past the end is a row that is not there rather than a mistake.
fn row_of(index: i32, count: usize) -> Option<usize> {
    let at = usize::try_from(index).ok()?;

    (at < count).then_some(at)
}

/// `len` as QML counts: a list longer than an `i32` counts is not one anybody scrolled to the end
/// of, and saying it is the longest countable list is closer than saying it is empty.
fn counted(len: usize) -> i32 {
    i32::try_from(len).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::PathBuf;
    use std::time::Duration;

    use super::{collision_refusal, stated_length, Book, BookState, TaffleAppRust};
    use crate::plan::{capture, Panel};
    use crate::worker::{BookFailure, Event};

    /// A queued book of `files`, stating the length `probed` says it plays.
    fn book(files: &[&str], probed: Option<Duration>) -> Book {
        let panel = Panel {
            files: files.iter().map(PathBuf::from).collect(),
            ..Panel::default()
        };
        Book {
            plan: capture(&panel).expect("a plan"),
            probed,
            state: BookState::Ready,
        }
    }

    /// The queue `books`, with `batch` naming the row each job of the running batch belongs to.
    fn app(books: Vec<Book>, batch: Vec<usize>) -> TaffleAppRust {
        TaffleAppRust {
            books,
            batch,
            ..TaffleAppRust::default()
        }
    }

    /// The book at `at` in the queue.
    fn row(app: &TaffleAppRust, at: usize) -> &Book {
        app.books.get(at).expect("a row")
    }

    /// What a conversion came to: `chapters` marks over `duration` of audio, and no cover.
    fn outcome(chapters: usize, duration: Duration) -> taffle::JobOutcome {
        taffle::JobOutcome {
            taf_path: PathBuf::from("out/book.taf"),
            cover_path: None,
            cover_error: None,
            report: taffle::ConversionReport {
                chapters: (0..chapters)
                    .map(|at| taffle::ChapterOut {
                        page: taffle::BlockIndex::new(u32::try_from(at).expect("a block index")),
                        start: Duration::ZERO,
                        title: None,
                    })
                    .collect(),
                duration,
                cover: None,
                audio_id: taffle::AudioId::new(1),
            },
        }
    }

    #[test]
    fn what_the_batch_says_lands_on_the_row_the_job_belongs_to() {
        let mut converted = book(&["a.mp3"], None);
        converted.state = BookState::Done {
            result_line: "wrote a.taf (0:30, 1 chapter)".to_owned(),
        };
        // A book that has already converted stays in the queue, so the one job of this batch is
        // the second row — which is the whole reason a job's index is looked up and not used.
        let mut app = app(vec![converted, book(&["b.mp3"], None)], vec![1]);

        app.adopt(Event::Started { index: 0 });
        assert!(matches!(
            row(&app, 1).state,
            BookState::Converting { samples_done: 0 }
        ));
        assert_eq!(row(&app, 1).state.name(), "converting");

        app.adopt(Event::Progress {
            index: 0,
            samples_done: 96_000,
        });
        assert!(matches!(
            row(&app, 1).state,
            BookState::Converting {
                samples_done: 96_000
            }
        ));
        // The row that already converted is nobody's job, and says what it always said.
        assert_eq!(row(&app, 0).result(), "wrote a.taf (0:30, 1 chapter)");
    }

    #[test]
    fn a_book_that_converted_says_what_the_command_line_says_about_it() {
        let mut app = app(vec![book(&["a.mp3"], None)], vec![0]);
        app.adopt(Event::Finished {
            index: 0,
            result: Ok(outcome(16, Duration::from_secs(3852))),
        });
        assert_eq!(row(&app, 0).state.name(), "done");
        assert_eq!(
            row(&app, 0).result(),
            "wrote out/book.taf (1:04:12, 16 chapters)"
        );
    }

    #[test]
    fn the_cover_beside_the_book_is_a_line_of_its_own() {
        let mut written = outcome(1, Duration::from_secs(30));
        written.cover_path = Some(PathBuf::from("out/book.png"));
        let mut with_cover = app(vec![book(&["a.mp3"], None)], vec![0]);
        with_cover.adopt(Event::Finished {
            index: 0,
            result: Ok(written),
        });
        assert_eq!(
            row(&with_cover, 0).result(),
            "wrote out/book.taf (0:30, 1 chapter)\nwrote out/book.png"
        );

        let mut refused = outcome(1, Duration::from_secs(30));
        refused.cover_error = Some("the picture is a WEBP, which nothing here writes".to_owned());
        let mut without_cover = app(vec![book(&["a.mp3"], None)], vec![0]);
        without_cover.adopt(Event::Finished {
            index: 0,
            result: Ok(refused),
        });
        // A cover that could not be written is a note beside a book that converted, never a
        // failure of it.
        assert_eq!(
            row(&without_cover, 0).result(),
            "wrote out/book.taf (0:30, 1 chapter)\nno cover was written: the picture is a WEBP, which nothing here writes"
        );
        assert_eq!(row(&without_cover, 0).state.name(), "done");
    }

    #[test]
    fn a_book_that_failed_says_why_and_that_nothing_half_written_was_left_behind() {
        let mut app = app(vec![book(&["a.mp3"], None)], vec![0]);
        app.adopt(Event::Finished {
            index: 0,
            result: Err(BookFailure::Failed(
                "cannot open input a.mp3: no such file or directory".to_owned(),
            )),
        });
        assert_eq!(row(&app, 0).state.name(), "failed");
        assert_eq!(
            row(&app, 0).result(),
            "cannot open input a.mp3: no such file or directory; the unfinished file was removed"
        );
    }

    #[test]
    fn a_book_that_was_stopped_is_cancelled_rather_than_failed() {
        let mut app = app(vec![book(&["a.mp3"], None)], vec![0]);
        app.adopt(Event::Finished {
            index: 0,
            result: Err(BookFailure::Cancelled),
        });
        assert_eq!(row(&app, 0).state.name(), "cancelled");
        assert_eq!(
            row(&app, 0).result(),
            "the conversion was stopped; the unfinished file was removed"
        );
    }

    #[test]
    fn a_word_about_a_job_this_batch_does_not_hold_changes_nothing() {
        let mut app = app(vec![book(&["a.mp3"], None)], vec![0]);
        app.adopt(Event::Started { index: 7 });
        // The last word of a batch is about the run and not about a row, and the property that
        // says a run is on is Qt's.
        app.adopt(Event::BatchDone);
        assert_eq!(row(&app, 0).state.name(), "ready");
        assert_eq!(row(&app, 0).result(), "");
    }

    #[test]
    fn the_bar_counts_the_audio_that_is_in_against_the_length_the_files_state() {
        let mut converting = book(&["a.mp3"], Some(Duration::from_secs(100)));
        converting.state = BookState::Converting {
            samples_done: 50 * 48_000,
        };
        assert!((converting.progress() - 0.5).abs() < f64::EPSILON);

        // A conversion writes more audio than its inputs stated wherever it adds a pause, and a
        // book is not converted until it says so — so the bar stops short of full.
        converting.state = BookState::Converting {
            samples_done: 200 * 48_000,
        };
        assert!((converting.progress() - 0.99).abs() < f64::EPSILON);

        // Every way a probe can come to nothing is one answer here: no length to count against,
        // which a row shows as a stripe rather than a percent.
        assert!(book(&["a.mp3"], None).progress() < 0.0);
        assert!(book(&["a.mp3"], Some(Duration::ZERO)).progress() < 0.0);
    }

    #[test]
    fn a_converting_row_says_how_much_audio_is_in() {
        let mut converting = book(&["a.mp3"], None);
        converting.state = BookState::Converting {
            samples_done: 96_500,
        };
        // The seconds are counted the way the command line counts them: the one being encoded is
        // not counted until it has been.
        assert_eq!(converting.result(), "0:02 encoded");
    }

    #[test]
    fn a_row_says_how_many_files_it_holds_and_how_long_they_play() {
        assert_eq!(
            book(&["a.mp3"], Some(Duration::from_secs(3852))).meta(),
            "1 file · 1:04:12"
        );
        // A book nothing could be read off states no length, and says the files alone.
        assert_eq!(book(&["a.mp3", "b.mp3"], None).meta(), "2 files");
    }

    #[test]
    fn a_book_states_a_length_only_where_every_one_of_its_files_does() {
        let ten = Some(Duration::from_secs(10));
        assert_eq!(
            stated_length(&[ten, Some(Duration::from_secs(20))]),
            Some(Duration::from_secs(30))
        );
        // A sum that is missing a file is not how long the book plays, and a row showing it would
        // be stating a length that is wrong.
        assert_eq!(stated_length(&[ten, None]), None);
    }

    #[test]
    fn a_refused_set_of_conversions_names_the_book_it_is_about() {
        let over_itself = Panel {
            files: vec![PathBuf::from("a/01.mp3")],
            output_text: "a/01.mp3".to_owned(),
            ..Panel::default()
        };
        let plan = capture(&over_itself).expect("a plan");
        let error =
            taffle::refuse_collisions(std::slice::from_ref(&plan.job)).expect_err("a collision");

        assert_eq!(
            collision_refusal(&[&plan], &error),
            "01: the output a/01.mp3 is one of the inputs: converting it would write over the audio being read"
        );
    }
}
