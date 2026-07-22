// trace:STORY-299 | ai:claude — TASK-257's observability seam.
//! The diagnostic breadcrumb log — the ONE place quizdom records something that
//! went wrong but did not stop the session.
//!
//! ## Why a file, and why never the terminal
//!
//! The TUI front-end owns the terminal for the whole session: crossterm's
//! alternate screen plus raw mode (`tui.rs`). Writing to stdout there lands
//! inside a frame ratatui is about to redraw; writing to stderr is invisible at
//! best and display-corrupting at worst. So until STORY-299 the codebase had
//! nowhere at all to put a diagnostic, and the consequence was not "no logs" —
//! it was WRONG CONTENT on screen. `load_probed_terms` swallowed a store
//! failure with `unwrap_or_default()` and the session then rendered the
//! question as defining nothing, which is exactly what a question that probes
//! no terms looks like. A degraded read and an empty graph were
//! indistinguishable.
//!
//! This module is a breadcrumb trail, deliberately NOT a logging framework: no
//! levels, no filters, no targets, no dependency. One append-only file, one
//! line per event, written through one function.
//!
//! ## Bounded (TASK-321)
//!
//! Append-only is not the same as unbounded. A healthy install writes nothing
//! at all — the seam only fires on a degraded read or a failed auto-backup —
//! but a PERSISTENTLY broken store writes a line per probed read per turn, and
//! a diagnostic breadcrumb must not become a disk problem in exactly the
//! situation the user is least likely to be watching. At [`LOG_SIZE_LIMIT`] the
//! contents are copied to `<log>.1` and the live file is truncated back to
//! empty, opening with a line saying so. One kept generation, no rotation state
//! to be wrong.
//!
//! ## Safe with more than one quizdom running (TASK-333)
//!
//! Two quizdom processes share one log — a TUI session in one terminal, a
//! `db-backup` from cron in another — so rotation is a CONCURRENT operation,
//! and the obvious spelling of it is not safe. `stat`-then-`rename` is a
//! time-of-check/time-of-use race: both processes see an over-sized file, the
//! first renames it to `<log>.1`, the second renames the near-empty file that
//! replaced it OVER that rotated generation, and the megabyte of history
//! explaining the breakage is gone — in exactly the pathological case the
//! rotation exists to survive.
//!
//! Two things close it, and they are one mechanism:
//!
//! * Every append takes an **exclusive advisory lock** on the log
//!   ([`std::fs::File::lock`], std since 1.89 — no dependency), so the
//!   size check, the rotation, and the write are one critical section across
//!   processes rather than three racing syscalls.
//! * Rotation **copies then truncates in place** instead of renaming, so the
//!   live log is always the same inode. A writer holding the log open across a
//!   rotation keeps writing to the LIVE file rather than silently into the dead
//!   generation, and the lock always guards the same object. The cost is
//!   copying a megabyte once per megabyte logged, which is nothing against the
//!   guarantee — and the copy lands on a staging name and is renamed into
//!   place BEFORE the truncate, so `<log>.1` is never half-written and a
//!   rotation that fails leaves the log whole rather than empty.
//!
//! A lock that cannot be taken (a filesystem that does not support it) DEGRADES
//! to the unlocked write rather than dropping the breadcrumb: the same call the
//! module makes everywhere else, that a diagnostic which cannot be written is
//! worse than one written imperfectly.
//!
//! ## The invariant
//!
//! **Nothing here ever writes to stdout or stderr.** [`record`] takes no writer
//! and returns nothing, so no caller can aim it at the terminal even by
//! accident, and a failed write is DROPPED rather than reported — a breadcrumb
//! that takes down the session it was meant to explain is worse than no
//! breadcrumb at all.
//!
//! Two tests pin it, and they are complementary rather than redundant
//! (TASK-322). `the_seam_never_touches_the_terminal` scans the seam's own
//! source: lexical, but it covers every path through the module including ones
//! no test exercises, which is the property that actually matters — a print on
//! a rare error branch is precisely what would corrupt a frame in the field.
//! `nothing_reaches_the_terminal_while_the_alternate_screen_is_active` is the
//! behavioural half: it `dup2`s the process's real fd 1 and 2 to a file, enters
//! the alternate screen, drives the seam (including its failure branches), and
//! asserts the captured bytes are EXACTLY crossterm's own enter/leave
//! sequences. A print reached through a re-export, a macro, or a helper in
//! another module passes the scan and fails the capture.
//!
//! Path resolution matches every other quizdom path
//! ([`crate::settings::resolve_log_path`]): `QUIZDOM_LOG_PATH` > `log_path` in
//! `settings.toml` > `$XDG_DATA_HOME/quizdom/quizdom.log`.
//!
//! ## Under `cfg(test)` the sink is a thread-local buffer
//!
//! The TASK-266 pattern (`settings::load_or_seed`): the ~620 in-crate tests
//! must not append to the developer's real log, and the assertion a test wants
//! to make is "the seam recorded this", not "a file grew". A THREAD-LOCAL
//! buffer gives both — the test harness runs each test on its own thread, so
//! [`captured`] is per-test by construction and parallel tests cannot see each
//! other's entries without any locking. The file half stays covered by driving
//! [`append_entry`] directly at a temp path.

use crate::error::QuizdomError;
use chrono::Local;
use std::io::Write;
use std::path::Path;

/// Record one diagnostic event.
///
/// Takes no writer and returns no error on purpose: the two things a caller
/// must not be able to do are route this at the terminal and have it fail their
/// operation. Callers phrase `event` as what happened and what was done about
/// it, since nobody reading the log later has the surrounding context.
pub(crate) fn record(event: &str) {
    // Resolving the path is part of the seam and stays on ONE code path in both
    // builds; it is only the destination that `cfg(test)` swaps out, so a test
    // build cannot diverge from the real one in where it thinks the log lives.
    emit(
        &crate::settings::resolve_log_path(),
        format_entry(&Local::now().to_rfc3339(), event),
    );
}

/// The canonical entry for a read that FAILED and degraded to "no data".
///
/// This is the shape STORY-299 was filed about, so it gets a named helper
/// rather than a hand-rolled string per call site: the log has to say which
/// operation, on which subject, failed how — because the user-visible symptom
/// (an empty list) carries none of that.
pub(crate) fn degraded_read(operation: &str, subject: &str, error: &QuizdomError) {
    record(&format!(
        "degraded read: {operation}({subject}) failed and returned nothing: {error}"
    ));
}

/// `<rfc3339>  <event>` — one line. Newlines inside `event` collapse to spaces
/// so one event stays one line and the file stays greppable.
fn format_entry(at: &str, event: &str) -> String {
    format!("{at}  {}", event.replace('\n', " "))
}

// trace:TASK-321 | ai:claude
/// The size at which the log rotates. A breadcrumb trail must not become a disk
/// problem: the case this bounds is not a healthy install (which writes nothing
/// at all) but a PERSISTENTLY broken store — a graph whose `edges` table is
/// missing writes one line per probed read per turn, unbounded, in exactly the
/// situation where the user is least likely to be watching.
///
/// 1 MiB is roughly ten thousand entries, which is far more history than anyone
/// diagnosing a degraded read reads, and small enough that the worst case costs
/// 2 MiB (this generation plus the one kept beside it).
const LOG_SIZE_LIMIT: u64 = 1024 * 1024;

// trace:TASK-321 | ai:claude
/// The suffix of the one kept previous generation.
const ROTATED_SUFFIX: &str = ".1";

// trace:TASK-333 | ai:claude
/// Where a rotation's copy lands before it is renamed onto [`ROTATED_SUFFIX`],
/// so nobody reading the previous generation can catch it half-copied.
const STAGING_SUFFIX: &str = ".partial";

/// Append one line to `path`, creating the file and any missing parents, and
/// ROTATING first when the file has reached [`LOG_SIZE_LIMIT`].
fn append_entry(path: &Path, line: &str) -> std::io::Result<()> {
    append_entry_bounded(path, line, LOG_SIZE_LIMIT)
}

// trace:TASK-333 | ai:claude
/// [`append_entry`] with the rotation threshold passed in, so the concurrency
/// tests can drive real rotations against a small log instead of writing a
/// megabyte per assertion.
///
/// Append mode, never truncate the file out from under a reader mid-session:
/// the log is a trail across sessions, and the several quizdom processes that
/// may share it must not clobber each other.
///
/// The size check, the rotation, and the write happen under ONE exclusive
/// advisory lock on the log — see the module docs for the race that opens up
/// without it. A lock that cannot be taken is ignored rather than fatal: a
/// breadcrumb written without the guarantee still beats no breadcrumb.
fn append_entry_bounded(path: &Path, line: &str, limit: u64) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    // Released when `file` drops at the end of this call, so every path out —
    // including the `?` on a failed write — unlocks.
    let _ = file.lock();
    // The fresh generation opens by saying it is one, so a reader who tails the
    // log does not silently read a truncated story as the whole story.
    if let Some(note) = rotate_if_full(path, &file, limit) {
        writeln!(file, "{note}")?;
    }
    writeln!(file, "{line}")
}

// trace:TASK-321 | ai:claude
// trace:TASK-333 | ai:claude — copy-then-truncate, under the caller's lock.
/// Move an over-sized log's contents aside, returning the note to open the
/// fresh generation with. The caller must already hold the lock on `file`, and
/// `file` must be the open handle for `path` — the size is read THROUGH it so
/// the check and the rotation cannot disagree about which file they mean.
///
/// Copy-then-truncate rather than `rename`: the live log keeps its identity
/// across a rotation, so a concurrent writer cannot end up appending into the
/// dead generation, and the lock this runs under always guards the same object.
/// The copy lands on a staging name and is RENAMED into place, so `<log>.1` is
/// never observable half-written — it is either the previous generation or the
/// one before it, never a torn mix — and the live log is emptied only once the
/// copy is safely there.
///
/// Failure is silence, for the same reason a failed write is dropped: a
/// breadcrumb that takes down the session it was meant to explain is worse than
/// no breadcrumb. A rotation that cannot happen just leaves the log growing —
/// the state we were already in — and takes its staging file with it rather
/// than leaving litter beside the log.
fn rotate_if_full(path: &Path, file: &std::fs::File, limit: u64) -> Option<String> {
    let size = file.metadata().ok()?.len();
    if size < limit {
        return None;
    }
    let name = path.file_name()?.to_string_lossy().into_owned();
    let previous = path.with_file_name(format!("{name}{ROTATED_SUFFIX}"));
    let staging = path.with_file_name(format!("{name}{ROTATED_SUFFIX}{STAGING_SUFFIX}"));
    if std::fs::copy(path, &staging).is_err() || std::fs::rename(&staging, &previous).is_err() {
        let _ = std::fs::remove_file(&staging);
        return None;
    }
    file.set_len(0).ok()?;
    Some(format!(
        "-- rotated at {size} bytes; the previous entries are in {} --",
        previous.display()
    ))
}

/// The real sink: the resolved log file, and silence when it cannot be written.
#[cfg(not(test))]
fn emit(path: &Path, line: String) {
    let _ = append_entry(path, &line);
}

#[cfg(test)]
thread_local! {
    /// The `cfg(test)` sink — see the module docs.
    static CAPTURED: std::cell::RefCell<Vec<String>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

#[cfg(test)]
fn emit(_path: &Path, line: String) {
    CAPTURED.with(|captured| captured.borrow_mut().push(line));
}

/// Every entry this test's thread has recorded, oldest first.
#[cfg(test)]
pub(crate) fn captured() -> Vec<String> {
    CAPTURED.with(|captured| captured.borrow().clone())
}

/// Drop this thread's captured entries — call at the top of a test that
/// asserts on [`captured`], since one test may drive several recording paths.
#[cfg(test)]
pub(crate) fn clear_captured() {
    CAPTURED.with(|captured| captured.borrow_mut().clear());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seam's whole promise (see the module docs). Asserted against the
    /// seam's own SOURCE rather than by capturing the process's file
    /// descriptors, because what has to hold is stronger than "this call did
    /// not print": no path through this module may reach the terminal,
    /// including ones no test exercises. The scan covers the seam only — it
    /// stops at this test module — and skips comment lines so the docs above
    /// can name the macros they forbid.
    #[test]
    fn the_seam_never_touches_the_terminal() {
        let source = include_str!("diagnostics.rs");
        let (seam, _) = source
            .split_once("#[cfg(test)]\nmod tests {")
            .expect("the seam's source ends where its test module begins");
        let code = seam
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        for forbidden in [
            "println!",
            "eprintln!",
            "print!",
            "eprint!",
            "io::stdout",
            "io::stderr",
        ] {
            assert!(
                !code.contains(forbidden),
                "the diagnostic seam must never write to the terminal, but its \
                 source contains `{forbidden}`; the TUI owns the alternate \
                 screen and a print there corrupts the frame"
            );
        }
    }

    #[test]
    fn an_event_is_one_timestamped_line() {
        let entry = format_entry("2026-07-22T09:58:00-07:00", "degraded read: probes(Q-23)");

        assert_eq!(
            entry,
            "2026-07-22T09:58:00-07:00  degraded read: probes(Q-23)"
        );
    }

    #[test]
    fn a_multiline_event_still_occupies_one_line() {
        // Dolt's diagnostics are frequently multi-line; one event must stay one
        // grep hit.
        let entry = format_entry("2026-07-22T09:58:00-07:00", "dolt sql failed:\nline two");

        assert!(!entry.trim_end().contains('\n'), "{entry}");
        assert!(entry.contains("dolt sql failed: line two"), "{entry}");
    }

    #[test]
    fn degraded_read_names_the_operation_the_subject_and_the_cause() {
        clear_captured();

        degraded_read(
            "probes",
            "Q-23",
            &QuizdomError::Dolt("dolt sql failed: connection refused".to_string()),
        );

        let captured = captured();
        assert_eq!(captured.len(), 1, "{captured:?}");
        let entry = &captured[0];
        assert!(entry.contains("probes(Q-23)"), "{entry}");
        assert!(entry.contains("connection refused"), "{entry}");
        // The symptom the user sees is an empty list, so the log has to say
        // that emptiness was a failure rather than an answer.
        assert!(entry.contains("returned nothing"), "{entry}");
    }

    /// A scratch directory unique to the call site, cleaned up by the caller.
    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "quizdom-diagnostics-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn appending_creates_the_file_and_its_parents_and_never_truncates() {
        let dir = temp_dir("append");
        let path = dir.join("nested").join("quizdom.log");

        append_entry(&path, "first").unwrap();
        append_entry(&path, "second").unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, "first\nsecond\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-321 | ai:claude
    /// The bound. A persistently broken store writes a line per probed read per
    /// turn; without this the breadcrumb file grows without limit in exactly the
    /// case the user is least likely to be watching.
    #[test]
    fn an_oversized_log_rotates_instead_of_growing() {
        let dir = temp_dir("rotate");
        let path = dir.join("quizdom.log");
        std::fs::create_dir_all(&dir).unwrap();
        // One byte over the limit is enough; the history is the previous run.
        let history = "old entry\n".repeat((LOG_SIZE_LIMIT as usize / 10) + 1);
        std::fs::write(&path, &history).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() >= LOG_SIZE_LIMIT);

        append_entry(&path, "the entry that tipped it over").unwrap();

        // The live log is SMALL again — the property that bounds the disk.
        let live = std::fs::read_to_string(&path).unwrap();
        assert!(
            (live.len() as u64) < LOG_SIZE_LIMIT,
            "the live log restarted, {} bytes",
            live.len()
        );
        assert!(live.contains("the entry that tipped it over"), "{live}");
        // …and it says it is a fresh generation, so a reader who tails it does
        // not read a truncated story as the whole story.
        assert!(live.starts_with("-- rotated at "), "{live}");

        // The history is kept beside it rather than discarded: the run of
        // entries that EXPLAINS the breakage is the reason the file exists.
        let previous = std::fs::read_to_string(dir.join("quizdom.log.1")).unwrap();
        assert_eq!(previous, history);

        // A second rotation replaces the one kept generation — bounded at two
        // files, not a growing pile.
        std::fs::write(&path, &history).unwrap();
        append_entry(&path, "and again").unwrap();
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("and again"));
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            2,
            "the live log and exactly one previous generation"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-321 | ai:claude — and a log under the bound is untouched: the
    // common case must not pay for the pathological one.
    #[test]
    fn a_small_log_is_never_rotated() {
        let dir = temp_dir("no-rotate");
        let path = dir.join("quizdom.log");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "a short trail\n").unwrap();

        append_entry(&path, "another line").unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "a short trail\nanother line\n"
        );
        assert!(!dir.join("quizdom.log.1").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-333 | ai:claude
    /// The mechanism, pinned deterministically: a rotation does not swap the
    /// file out, it empties it. A `rename` would satisfy every assertion in
    /// `an_oversized_log_rotates_instead_of_growing` and still fail here — the
    /// handle opened before the rotation would be writing into `quizdom.log.1`,
    /// which is what makes a rename unsafe for a log several processes share.
    #[test]
    fn a_rotation_keeps_the_live_log_in_place_so_open_handles_follow_it() {
        const LIMIT: u64 = 2 * 1024;
        let dir = temp_dir("rotate-in-place");
        let path = dir.join("quizdom.log");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "old entry\n".repeat(LIMIT as usize / 10 + 1)).unwrap();

        // A second writer that opened the log BEFORE the rotation, the way a
        // long-lived process would.
        let mut held = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();

        append_entry_bounded(&path, "the entry that tipped it over", LIMIT).unwrap();
        writeln!(held, "written through the pre-rotation handle").unwrap();

        let live = std::fs::read_to_string(&path).unwrap();
        assert!(
            live.contains("written through the pre-rotation handle"),
            "the older handle still writes to the LIVE log: {live}"
        );
        assert!(
            !std::fs::read_to_string(dir.join("quizdom.log.1"))
                .unwrap()
                .contains("written through the pre-rotation handle"),
            "…and not into the dead generation"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-333 | ai:claude
    /// The critical section, pinned directly: an append cannot proceed while
    /// another writer holds the log. This is the half of TASK-333 that makes
    /// the size check and the rotation ONE operation — without it two writers
    /// can both decide to rotate the same over-sized log.
    ///
    /// Threads rather than processes: `File::lock` is per open file
    /// DESCRIPTION, so two `open`s contend whether or not they are in the same
    /// process, and the writer here opens the log per append exactly as the
    /// seam does. A slow machine makes this test weaker, never flaky — the only
    /// way it fails is a write that genuinely landed while the log was held.
    #[test]
    fn an_append_waits_for_whoever_holds_the_log() {
        let dir = temp_dir("append-lock");
        let path = dir.join("quizdom.log");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "").unwrap();

        let held = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        held.lock().expect("the fixture holds the log");

        let (started, has_started) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                started.send(()).unwrap();
                append_entry_bounded(&path, "the blocked entry", LOG_SIZE_LIMIT).unwrap();
            });

            has_started.recv().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(200));
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "",
                "an append landed while another writer held the log"
            );

            drop(held);
            writer.join().unwrap();
        });

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "the blocked entry\n",
            "…and it lands as soon as the log is free"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-333 | ai:claude
    /// The loss TASK-333 was filed about, under real contention. Rotation used
    /// to be `stat` then `rename`: two processes both see an over-sized log,
    /// the first renames it to `quizdom.log.1`, the second renames the
    /// near-empty file that replaced it OVER that rotated generation, and the
    /// history explaining the breakage is gone.
    ///
    /// `quizdom.log.1` is only ever written by copying a log that had REACHED
    /// the limit, so at every instant it is either absent or a FULL generation
    /// — a clobbered rotation leaves a nearly empty one instead. Checking that
    /// only at the end would miss it (the next good rotation overwrites the
    /// evidence), so a watcher samples it throughout and keeps the smallest it
    /// ever saw. The staged-then-renamed copy is what makes those samples
    /// trustworthy: a reader can never catch the file half-written.
    #[test]
    fn concurrent_writers_never_lose_a_rotated_generation() {
        const LIMIT: u64 = 4 * 1024;
        const WRITERS: usize = 8;
        const ENTRIES: usize = 120;
        let dir = temp_dir("rotate-concurrent");
        let path = dir.join("quizdom.log");
        let rotated = dir.join("quizdom.log.1");
        std::fs::create_dir_all(&dir).unwrap();
        let entry = "x".repeat(200);
        let done = std::sync::atomic::AtomicBool::new(false);
        let smallest = std::sync::atomic::AtomicU64::new(u64::MAX);

        std::thread::scope(|scope| {
            scope.spawn(|| {
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Ok(seen) = std::fs::metadata(&rotated) {
                        smallest.fetch_min(seen.len(), std::sync::atomic::Ordering::Relaxed);
                    }
                    std::thread::yield_now();
                }
            });
            let writers: Vec<_> = (0..WRITERS)
                .map(|_| {
                    scope.spawn(|| {
                        for _ in 0..ENTRIES {
                            append_entry_bounded(&path, &entry, LIMIT).unwrap();
                        }
                    })
                })
                .collect();
            for writer in writers {
                writer.join().unwrap();
            }
            // Joined here rather than at the end of the scope: the watcher runs
            // until it is told the writing is over.
            done.store(true, std::sync::atomic::Ordering::Relaxed);
        });

        // ~192 KiB through a 4 KiB log: dozens of rotations, so the writers
        // really did contend on rotation rather than all landing in one
        // generation.
        assert!(
            rotated.exists(),
            "the fixture must have rotated, or it proves nothing"
        );
        let smallest = smallest.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            smallest >= LIMIT,
            "a rotated generation was observed at {smallest} bytes, under the \
             {LIMIT}-byte limit — a racing writer clobbered the history it was \
             supposed to keep"
        );
        // …and the live log is still bounded: no writer's rotation was skipped.
        assert!(
            std::fs::metadata(&path).unwrap().len() < LIMIT + entry.len() as u64 + 128,
            "the live log stayed under the bound"
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            2,
            "the live log and exactly one previous generation — the staging \
             copy never outlives its rotation"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The name the parent hands to `--exact`, and the marker the child prints
    /// the alternate screen around. One constant so the two cannot drift.
    const CAPTURE_CHILD: &str = "diagnostics::tests::the_seam_under_a_live_alternate_screen";

    // trace:TASK-322 | ai:claude
    /// The BEHAVIOURAL half of the terminal-safety invariant (the scan above is
    /// the lexical half; see the module docs for why both).
    ///
    /// **Why a child process.** Capturing this process's fd 1 and 2 with `dup2`
    /// works, and is what TASK-322 proposed — but the captured bytes then also
    /// contain libtest's own `test … ok` progress lines from every OTHER test
    /// finishing concurrently, so "nothing arrived" is not assertable. Re-running
    /// the test binary for ONE test with `--test-threads=1 --nocapture` and a
    /// piped stdout gives a capture with exactly one writer. `--nocapture` is
    /// load-bearing: it is what makes a stray `println!` reach the pipe instead
    /// of being swallowed by libtest's own capture — which is precisely the
    /// regression this test exists to catch.
    #[test]
    fn nothing_reaches_the_terminal_while_the_alternate_screen_is_active() {
        let child = std::process::Command::new(
            std::env::current_exe().expect("the test binary re-runs itself"),
        )
        .args([
            "--exact",
            CAPTURE_CHILD,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .output()
        .expect("spawn the capture child");

        assert!(
            child.status.success(),
            "the child must have RUN the seam, or an empty capture proves nothing:\n{}\n{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr),
        );

        // Everything the child emitted while the alternate screen was active.
        // crossterm's own enter/leave sequences bracket the window, so the
        // slice between them is the seam's contribution and nothing else.
        let stdout = String::from_utf8_lossy(&child.stdout).into_owned();
        let mut entered: Vec<u8> = Vec::new();
        let mut left: Vec<u8> = Vec::new();
        crossterm::execute!(entered, crossterm::terminal::EnterAlternateScreen).unwrap();
        crossterm::execute!(left, crossterm::terminal::LeaveAlternateScreen).unwrap();
        let enter = String::from_utf8(entered).unwrap();
        let leave = String::from_utf8(left).unwrap();

        let start = stdout
            .find(&enter)
            .map(|at| at + enter.len())
            .unwrap_or_else(|| panic!("the child never entered the alternate screen:\n{stdout}"));
        let end = stdout[start..]
            .find(&leave)
            .unwrap_or_else(|| panic!("the child never left the alternate screen:\n{stdout}"));

        assert_eq!(
            &stdout[start..start + end],
            "",
            "the seam wrote to stdout while the alternate screen was active"
        );
        // stderr is invisible under the alternate screen at best and
        // display-corrupting at worst, so it is held to the same standard.
        assert_eq!(
            String::from_utf8_lossy(&child.stderr),
            "",
            "the seam wrote to stderr"
        );
        // …and the child really did drive the seam, so an empty window is
        // evidence of silence rather than of nothing having run.
        assert!(
            stdout.contains("drove the seam"),
            "the child did not report driving the seam:\n{stdout}"
        );
    }

    // trace:TASK-322 | ai:claude
    /// The child half of the test above — `#[ignore]`d because it is driven by
    /// its parent, not by a plain `cargo test` run. It enters the alternate
    /// screen for real and exercises every branch of the seam inside it,
    /// including the write that FAILS (the branch most likely to reach for a
    /// print), then reports OUTSIDE the window that it ran.
    #[test]
    #[ignore = "driven as a child process by nothing_reaches_the_terminal_while_the_alternate_screen_is_active"]
    fn the_seam_under_a_live_alternate_screen() {
        use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};

        let dir = temp_dir("alt-screen");
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("quizdom.log");

        crossterm::execute!(std::io::stdout(), EnterAlternateScreen).unwrap();
        record("a degraded read the user must not see on screen");
        degraded_read(
            "probes",
            "Q-23",
            &QuizdomError::Dolt("connection refused".to_string()),
        );
        let wrote = append_entry(&log_path, "a real file write");
        // `quizdom.log` is a FILE, so creating a directory under it cannot work.
        let failed = append_entry(&log_path.join("unwritable"), "a write that fails");
        crossterm::execute!(std::io::stdout(), LeaveAlternateScreen).unwrap();
        std::io::stdout().flush().unwrap();

        // Outside the window: assertions may speak freely here.
        wrote.expect("the real write is the control — the seam does write, silently");
        assert!(
            failed.is_err(),
            "the failing branch must actually have failed, or it proves nothing"
        );
        assert_eq!(captured().len(), 2, "{:?}", captured());
        assert!(std::fs::read_to_string(&log_path)
            .unwrap()
            .contains("a real file write"));
        let _ = std::fs::remove_dir_all(&dir);

        println!("drove the seam");
    }
}
