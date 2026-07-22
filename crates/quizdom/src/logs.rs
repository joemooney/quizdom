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
}

impl LogsConfig {
    /// Parse the argv tail over the resolved default, so an unflagged
    /// `quizdom logs` reads the same file the session would have written to.
    /// Taking the default as a parameter keeps this pure — the tests pin
    /// argument handling without the ambient environment leaking in (the
    /// TASK-228 pattern).
    fn parse(args: impl IntoIterator<Item = String>, default_path: PathBuf) -> Result<Self> {
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

        Ok(Self { path, tail })
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
    let Ok(contents) = std::fs::read_to_string(path) else {
        // An unreadable log is the same NEWS as an absent one — there is
        // nothing to show — and the healthy install genuinely has no file, so
        // this is a message rather than an error.
        writeln!(output, "{}", nothing_to_show(path, "no such file"))?;
        return Ok(());
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
        writeln!(output, "{entry}")?;
    }
    if let Some(previous) = previous_generation(path) {
        // The live log says "-- rotated at N bytes --" on its first line when a
        // rotation happened, but a `--tail` never reaches that line, so name
        // the older generation here too.
        writeln!(
            output,
            "\n(an earlier generation is in {})",
            previous.display()
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
        path.display()
    )
}

/// `<path> — N entries`, saying so when `--tail` is showing fewer than all.
fn header(path: &Path, total: usize, shown: usize) -> String {
    let count = if shown < total {
        format!("last {shown} of {total} entries")
    } else {
        format!("{total} entries")
    };
    format!("{} — {count}\n", path.display())
}

// trace:TASK-333 | ai:claude — the rotated generation the bounded log keeps.
/// `<path>.1` when it exists, so a reader learns there is older history rather
/// than reading a rotated log as the whole story.
fn previous_generation(path: &Path) -> Option<PathBuf> {
    let previous = path.with_file_name(format!("{}.1", path.file_name()?.to_string_lossy()));
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

        let printed = render(&LogsConfig {
            path: path.clone(),
            tail: None,
        });

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

        let printed = render(&LogsConfig {
            path: path.clone(),
            tail: Some(3),
        });

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

        let printed = render(&LogsConfig {
            path: path.clone(),
            tail: Some(500),
        });

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

        let printed = render(&LogsConfig {
            path: path.clone(),
            tail: None,
        });

        assert!(printed.contains("No diagnostics recorded"), "{printed}");
        assert!(
            printed.contains(&path.display().to_string()),
            "it still says WHERE quizdom would have written: {printed}"
        );

        // An empty log reads the same way, for the same reason.
        std::fs::write(&path, "").unwrap();
        let empty = render(&LogsConfig { path, tail: None });
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

        let printed = render(&LogsConfig {
            path: path.clone(),
            tail: Some(1),
        });

        assert!(printed.contains("quizdom.log.1"), "{printed}");
        assert!(
            !printed.contains("the older generation"),
            "it POINTS at the older generation without inlining it: {printed}"
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
