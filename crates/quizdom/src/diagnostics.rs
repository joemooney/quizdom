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
//! ## The invariant
//!
//! **Nothing here ever writes to stdout or stderr.** [`record`] takes no writer
//! and returns nothing, so no caller can aim it at the terminal even by
//! accident, and a failed write is DROPPED rather than reported — a breadcrumb
//! that takes down the session it was meant to explain is worse than no
//! breadcrumb at all. `the_seam_never_touches_the_terminal` pins the invariant
//! against the seam's own source, so a future edit that reaches for a print
//! macro fails the suite rather than corrupting a frame in the field.
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

/// Append one line to `path`, creating the file and any missing parents.
///
/// Append mode, never truncate: the log is a trail across sessions, and two
/// quizdom processes writing at once must not clobber each other.
fn append_entry(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")
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

    #[test]
    fn appending_creates_the_file_and_its_parents_and_never_truncates() {
        let dir = std::env::temp_dir().join(format!(
            "quizdom-diagnostics-{}-{}",
            std::process::id(),
            line!()
        ));
        let path = dir.join("nested").join("quizdom.log");

        append_entry(&path, "first").unwrap();
        append_entry(&path, "second").unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, "first\nsecond\n");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
