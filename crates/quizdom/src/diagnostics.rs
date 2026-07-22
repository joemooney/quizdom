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
//! file is renamed to `<log>.1` and a fresh one opens with a line saying so.
//! One generation, one `rename`, no rotation state to be wrong.
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

/// Append one line to `path`, creating the file and any missing parents, and
/// ROTATING first when the file has reached [`LOG_SIZE_LIMIT`].
///
/// Append mode, never truncate in place: the log is a trail across sessions,
/// and two quizdom processes writing at once must not clobber each other.
///
/// Rotation is a single `rename` to `<path>.1` — one generation, no state file,
/// no counters, nothing that can be wrong beyond "the previous log is beside
/// this one". Truncating instead would have been marginally simpler, but it
/// discards the run of entries that EXPLAINS the breakage that filled the file,
/// which is the only reason the file exists.
fn append_entry(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let rotated = rotate_if_full(path);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    // The fresh generation opens by saying it is one, so a reader who tails the
    // log does not silently read a truncated story as the whole story.
    if let Some(note) = rotated {
        writeln!(file, "{note}")?;
    }
    writeln!(file, "{line}")
}

// trace:TASK-321 | ai:claude
/// Move an over-sized log aside, returning the note to open the new one with.
///
/// Failure is silence, for the same reason a failed write is dropped: a
/// breadcrumb that takes down the session it was meant to explain is worse than
/// no breadcrumb. A rotation that cannot happen just leaves the log growing —
/// the state we were already in.
fn rotate_if_full(path: &Path) -> Option<String> {
    let size = std::fs::metadata(path).ok()?.len();
    if size < LOG_SIZE_LIMIT {
        return None;
    }
    let previous = path.with_file_name(format!(
        "{}{ROTATED_SUFFIX}",
        path.file_name()?.to_string_lossy()
    ));
    std::fs::rename(path, &previous).ok()?;
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
