//! Pretty-print a full session transcript from its JSONL log (STORY-77).
//!
//! `quizdom session show <id>` reads one recorded exploration and renders its
//! whole path: every question with the user's answer, surfaced term
//! definitions, detected/resolved contradictions, and branch markers. It
//! complements `session list`, which only surfaces the last answered question.
//!
//! Like `curate` (STORY-72) and `contradictions`, this is a read-only,
//! after-the-fact pass over the log — it never touches the session loop or
//! the input handling in `input.rs`.

use crate::error::{QuizdomError, Result};
use crate::session::normalize_session_id;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;

/// Default quizdom user whose session logs `quizdom session show` reads when no
/// `--user` is given — matching the session loop's own default.
const DEFAULT_USER: &str = "local-user";

/// Parsed flags for the `quizdom session show <id>` command.
///
/// `<id>` (or `--session`) names the session to render; `--log` points at one
/// explicit file instead; `--user` selects whose `data/users/<user>/sessions`
/// directory the id resolves against; `--branch` filters to a single session
/// branch (default: render every branch, with fork markers inline).
#[derive(Debug)]
struct ShowConfig {
    user_id: String,
    session_id: Option<String>,
    log_path: Option<PathBuf>,
    branch: Option<String>,
}

impl ShowConfig {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut user_id = DEFAULT_USER.to_string();
        let mut session_id = None;
        let mut log_path = None;
        let mut branch = None;
        let mut args = args.into_iter().peekable();

        // Strip the `session show` command prefix the dispatcher passes through.
        if matches!(args.peek().map(String::as_str), Some("session")) {
            args.next();
        }
        if matches!(args.peek().map(String::as_str), Some("show")) {
            args.next();
        }

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--user" => user_id = next_arg(&mut args, "--user")?,
                "--session" => {
                    session_id = Some(normalize_session_id(&next_arg(&mut args, "--session")?))
                }
                "--log" => log_path = Some(PathBuf::from(next_arg(&mut args, "--log")?)),
                "--branch" => branch = Some(next_arg(&mut args, "--branch")?),
                "--help" | "-h" => return Err(QuizdomError::Usage(show_usage())),
                other if !other.starts_with('-') => {
                    session_id = Some(normalize_session_id(other));
                }
                other => {
                    return Err(QuizdomError::Usage(format!(
                        "unknown argument: {other}\n{}",
                        show_usage()
                    )))
                }
            }
        }

        Ok(Self {
            user_id,
            session_id,
            log_path,
            branch,
        })
    }

    /// The single log file to render: an explicit `--log`, otherwise the
    /// `<id>` resolved against the user's sessions directory. Mirrors the
    /// on-disk layout `curate` and `contradictions` read.
    fn log_path(&self) -> Result<PathBuf> {
        if let Some(log_path) = &self.log_path {
            return Ok(log_path.clone());
        }
        let session_id = self.session_id.as_ref().ok_or_else(|| {
            QuizdomError::Usage(format!(
                "session show requires a session id\n{}",
                show_usage()
            ))
        })?;
        Ok(PathBuf::from("data")
            .join("users")
            .join(&self.user_id)
            .join("sessions")
            .join(format!("{session_id}.jsonl")))
    }
}

fn show_usage() -> String {
    "usage: quizdom session show <session-id> [--user local-user] [--branch main] [--log path]"
        .to_string()
}

fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| QuizdomError::Usage(format!("{name} requires a value")))
}

fn json_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// The branch a logged event belongs to, defaulting to `"main"` when the event
/// carries no `branch_id` (matching the session loop's own default).
fn event_branch(value: &Value) -> &str {
    json_str(value, "branch_id").unwrap_or("main")
}

/// Render one logged event as transcript line(s). Returns `true` when the event
/// produced output, so the caller can detect an empty (or fully filtered)
/// transcript. Unknown event types render nothing and return `false`.
fn render_event(value: &Value, output: &mut dyn Write) -> Result<bool> {
    match json_str(value, "event_type") {
        Some("session_started") => {
            let branch = event_branch(value);
            let strategy = json_str(value, "strategy").unwrap_or("deterministic");
            match json_str(value, "llm_backend") {
                Some(backend) => writeln!(
                    output,
                    "── branch {branch} · strategy {strategy} · backend {backend} ──"
                )?,
                None => writeln!(output, "── branch {branch} · strategy {strategy} ──")?,
            }
        }
        Some("question_presented") => {
            let turn = value.get("turn").and_then(Value::as_u64).unwrap_or(0);
            let question_ref = json_str(value, "question_ref").unwrap_or("");
            let text = json_str(value, "question_text").unwrap_or("");
            writeln!(output, "turn {turn} · {question_ref}")?;
            writeln!(output, "  Q: {text}")?;
        }
        Some("answer_recorded") => {
            let raw = json_str(value, "raw_answer").unwrap_or("");
            let normalized = json_str(value, "normalized_answer").unwrap_or("");
            // Show the normalized form only when it adds something over the raw
            // text (e.g. a free-text answer normalized to "punt").
            let answer = if raw.is_empty() {
                normalized.to_string()
            } else if normalized.is_empty() || normalized == raw {
                raw.to_string()
            } else {
                format!("{raw} [{normalized}]")
            };
            writeln!(output, "  A: {answer}")?;
        }
        Some("term_interpreted") => {
            let term = json_str(value, "term").unwrap_or("");
            let definition = json_str(value, "adopted_definition")
                .or_else(|| json_str(value, "raw_definition"))
                .unwrap_or("");
            writeln!(output, "  ✎ defined \"{term}\": {definition}")?;
        }
        Some("contradiction_resolved") => {
            let left = json_str(value, "left_belief").unwrap_or("");
            let right = json_str(value, "right_belief").unwrap_or("");
            let kept = match json_str(value, "kept_side") {
                Some(side) => format!(" → kept {side}"),
                None => String::new(),
            };
            writeln!(output, "  ⚠ contradiction: \"{left}\" vs \"{right}\"{kept}")?;
        }
        Some("next_question_selected") => {
            let next = json_str(value, "selected_next_question_ref").unwrap_or("");
            match json_str(value, "selection_reason") {
                Some(reason) if !reason.is_empty() => {
                    writeln!(output, "  ↳ next: {next} ({reason})")?
                }
                _ => writeln!(output, "  ↳ next: {next}")?,
            }
        }
        Some("branch_forked") => {
            let proposition = json_str(value, "proposition").unwrap_or("");
            let mut seeds = Vec::new();
            if let Some(branches) = value.get("branches").and_then(Value::as_array) {
                for branch in branches {
                    let stance = json_str(branch, "stance")
                        .or_else(|| json_str(branch, "branch_id"))
                        .unwrap_or("?");
                    let seed = json_str(branch, "seed_question_ref").unwrap_or("?");
                    seeds.push(format!("{stance} → {seed}"));
                }
            }
            writeln!(output, "⑂ forked \"{proposition}\": {}", seeds.join(" · "))?;
        }
        Some("path_truncated") => {
            let from = value.get("from_turn").and_then(Value::as_u64).unwrap_or(0);
            let reason = json_str(value, "reason").unwrap_or("");
            writeln!(output, "✂ truncated from turn {from} ({reason})")?;
        }
        Some("session_ended") => {
            let summary = json_str(value, "summary").unwrap_or("");
            writeln!(output, "── ended · {summary} ──")?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Render a session log as a human-readable transcript.
///
/// `branch` filters to a single session branch (matching `branch_id`, default
/// `"main"`); pass `None` to render every branch. `branch_forked` markers carry
/// no `branch_id`, so they survive the filter — a forked exploration still
/// shows where it split even when narrowed to one side.
// trace:STORY-77 | ai:claude
pub fn render_transcript(
    reader: impl Read,
    branch: Option<&str>,
    output: &mut dyn Write,
) -> Result<()> {
    let reader = BufReader::new(reader);
    let mut events = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value =
            serde_json::from_str(&line).map_err(|error| QuizdomError::Parse(error.to_string()))?;
        events.push(value);
    }

    // The header identifies the session from whatever event first carries the
    // id/user, so the transcript stands on its own in scrollback.
    let session_id = events
        .iter()
        .find_map(|value| json_str(value, "session_id"))
        .unwrap_or("(unknown)");
    let user_id = events
        .iter()
        .find_map(|value| json_str(value, "user_id"))
        .unwrap_or("(unknown)");
    writeln!(output, "Transcript for {session_id} · user {user_id}")?;

    let mut rendered_body = false;
    for value in &events {
        if let Some(branch) = branch {
            let is_fork = json_str(value, "event_type") == Some("branch_forked");
            if !is_fork && event_branch(value) != branch {
                continue;
            }
        }
        if render_event(value, output)? {
            rendered_body = true;
        }
    }
    if !rendered_body {
        writeln!(output, "(no transcript events)")?;
    }
    Ok(())
}

/// Entry point for the standalone `quizdom session show <id>` command. Resolves
/// the session's log file, then pretty-prints its full path. Read-only: like
/// `curate`, it never shells out to aida or mutates anything.
// trace:STORY-77 | ai:claude
pub fn run_session_show(
    args: impl IntoIterator<Item = String>,
    output: &mut dyn Write,
) -> Result<()> {
    let config = ShowConfig::parse(args)?;
    let path = config.log_path()?;
    if !path.exists() {
        return Err(QuizdomError::Usage(format!(
            "no session log at {}",
            path.display()
        )));
    }
    let file = std::fs::File::open(&path)?;
    render_transcript(file, config.branch.as_deref(), output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    // A log exercising every rendered event type across two branches:
    //   main: Q-1 (with a surfaced definition + a resolved contradiction),
    //         then a fork, then Q-2.
    //   agree: Q-3 (so the branch filter has something to keep/drop).
    const SAMPLE_LOG: &str = r#"
{"event_type":"session_started","session_id":"sess-1","user_id":"ada","branch_id":"main","strategy":"deterministic"}
{"event_type":"question_presented","session_id":"sess-1","user_id":"ada","branch_id":"main","turn":1,"question_ref":"Q-1","question_text":"What is freedom?"}
{"event_type":"term_interpreted","session_id":"sess-1","user_id":"ada","branch_id":"main","turn":1,"term":"freedom","adopted_definition":"the absence of constraint"}
{"event_type":"answer_recorded","session_id":"sess-1","user_id":"ada","branch_id":"main","turn":1,"question_ref":"Q-1","raw_answer":"it means choosing","normalized_answer":"free-text"}
{"event_type":"contradiction_resolved","session_id":"sess-1","user_id":"ada","branch_id":"main","turn":1,"left_belief":"freedom is absolute","right_belief":"freedom has limits","kept_side":"right"}
{"event_type":"next_question_selected","session_id":"sess-1","user_id":"ada","branch_id":"main","turn":1,"question_ref":"Q-1","selected_next_question_ref":"Q-2","selection_reason":"begets"}
{"event_type":"branch_forked","session_id":"sess-1","user_id":"ada","proposition":"is freedom absolute?","branches":[{"branch_id":"agree","stance":"agree","seed_question_ref":"Q-3"},{"branch_id":"disagree","stance":"disagree","seed_question_ref":"Q-4"}]}
{"event_type":"question_presented","session_id":"sess-1","user_id":"ada","branch_id":"main","turn":2,"question_ref":"Q-2","question_text":"Is freedom innate?"}
{"event_type":"answer_recorded","session_id":"sess-1","user_id":"ada","branch_id":"main","turn":2,"question_ref":"Q-2","raw_answer":"yes","normalized_answer":"yes"}
{"event_type":"question_presented","session_id":"sess-1","user_id":"ada","branch_id":"agree","turn":3,"question_ref":"Q-3","question_text":"Can freedom be lost?"}
{"event_type":"answer_recorded","session_id":"sess-1","user_id":"ada","branch_id":"agree","turn":3,"question_ref":"Q-3","raw_answer":"no","normalized_answer":"no"}
{"event_type":"session_ended","session_id":"sess-1","user_id":"ada","branch_id":"main","turn":2,"summary":"explored 3 questions"}
"#;

    fn render(log: &str, branch: Option<&str>) -> String {
        let mut output = Vec::new();
        render_transcript(log.as_bytes(), branch, &mut output).expect("render should succeed");
        String::from_utf8(output).expect("utf8")
    }

    fn strings<const N: usize>(args: [&str; N]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn renders_header_questions_and_answers() {
        let rendered = render(SAMPLE_LOG, None);
        assert!(rendered.contains("Transcript for sess-1 · user ada"));
        assert!(rendered.contains("── branch main · strategy deterministic ──"));
        assert!(rendered.contains("turn 1 · Q-1"));
        assert!(rendered.contains("  Q: What is freedom?"));
        // Free-text answer keeps the raw text and shows the normalized form.
        assert!(rendered.contains("  A: it means choosing [free-text]"));
        // A plain yes-no answer doesn't repeat itself.
        assert!(rendered.contains("  A: yes"));
        assert!(!rendered.contains("yes [yes]"));
        assert!(rendered.contains("── ended · explored 3 questions ──"));
    }

    #[test]
    fn renders_definitions_contradictions_and_path() {
        let rendered = render(SAMPLE_LOG, None);
        assert!(rendered.contains("  ✎ defined \"freedom\": the absence of constraint"));
        assert!(rendered.contains(
            "  ⚠ contradiction: \"freedom is absolute\" vs \"freedom has limits\" → kept right"
        ));
        assert!(rendered.contains("  ↳ next: Q-2 (begets)"));
    }

    #[test]
    fn renders_branch_fork_marker() {
        let rendered = render(SAMPLE_LOG, None);
        assert!(
            rendered.contains("⑂ forked \"is freedom absolute?\": agree → Q-3 · disagree → Q-4")
        );
    }

    #[test]
    fn branch_filter_keeps_one_branch_and_the_fork() {
        let rendered = render(SAMPLE_LOG, Some("main"));
        // The `main` questions stay...
        assert!(rendered.contains("Q: What is freedom?"));
        assert!(rendered.contains("Q: Is freedom innate?"));
        // ...the `agree` branch's question is filtered out...
        assert!(!rendered.contains("Q: Can freedom be lost?"));
        // ...but the fork marker survives so the split is still visible.
        assert!(rendered.contains("⑂ forked"));
    }

    #[test]
    fn renders_path_truncated_marker() {
        let log = r#"
{"event_type":"session_started","session_id":"sess-9","user_id":"local-user","branch_id":"main","strategy":"weighted"}
{"event_type":"path_truncated","session_id":"sess-9","user_id":"local-user","branch_id":"main","from_turn":2,"reason":"contradiction"}
"#;
        let rendered = render(log, None);
        assert!(rendered.contains("── branch main · strategy weighted ──"));
        assert!(rendered.contains("✂ truncated from turn 2 (contradiction)"));
    }

    #[test]
    fn empty_log_reports_no_events() {
        let rendered = render("", None);
        assert!(rendered.contains("Transcript for (unknown) · user (unknown)"));
        assert!(rendered.contains("(no transcript events)"));
    }

    #[test]
    fn parse_takes_positional_id_and_flags() {
        let config = ShowConfig::parse(strings([
            "session", "show", "1234", "--user", "ada", "--branch", "main",
        ]))
        .expect("parse should succeed");
        assert_eq!(config.user_id, "ada");
        assert_eq!(config.session_id.as_deref(), Some("sess-1234"));
        assert_eq!(config.branch.as_deref(), Some("main"));
    }

    #[test]
    fn parse_session_flag_is_normalized() {
        let config = ShowConfig::parse(strings(["session", "show", "--session", "sess-7"]))
            .expect("parse should succeed");
        assert_eq!(config.session_id.as_deref(), Some("sess-7"));
    }

    #[test]
    fn parse_rejects_unknown_flag() {
        let error = ShowConfig::parse(strings(["session", "show", "--nope"])).unwrap_err();
        assert!(matches!(error, QuizdomError::Usage(_)));
    }

    #[test]
    fn log_path_resolves_id_against_user_dir() {
        let config =
            ShowConfig::parse(strings(["session", "show", "sess-1", "--user", "ada"])).unwrap();
        assert_eq!(
            config.log_path().expect("path"),
            PathBuf::from("data/users/ada/sessions/sess-1.jsonl")
        );
    }

    #[test]
    fn log_path_without_id_is_a_usage_error() {
        let config = ShowConfig::parse(strings(["session", "show"])).unwrap();
        assert!(matches!(config.log_path(), Err(QuizdomError::Usage(_))));
    }

    /// Write `contents` to a unique temp file and return its path. Uniqueness
    /// comes from pid + a process-wide counter so parallel tests don't collide.
    fn temp_log(contents: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "quizdom-show-{}-{}.jsonl",
            std::process::id(),
            nonce
        ));
        std::fs::write(&path, contents).expect("write temp log");
        path
    }

    #[test]
    fn run_session_show_renders_an_explicit_log() {
        let log = temp_log(SAMPLE_LOG);
        let mut output = Vec::new();
        run_session_show(
            strings(["session", "show", "--log", log.to_str().unwrap()]),
            &mut output,
        )
        .expect("run should succeed");
        std::fs::remove_file(&log).ok();
        let rendered = String::from_utf8(output).expect("utf8");
        assert!(rendered.contains("Transcript for sess-1 · user ada"));
        assert!(rendered.contains("Q: What is freedom?"));
    }

    #[test]
    fn run_session_show_missing_log_is_a_usage_error() {
        let mut output = Vec::new();
        let error = run_session_show(
            strings(["session", "show", "--log", "/tmp/does-not-exist-xyz.jsonl"]),
            &mut output,
        )
        .unwrap_err();
        assert!(matches!(error, QuizdomError::Usage(_)));
    }
}
