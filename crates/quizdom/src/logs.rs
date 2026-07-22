// trace:TASK-331 | ai:claude
//! `quizdom logs` — the READER for the diagnostic breadcrumb log.
//!
//! ## Why this exists
//!
//! STORY-299 gave quizdom somewhere to put a diagnostic that must not reach the
//! terminal (`crate::diagnostics`), and STORY-326 started pointing users at it:
//! a degraded store read, a failed auto-backup, a blind backup probe all leave
//! a line there. What none of that shipped was a way to READ it. The path is
//! resolved through a three-tier chain (`QUIZDOM_LOG_PATH` > `log_path` in
//! `settings.toml` > `~/.local/share/quizdom/quizdom.log`), so "just cat it"
//! requires already knowing which tier won — and a log that can only be read by
//! someone who knows where it is will not be read.
//!
//! So this command answers both halves at once: it NAMES the resolved file and
//! prints what is in it. `--tail N` limits the output to the last N entries,
//! which is the shape wanted after a session that just said something went
//! wrong.
//!
//! ## Deliberately not part of the seam
//!
//! The reader lives here rather than in `diagnostics.rs` because that module's
//! whole promise — pinned by a scan of its own source — is that NO path through
//! it reaches the terminal. Printing the log is the one thing that legitimately
//! must, so it belongs on the other side of the seam: `diagnostics` writes and
//! never prints, `logs` prints and never writes.
//!
//! Nothing here is quiet about an absent log. A missing file is the healthy
//! case (quizdom writes there only when something went wrong and the session
//! continued anyway), so it is a plain message and an exit code of 0, not an
//! error.
//!
//! ## Absence is one cause, not the only one (TASK-347)
//!
//! That healthy-case message used to be printed for EVERY unreadable log: the
//! reader discarded the `io::Error` and hardcoded `no such file`, so a
//! permissions problem, a directory in the way, and a file that is not valid
//! UTF-8 all reported a specific cause nobody had verified — and reported it
//! identically to the case where the install is simply fine. The broken case
//! was invisible inside the healthy one.
//!
//! So the two are split by `ErrorKind`. `NotFound` keeps the message and the
//! zero exit; anything else is a FAILURE — the command could not do what it was
//! asked — and exits non-zero naming the cause the OS gave. A `--path` that
//! found nothing also says the path came from the flag and names the log that
//! was resolved, since "no such file" about a typo tells you nothing about
//! which of your two candidate paths was wrong.
//!
//! ## What is printed is printable (TASK-349)
//!
//! `diagnostics::one_line` sanitizes on the way IN, which is where the property
//! belongs. This module applies it again on the way OUT, and the second pass is
//! not belt-and-braces: `--path` reads any file the user names — a rotated
//! generation written by an older quizdom, a copy someone mailed them — and
//! this module is the one place that guarantees what it prints cannot rearrange
//! the terminal it prints to.

use crate::error::{QuizdomError, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Parsed flags for `quizdom logs`.
#[derive(Debug)]
struct LogsConfig {
    /// The log to read — the resolved diagnostic log unless `--path` names
    /// another (the rotated `<log>.1`, or a copy someone sent you).
    path: PathBuf,
    /// `--tail N`: print only the last N entries. `None` prints all of them.
    tail: Option<usize>,
    // trace:TASK-347 | ai:claude
    /// The resolved log, kept even when `--path` overrides it. A mistyped
    /// `--path` reads as "no such file", which is true and useless: the reader
    /// has two candidate paths in their head and has just been told nothing
    /// about which one was wrong. Naming this one alongside answers that.
    resolved: PathBuf,
}

impl LogsConfig {
    /// Parse the argv tail over the resolved default, so an unflagged
    /// `quizdom logs` reads the same file the session would have written to.
    /// Taking the default as a parameter keeps this pure — the tests pin
    /// argument handling without the ambient environment leaking in (the
    /// TASK-228 pattern).
    fn parse(args: impl IntoIterator<Item = String>, default_path: PathBuf) -> Result<Self> {
        let resolved = default_path.clone();
        let mut path = default_path;
        let mut tail = None;
        let mut args = args.into_iter().peekable();

        if args.peek().map(String::as_str) == Some("logs") {
            args.next();
        }

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--tail" => {
                    let raw = next_arg(&mut args, "--tail")?;
                    tail = Some(raw.parse::<usize>().map_err(|_| {
                        QuizdomError::Usage(format!(
                            "--tail takes a whole number of entries, not `{raw}`\n{}",
                            logs_usage()
                        ))
                    })?);
                }
                "--path" => path = PathBuf::from(next_arg(&mut args, "--path")?),
                "--help" | "-h" => return Err(QuizdomError::Usage(logs_usage())),
                other => {
                    return Err(QuizdomError::Usage(format!(
                        "unknown argument: {other}\n{}",
                        logs_usage()
                    )))
                }
            }
        }

        Ok(Self {
            path,
            tail,
            resolved,
        })
    }
}

fn logs_usage() -> String {
    "usage: quizdom logs [--tail N] [--path <file>]\n\
     \n\
     Prints the diagnostic log — what quizdom recorded when something went\n\
     wrong but the session carried on. The file defaults to the resolved\n\
     log path (QUIZDOM_LOG_PATH > log_path in settings.toml > the default),\n\
     which `/settings` also shows."
        .to_string()
}

fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| QuizdomError::Usage(format!("{name} requires a value")))
}

/// Entry point for `quizdom logs`.
pub fn run_logs(args: impl IntoIterator<Item = String>, output: &mut impl Write) -> Result<()> {
    let config = LogsConfig::parse(args, crate::settings::resolve_log_path())?;
    render_logs(&config, output)
}

/// The rendering, split from the argument parsing so both halves are testable
/// without an ambient log file.
fn render_logs(config: &LogsConfig, output: &mut impl Write) -> Result<()> {
    let path = &config.path;
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        // trace:TASK-347 | ai:claude — absence is the healthy install: news,
        // not a failure, so it stays a message and a zero exit.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            writeln!(output, "{}", nothing_to_show(path, "no such file"))?;
            if path != &config.resolved {
                writeln!(
                    output,
                    "That path came from --path; the resolved log is {}.",
                    printable_path(&config.resolved)
                )?;
            }
            return Ok(());
        }
        // trace:TASK-347 | ai:claude — every OTHER cause is a genuine failure
        // to do what was asked, and must not masquerade as an empty install.
        Err(error) => return Err(unreadable(path, &error)),
    };
    let entries: Vec<&str> = contents.lines().collect();
    if entries.is_empty() {
        writeln!(output, "{}", nothing_to_show(path, "the file is empty"))?;
        return Ok(());
    }

    let shown = match config.tail {
        Some(tail) => &entries[entries.len().saturating_sub(tail)..],
        None => &entries[..],
    };
    writeln!(output, "{}", header(path, entries.len(), shown.len()))?;
    for entry in shown {
        // trace:TASK-349 | ai:claude — the file is sanitized on the way in, but
        // `--path` reads files this crate never wrote.
        writeln!(output, "{}", crate::diagnostics::one_line(entry))?;
    }
    if let Some(previous) = previous_generation(path) {
        // The live log says "-- rotated at N bytes --" on its first line when a
        // rotation happened, but a `--tail` never reaches that line, so name
        // the older generation here too.
        writeln!(
            output,
            "\n(an earlier generation is in {})",
            printable_path(&previous)
        )?;
    }
    Ok(())
}

/// The one-line answer when there is nothing to print: it still NAMES the
/// resolved file, because "where does quizdom log?" is half of what someone
/// running this command wants to know, and an empty log does not answer it.
fn nothing_to_show(path: &Path, reason: &str) -> String {
    format!(
        "No diagnostics recorded — {} ({reason}). quizdom writes there only when \
         something goes wrong that does not stop the session.",
        printable_path(path)
    )
}

// trace:TASK-347 | ai:claude
/// The error for a log that is THERE and could not be read — a permissions
/// problem, a directory in the way, a file that is not valid UTF-8.
///
/// This is the case the old hardcoded `no such file` swallowed. It carries the
/// `ErrorKind` through rather than flattening to a string, so the cause the OS
/// gave survives, and it exits non-zero: the user asked to read a log and
/// quizdom did not read it.
fn unreadable(path: &Path, error: &std::io::Error) -> QuizdomError {
    QuizdomError::Io(std::io::Error::new(
        error.kind(),
        format!(
            "could not read the diagnostic log at {}: {}",
            printable_path(path),
            crate::diagnostics::one_line(&error.to_string())
        ),
    ))
}

// trace:TASK-349 | ai:claude
/// A path as one line of printable text. Paths reach this module from
/// `settings.toml`, `$QUIZDOM_LOG_PATH` and `--path`, none of which is
/// obliged to hold characters a terminal will merely display.
fn printable_path(path: &Path) -> String {
    crate::diagnostics::one_line(&path.display().to_string())
}

/// `<path> — N entries`, saying so when `--tail` is showing fewer than all.
fn header(path: &Path, total: usize, shown: usize) -> String {
    let count = if shown < total {
        format!("last {shown} of {total} entries")
    } else {
        format!("{total} entries")
    };
    format!("{} — {count}\n", printable_path(path))
}

// trace:TASK-333 | ai:claude — the rotated generation the bounded log keeps.
// trace:TASK-348 | ai:claude — named by the module that WRITES it.
/// `<path><ROTATED_SUFFIX>` when it exists, so a reader learns there is older
/// history rather than reading a rotated log as the whole story.
fn previous_generation(path: &Path) -> Option<PathBuf> {
    let suffix = crate::diagnostics::ROTATED_SUFFIX;
    let previous = path.with_file_name(format!("{}{suffix}", path.file_name()?.to_string_lossy()));
    previous.exists().then_some(previous)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory unique to the call site, cleaned up by the caller.
    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("quizdom-logs-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn render(config: &LogsConfig) -> String {
        let mut out: Vec<u8> = Vec::new();
        render_logs(config, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// A config reading `path` as the RESOLVED log — the unflagged
    /// `quizdom logs`, which is what most of these tests are about.
    fn resolved(path: &Path, tail: Option<usize>) -> LogsConfig {
        LogsConfig {
            path: path.to_path_buf(),
            tail,
            resolved: path.to_path_buf(),
        }
    }

    fn args(tail: &[&str]) -> Vec<String> {
        std::iter::once("logs".to_string())
            .chain(tail.iter().map(|arg| arg.to_string()))
            .collect()
    }

    // trace:TASK-331 | ai:claude
    #[test]
    fn the_log_prints_with_the_resolved_path_named_above_it() {
        let dir = temp_dir("print");
        let path = dir.join("quizdom.log");
        std::fs::write(&path, "first entry\nsecond entry\nthird entry\n").unwrap();

        let printed = render(&resolved(&path, None));

        // The path is half the answer: the reader does not have to know which
        // tier of the resolution chain won.
        assert!(printed.contains(&path.display().to_string()), "{printed}");
        assert!(printed.contains("3 entries"), "{printed}");
        assert!(printed.contains("first entry"), "{printed}");
        assert!(printed.contains("third entry"), "{printed}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-331 | ai:claude
    #[test]
    fn tail_limits_the_output_to_the_last_entries_and_says_it_did() {
        let dir = temp_dir("tail");
        let path = dir.join("quizdom.log");
        let entries: String = (1..=10).map(|n| format!("entry {n}\n")).collect();
        std::fs::write(&path, entries).unwrap();

        let printed = render(&resolved(&path, Some(3)));

        assert!(printed.contains("last 3 of 10 entries"), "{printed}");
        assert!(printed.contains("entry 8"), "{printed}");
        assert!(printed.contains("entry 10"), "{printed}");
        assert!(
            !printed.contains("entry 7"),
            "the older entries are held back: {printed}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-331 | ai:claude — a `--tail` larger than the log is not an
    // error and not a truncation; it is just the whole log.
    #[test]
    fn a_tail_longer_than_the_log_prints_all_of_it() {
        let dir = temp_dir("tail-long");
        let path = dir.join("quizdom.log");
        std::fs::write(&path, "only entry\n").unwrap();

        let printed = render(&resolved(&path, Some(500)));

        assert!(printed.contains("1 entries"), "{printed}");
        assert!(printed.contains("only entry"), "{printed}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-331 | ai:claude
    /// The healthy install has NO log — the seam only fires on a degraded read
    /// or a failed backup — so the common case must read as good news with the
    /// path named, not as a failure.
    #[test]
    fn a_missing_log_is_a_clear_message_rather_than_an_error() {
        let dir = temp_dir("missing");
        let path = dir.join("never-written.log");

        let printed = render(&resolved(&path, None));

        assert!(printed.contains("No diagnostics recorded"), "{printed}");
        assert!(
            printed.contains(&path.display().to_string()),
            "it still says WHERE quizdom would have written: {printed}"
        );

        // An empty log reads the same way, for the same reason.
        std::fs::write(&path, "").unwrap();
        let empty = render(&resolved(&path, None));
        assert!(empty.contains("No diagnostics recorded"), "{empty}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-331 | ai:claude
    /// The bounded log keeps one previous generation (TASK-321/TASK-333). A
    /// `--tail` never reaches the live log's `-- rotated at … --` first line,
    /// so the reader would otherwise take a rotated log for the whole story.
    #[test]
    fn a_rotated_generation_beside_the_log_is_pointed_at() {
        let dir = temp_dir("rotated");
        let path = dir.join("quizdom.log");
        std::fs::write(&path, "the fresh generation\n").unwrap();
        std::fs::write(dir.join("quizdom.log.1"), "the older generation\n").unwrap();

        let printed = render(&resolved(&path, Some(1)));

        assert!(printed.contains("quizdom.log.1"), "{printed}");
        assert!(
            !printed.contains("the older generation"),
            "it POINTS at the older generation without inlining it: {printed}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-347 | ai:claude
    /// The failure the hardcoded `no such file` swallowed. A log that is THERE
    /// and cannot be read is not the healthy install, and reporting it as one
    /// makes the broken case invisible inside the good one.
    #[test]
    fn an_unreadable_log_reports_its_actual_cause_rather_than_absence() {
        let dir = temp_dir("unreadable");

        // A directory where the log should be — the "something is in the way"
        // case. It EXISTS, so `no such file` would be a plain falsehood.
        let in_the_way = dir.join("quizdom.log");
        std::fs::create_dir_all(&in_the_way).unwrap();
        let mut out: Vec<u8> = Vec::new();
        let error = render_logs(&resolved(&in_the_way, None), &mut out)
            .expect_err("a directory is not a readable log");
        assert!(matches!(error, QuizdomError::Io(_)), "{error:?}");
        let message = error.to_string();
        assert!(
            !message.contains("No diagnostics recorded"),
            "an unreadable log is not an empty one: {message}"
        );
        assert!(message.contains("could not read"), "{message}");
        assert!(
            message.contains(&in_the_way.display().to_string()),
            "it names WHICH file it could not read: {message}"
        );
        assert!(
            out.is_empty(),
            "nothing is printed as if it were the log's contents: {out:?}"
        );

        // A file that is not valid UTF-8: `read_to_string` fails at the READ,
        // not the open, so the old `let Ok(..) else` reported it as absence too.
        let garbled = dir.join("garbled.log");
        std::fs::write(&garbled, [0xff, 0xfe, 0x00]).unwrap();
        let error = render_logs(&resolved(&garbled, None), &mut Vec::new())
            .expect_err("invalid UTF-8 is not a readable log");
        assert!(error.to_string().contains("could not read"), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-347 | ai:claude
    /// The other half: splitting the causes must not turn the COMMON case — a
    /// healthy install with nothing to report — into an error.
    #[test]
    fn genuine_absence_is_still_absence_and_still_exits_zero() {
        let dir = temp_dir("still-absent");
        let path = dir.join("never-written.log");

        let printed = render(&resolved(&path, None));

        assert!(printed.contains("No diagnostics recorded"), "{printed}");
        assert!(printed.contains("no such file"), "{printed}");
        assert!(
            !printed.contains("could not read"),
            "absence is not a read failure: {printed}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-347 | ai:claude
    /// A mistyped `--path` is the third input that reaches "no such file", and
    /// there the message is true and useless: the user is holding two candidate
    /// paths and has just been told nothing about which one was wrong.
    #[test]
    fn a_path_that_found_nothing_names_the_resolved_log_too() {
        let dir = temp_dir("mistyped");
        let mistyped = dir.join("quizdom.lgo");
        let real = dir.join("quizdom.log");

        let printed = render(&LogsConfig {
            path: mistyped.clone(),
            tail: None,
            resolved: real.clone(),
        });

        assert!(
            printed.contains(&mistyped.display().to_string()),
            "{printed}"
        );
        assert!(
            printed.contains("--path"),
            "it says the path came from the flag: {printed}"
        );
        assert!(
            printed.contains(&real.display().to_string()),
            "…and names the log it would otherwise have read: {printed}"
        );

        // The unflagged form has only one candidate, so it says none of this.
        let plain = render(&resolved(&real, None));
        assert!(!plain.contains("--path"), "{plain}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-349 | ai:claude
    /// `diagnostics::one_line` keeps escapes out of the file, but `--path`
    /// reads whatever it is pointed at — a rotated generation from an older
    /// quizdom, a copy someone mailed over. This module is the one place that
    /// can promise what reaches the terminal cannot rearrange it.
    #[test]
    fn a_recorded_entry_cannot_rearrange_the_terminal_it_prints_to() {
        let dir = temp_dir("escapes");
        let path = dir.join("quizdom.log");
        std::fs::write(
            &path,
            "2026-07-22T09:58:00-07:00  push failed: \u{1b}[31mfatal\u{1b}[0m\n\
             2026-07-22T09:59:00-07:00  degraded read\rTHE ENTRY ABOVE IS HIDDEN\n\
             2026-07-22T10:00:00-07:00  \u{1b}]0;window title\u{7}last\n",
        )
        .unwrap();

        let printed = render(&resolved(&path, None));

        assert!(
            !printed.contains('\u{1b}'),
            "no escape reaches stdout: {printed:?}"
        );
        assert!(
            !printed.contains('\r'),
            "no cursor return either: {printed:?}"
        );
        // The TEXT survives — sanitizing must not silently eat the diagnostic,
        // which is the whole reason the file exists.
        assert!(printed.contains("push failed: fatal"), "{printed}");
        assert!(
            printed.contains("degraded read THE ENTRY ABOVE IS HIDDEN"),
            "{printed}"
        );
        assert!(printed.contains("last"), "{printed}");
        // …and each entry still occupies exactly one line.
        assert_eq!(
            printed
                .lines()
                .filter(|line| line.contains("2026-07-22"))
                .count(),
            3,
            "{printed}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-348 | ai:claude
    /// The reader points at the rotated generation using the constant the
    /// WRITER rotates onto. Two spellings agree until the suffix changes, and
    /// then this line stops printing — no error, a pointer that quietly
    /// disappears, in exactly the case (`--tail` never reaching the live log's
    /// `-- rotated at … --` first line) where its absence is undetectable.
    #[test]
    fn the_rotated_pointer_uses_the_suffix_the_writer_rotates_onto() {
        let dir = temp_dir("shared-suffix");
        let path = dir.join("quizdom.log");
        std::fs::write(&path, "the fresh generation\n").unwrap();
        let rotated = dir.join(format!("quizdom.log{}", crate::diagnostics::ROTATED_SUFFIX));
        std::fs::write(&rotated, "the older generation\n").unwrap();

        let printed = render(&resolved(&path, Some(1)));

        assert!(
            printed.contains(&rotated.display().to_string()),
            "the reader found what the writer would have written: {printed}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-331 | ai:claude
    #[test]
    fn the_default_path_is_the_resolved_log_and_a_flag_overrides_it() {
        let resolved =
            LogsConfig::parse(args(&[]), PathBuf::from("/resolved/quizdom.log")).unwrap();
        assert_eq!(resolved.path, PathBuf::from("/resolved/quizdom.log"));
        assert_eq!(resolved.tail, None);

        let overridden = LogsConfig::parse(
            args(&["--path", "/elsewhere/copy.log", "--tail", "20"]),
            PathBuf::from("/resolved/quizdom.log"),
        )
        .unwrap();
        assert_eq!(overridden.path, PathBuf::from("/elsewhere/copy.log"));
        assert_eq!(overridden.tail, Some(20));
        // trace:TASK-347 | ai:claude — the overridden path does not erase the
        // resolved one; a `--path` that finds nothing has to name both.
        assert_eq!(overridden.resolved, PathBuf::from("/resolved/quizdom.log"));
        assert_eq!(resolved.resolved, resolved.path);
    }

    // trace:TASK-331 | ai:claude — a mistyped count must not silently read as
    // "print everything"; that is the shape where a user thinks they saw the
    // whole log and did not.
    #[test]
    fn a_non_numeric_tail_is_a_usage_error_naming_the_flag() {
        let error = LogsConfig::parse(args(&["--tail", "lots"]), PathBuf::from("/resolved.log"))
            .expect_err("`lots` is not a count");

        assert!(matches!(error, QuizdomError::Usage(_)), "{error:?}");
        assert!(error.to_string().contains("--tail"), "{error}");
        assert!(error.to_string().contains("lots"), "{error}");
    }

    #[test]
    fn an_unknown_flag_is_a_usage_error_carrying_the_usage_text() {
        let error = LogsConfig::parse(args(&["--follow"]), PathBuf::from("/resolved.log"))
            .expect_err("`--follow` is not a flag this command has");

        assert!(error.to_string().contains("--follow"), "{error}");
        assert!(error.to_string().contains("usage: quizdom logs"), "{error}");
    }
}
