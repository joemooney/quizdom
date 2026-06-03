// trace:STORY-128 | ai:claude
//! The GLOBAL synopsis mode of the Observer: a BELIEF-NEUTRAL, CLARIFY-ONLY
//! reading of a *whole session* rather than a single exchange (STORY-127).
//!
//! Where the per-exchange observer (`observer.rs`) reads one question / answer /
//! rebuttal turn, the synopsis reads the full JSONL session log and summarizes
//! the **arc**:
//!
//! - the positions taken across the session,
//! - how they evolved (what shifted, what held),
//! - the internal consistency of those positions (the "tensions"),
//! - where the user now stands, and
//! - what is still unresolved (the open threads),
//!
//! plus a short belief-neutral **engagement** read (clarity / consistency /
//! precision) — *never* belief grading. It never says which position is
//! "right", never advocates a belief, and never supplies the user's answer. It
//! only helps the user see the shape of their own session more clearly.
//!
//! Like the per-exchange observer, the synopsis is produced by an LLM (default
//! backend: claude-cli). When the LLM is unavailable (offline, not logged in,
//! malformed response), it degrades to a **structural summary** derived purely
//! from the log — the recorded turns and positions, with no model and no belief
//! content invented.
//!
//! It is surfaced two ways:
//! - the standalone `quizdom session synopsis <id> [--user]` command, and
//! - an in-session key (handled in `session.rs`).

use crate::error::{QuizdomError, Result};
use crate::session::normalize_session_id;
use llm::{LLMClient, Message};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;

/// Default quizdom user whose session logs the synopsis command reads when no
/// `--user` is given — matching the session loop's own default.
const DEFAULT_USER: &str = "local-user";

/// One recorded step of a session: the question that was asked and the position
/// the user took on it. Extracted from the JSONL log purely structurally — no
/// belief content is invented, only the user's own recorded words are carried.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SessionTurn {
    /// The turn number, as recorded in the log.
    pub turn: u64,
    /// The branch this turn belongs to (default `"main"`).
    pub branch: String,
    /// The question text that was asked.
    pub question: String,
    /// The user's raw position / answer on that question.
    pub position: String,
}

/// The structural arc of a session: the ordered turns plus the standalone
/// markers (definitions settled, contradictions surfaced) that colour it.
///
/// This is the belief-neutral substrate both the LLM synopsis and the offline
/// structural summary are built from. It carries only what the user recorded —
/// it never adds a position, a judgement, or a belief.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct SessionArc {
    /// The session id, if the log carried one.
    pub session_id: String,
    /// The user id, if the log carried one.
    pub user_id: String,
    /// The ordered positions the user took.
    pub turns: Vec<SessionTurn>,
    /// Terms the user settled a working definition for (term + definition).
    pub definitions: Vec<(String, String)>,
    /// Tensions already surfaced in-session (contradictions the session flagged),
    /// rendered as a short "left vs right" label. May feed the synopsis's
    /// consistency read (EPIC-9 contradiction detection).
    pub tensions: Vec<String>,
}

impl SessionArc {
    /// True when the log carried no recorded position at all — there is nothing
    /// to summarize.
    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }
}

/// Build the structural [`SessionArc`] from a session JSONL log.
///
/// `branch` filters to a single branch (matching `branch_id`, default `"main"`);
/// pass `None` to fold every branch into the arc. Purely structural: it reads
/// the recorded events and carries the user's own words, inventing nothing.
pub fn arc_from_session_log(reader: impl Read, branch: Option<&str>) -> Result<SessionArc> {
    let reader = BufReader::new(reader);
    let mut arc = SessionArc::default();
    // Question text is logged on `question_presented`; the answer arrives on a
    // later `answer_recorded` for the same turn, so we stage the text by turn.
    let mut pending_question: Vec<(u64, String, String)> = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value =
            serde_json::from_str(&line).map_err(|error| QuizdomError::Parse(error.to_string()))?;

        let event_branch = value
            .get("branch_id")
            .and_then(Value::as_str)
            .unwrap_or("main");
        // `branch_forked` carries no branch_id and is not branch-specific; the
        // arc never renders it, so we just skip non-matching branches uniformly.
        if let Some(branch) = branch {
            if event_branch != branch {
                continue;
            }
        }

        if arc.session_id.is_empty() {
            if let Some(id) = value.get("session_id").and_then(Value::as_str) {
                arc.session_id = id.to_string();
            }
        }
        if arc.user_id.is_empty() {
            if let Some(id) = value.get("user_id").and_then(Value::as_str) {
                arc.user_id = id.to_string();
            }
        }

        match value.get("event_type").and_then(Value::as_str) {
            Some("question_presented") => {
                if let (Some(turn), Some(text)) = (
                    value.get("turn").and_then(Value::as_u64),
                    value.get("question_text").and_then(Value::as_str),
                ) {
                    pending_question.push((turn, event_branch.to_string(), text.to_string()));
                }
            }
            Some("answer_recorded") => {
                let Some(turn) = value.get("turn").and_then(Value::as_u64) else {
                    continue;
                };
                let position = value
                    .get("raw_answer")
                    .and_then(Value::as_str)
                    .or_else(|| value.get("normalized_answer").and_then(Value::as_str))
                    .unwrap_or("")
                    .trim()
                    .to_string();
                // Pair with the most recent matching question on this branch.
                let question = pending_question
                    .iter()
                    .rev()
                    .find(|(t, b, _)| *t == turn && b == event_branch)
                    .map(|(_, _, text)| text.clone())
                    .unwrap_or_default();
                if question.is_empty() && position.is_empty() {
                    continue;
                }
                arc.turns.push(SessionTurn {
                    turn,
                    branch: event_branch.to_string(),
                    question,
                    position,
                });
            }
            Some("term_interpreted") => {
                let term = value.get("term").and_then(Value::as_str).unwrap_or("");
                let definition = value
                    .get("adopted_definition")
                    .and_then(Value::as_str)
                    .or_else(|| value.get("raw_definition").and_then(Value::as_str))
                    .unwrap_or("");
                if !term.is_empty() && !definition.is_empty() {
                    arc.definitions
                        .push((term.to_string(), definition.to_string()));
                }
            }
            Some("contradiction_resolved") => {
                let left = value
                    .get("left_belief")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let right = value
                    .get("right_belief")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !left.is_empty() || !right.is_empty() {
                    arc.tensions.push(format!("\"{left}\" vs \"{right}\""));
                }
            }
            _ => {}
        }
    }

    Ok(arc)
}

/// A belief-neutral, clarify-only synopsis of a whole [`SessionArc`].
///
/// Every field is descriptive, never prescriptive: it names the shape of the
/// session — positions, evolution, consistency, open threads — without
/// supplying an answer or asserting which belief is correct.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SessionSynopsis {
    /// The positions the user took, in their own terms.
    pub positions: Vec<String>,
    /// How the positions evolved across the session (what shifted, what held).
    pub evolution: String,
    /// The internal consistency of the positions — the tensions, named
    /// belief-neutrally, never resolved in the user's stead.
    pub consistency: String,
    /// Where the user now stands, as a structural summary of their arc.
    pub standing: String,
    /// What is still unresolved — the open threads the session left dangling.
    pub open_threads: Vec<String>,
    /// A short belief-neutral engagement read (clarity / consistency /
    /// precision) — NOT a grade of which belief is right.
    pub engagement: String,
    /// True when this synopsis was synthesized structurally (offline / degraded)
    /// rather than by the LLM.
    pub degraded: bool,
}

/// System prompt pinning the synopsis to its belief-neutral, clarify-only
/// contract. Mirrors the per-exchange observer's contract, scaled to a whole
/// session.
const SYNOPSIS_SYSTEM_PROMPT: &str = "You are quizdom's session Synopsis observer. You are STRICTLY belief-neutral and clarify-only. You read a WHOLE session — the positions the user took across many questions — and summarize the ARC so the user can see their own thinking more clearly. You MUST NOT supply the user's answer, take a side, assert which belief is correct, advocate a position, or grade which belief is better. Assess ENGAGEMENT only: clarity, internal consistency, and precision — never belief correctness. Only: list the positions taken, describe how they evolved, name the internal tensions (without resolving them), summarize where the user now stands, and list what is still unresolved. Stay descriptive, not prescriptive.";

/// Build the synopsis prompt for one [`SessionArc`].
fn synopsis_prompt(arc: &SessionArc) -> String {
    let mut log = String::new();
    for turn in &arc.turns {
        log.push_str(&format!(
            "- (turn {}, branch {}) Q: {} | position: {}\n",
            turn.turn,
            turn.branch,
            turn.question,
            if turn.position.is_empty() {
                "(no answer)"
            } else {
                &turn.position
            }
        ));
    }
    if !arc.definitions.is_empty() {
        log.push_str("Working definitions the user settled:\n");
        for (term, definition) in &arc.definitions {
            log.push_str(&format!("- {term}: {definition}\n"));
        }
    }
    if !arc.tensions.is_empty() {
        log.push_str("Tensions already surfaced in-session:\n");
        for tension in &arc.tensions {
            log.push_str(&format!("- {tension}\n"));
        }
    }
    format!(
        "Here is the session arc (the user's own recorded positions):\n{log}\n\
         Return only JSON with these fields: {{\"positions\":[\"a position the user took, in their terms\"],\"evolution\":\"how the positions evolved across the session\",\"consistency\":\"the internal tensions, named neutrally and NOT resolved\",\"standing\":\"where the user now stands\",\"open_threads\":[\"a question the session left unresolved\"],\"engagement\":\"short read of clarity/consistency/precision — NOT which belief is right\"}}. \
         Do NOT supply an answer, take a side, advocate a position, or grade which belief is right."
    )
}

/// Read a [`SessionArc`] with the supplied LLM client, degrading to a structural
/// summary when the call fails or returns something unusable.
pub fn synopsize<C: LLMClient>(client: &C, arc: &SessionArc) -> SessionSynopsis {
    match llm_synopsize(client, arc) {
        Some(synopsis) => synopsis,
        // Offline / not-logged-in / malformed: fall back to the structural
        // summary rather than failing the synopsis request.
        None => structural_synopsis(arc),
    }
}

/// The LLM leg of [`synopsize`]: run the call on a current-thread runtime, parse
/// the JSON, and enforce the no-answer-supplied guarantee. Returns `None` on any
/// failure so the caller degrades gracefully.
fn llm_synopsize<C: LLMClient>(client: &C, arc: &SessionArc) -> Option<SessionSynopsis> {
    let prompt = synopsis_prompt(arc);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .ok()?;
    let (text, _tool_calls) = runtime
        .block_on(client.call(SYNOPSIS_SYSTEM_PROMPT, &[Message::user(prompt)], &[]))
        .ok()?;
    parse_synopsis(&text, arc)
}

/// Parse the synopsis JSON into a [`SessionSynopsis`], enforcing the
/// belief-neutral / no-answer-supplied guarantee.
///
/// Returns `None` when the payload is not the expected JSON object so the caller
/// degrades to the structural summary. Any field (or list item) that reproduces
/// one of the user's own recorded positions verbatim is scrubbed (see
/// [`scrub_supplied_position`]) so a misbehaving model can never hand the user's
/// answer back as if it were guidance.
pub fn parse_synopsis(text: &str, arc: &SessionArc) -> Option<SessionSynopsis> {
    let value: Value = serde_json::from_str(text.trim()).ok()?;
    if !value.is_object() {
        return None;
    }
    let positions: Vec<String> = arc.turns.iter().map(|t| t.position.clone()).collect();
    let field = |key: &str| -> String {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .map(|raw| scrub_supplied_position(raw, &positions))
            .unwrap_or_default()
    };
    let list = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(|item| scrub_supplied_position(item, &positions))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };

    let positions_out = list("positions");
    let evolution = field("evolution");
    let consistency = field("consistency");
    let standing = field("standing");
    let open_threads = list("open_threads");
    let engagement = field("engagement");

    // A synopsis with no usable content is no better than the structural
    // summary; let the caller degrade instead of rendering an empty box.
    if positions_out.is_empty()
        && evolution.is_empty()
        && consistency.is_empty()
        && standing.is_empty()
        && open_threads.is_empty()
        && engagement.is_empty()
    {
        return None;
    }

    Some(SessionSynopsis {
        positions: positions_out,
        evolution,
        consistency,
        standing,
        open_threads,
        engagement,
        degraded: false,
    })
}

/// The no-answer-supplied guarantee for the global synopsis. The observer must
/// never hand one of the user's own recorded positions back as if it were
/// guidance, so if a field reproduces a position verbatim (case-insensitively)
/// we replace it with a neutral placeholder. Empty positions are ignored
/// (nothing to leak).
fn scrub_supplied_position(field: &str, positions: &[String]) -> String {
    let candidate = field.trim();
    for position in positions {
        let position = position.trim();
        if !position.is_empty() && candidate.eq_ignore_ascii_case(position) {
            return "(withheld: the observer does not supply your answer)".to_string();
        }
    }
    field.to_string()
}

/// The offline / degraded synopsis: a minimal *structural* summary derived
/// purely from the [`SessionArc`]. It invents no belief content — it only
/// restates the recorded positions, counts the turns, surfaces any in-session
/// tensions, and names the questions left without an answer.
pub fn structural_synopsis(arc: &SessionArc) -> SessionSynopsis {
    let positions: Vec<String> = arc
        .turns
        .iter()
        .filter(|turn| !turn.position.is_empty())
        .map(|turn| {
            let asked = first_sentence(&turn.question);
            if asked.is_empty() {
                turn.position.clone()
            } else {
                format!("On \"{asked}\": {}", turn.position)
            }
        })
        .collect();

    let answered = arc
        .turns
        .iter()
        .filter(|turn| !turn.position.is_empty())
        .count();
    let branches: std::collections::BTreeSet<&str> =
        arc.turns.iter().map(|turn| turn.branch.as_str()).collect();
    let evolution = if answered == 0 {
        "No positions recorded yet.".to_string()
    } else if branches.len() > 1 {
        format!(
            "Recorded {answered} position(s) across {} branch(es).",
            branches.len()
        )
    } else {
        format!("Recorded {answered} position(s) along one line of inquiry.")
    };

    let consistency = if arc.tensions.is_empty() {
        "Offline summary: no tensions were surfaced in-session; re-read the positions for consistency yourself.".to_string()
    } else {
        format!(
            "Tensions surfaced in-session: {}. The observer names them but does not resolve them.",
            arc.tensions.join("; ")
        )
    };

    let standing = match arc
        .turns
        .iter()
        .rev()
        .find(|turn| !turn.position.is_empty())
    {
        Some(last) => format!(
            "Most recent position — on \"{}\": {}",
            first_sentence(&last.question),
            last.position
        ),
        None => "No position recorded — nothing to stand on yet.".to_string(),
    };

    let open_threads: Vec<String> = arc
        .turns
        .iter()
        .filter(|turn| turn.position.is_empty() && !turn.question.is_empty())
        .map(|turn| format!("Unanswered: \"{}\"", first_sentence(&turn.question)))
        .collect();

    SessionSynopsis {
        positions,
        evolution,
        consistency,
        standing,
        open_threads,
        engagement:
            "Offline reading: re-read your positions and check each one is clear, consistent, and precise."
                .to_string(),
        degraded: true,
    }
}

/// The first sentence (or the whole string if it has no terminator), trimmed,
/// used to keep the structural summary compact without echoing a long prompt.
fn first_sentence(text: &str) -> String {
    let trimmed = text.trim();
    trimmed
        .split_inclusive(['.', '?', '!'])
        .next()
        .unwrap_or(trimmed)
        .trim()
        .to_string()
}

/// Render a [`SessionSynopsis`] as a clearly-labeled META voice, visually
/// distinct (style::meta), mirroring the per-exchange observer's rendering.
/// Belief-neutral and clarify-only: it lists positions, describes the arc, names
/// tensions, and lists open threads — it never advocates a belief. Pure over the
/// buffer + synopsis, so it is unit-testable without a live LLM.
pub fn render_synopsis(synopsis: &SessionSynopsis, output: &mut impl Write) -> Result<()> {
    let header = if synopsis.degraded {
        "META (synopsis, offline) — a belief-neutral reading of this session:"
    } else {
        "META (synopsis) — a belief-neutral reading of this session:"
    };
    writeln!(
        output,
        "\n{}",
        crate::style::paint(crate::style::meta(), header)
    )?;

    let line = |label: &str, body: &str, output: &mut dyn Write| -> Result<()> {
        if !body.trim().is_empty() {
            writeln!(
                output,
                "{}",
                crate::style::paint(crate::style::meta(), &format!("  {label}: {body}"))
            )?;
        }
        Ok(())
    };
    let bullets = |label: &str, items: &[String], output: &mut dyn Write| -> Result<()> {
        if !items.is_empty() {
            writeln!(
                output,
                "{}",
                crate::style::paint(crate::style::meta(), &format!("  {label}:"))
            )?;
            for item in items {
                writeln!(
                    output,
                    "{}",
                    crate::style::paint(crate::style::meta(), &format!("    - {item}"))
                )?;
            }
        }
        Ok(())
    };

    bullets("Positions taken", &synopsis.positions, output)?;
    line("How they evolved", &synopsis.evolution, output)?;
    line("Internal consistency", &synopsis.consistency, output)?;
    line("Where you stand", &synopsis.standing, output)?;
    bullets("Still unresolved", &synopsis.open_threads, output)?;
    line("Engagement", &synopsis.engagement, output)?;
    Ok(())
}

/// Which LLM backend the standalone synopsis command uses.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SynopsisBackend {
    ClaudeCli,
    Anthropic,
    Disabled,
}

/// Parsed flags for the `quizdom session synopsis <id>` command.
///
/// Mirrors `session show`'s dispatch: `<id>` (or `--session`) names the session;
/// `--log` points at one explicit file; `--user` selects whose sessions
/// directory the id resolves against; `--branch` folds to a single branch.
#[derive(Debug)]
struct SynopsisConfig {
    user_id: String,
    session_id: Option<String>,
    log_path: Option<PathBuf>,
    branch: Option<String>,
    backend: SynopsisBackend,
}

impl SynopsisConfig {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut user_id = DEFAULT_USER.to_string();
        let mut session_id = None;
        let mut log_path = None;
        let mut branch = None;
        let mut backend = env_backend();
        let mut args = args.into_iter().peekable();

        // Strip the `session synopsis` command prefix the dispatcher passes.
        if matches!(args.peek().map(String::as_str), Some("session")) {
            args.next();
        }
        if matches!(args.peek().map(String::as_str), Some("synopsis")) {
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
                "--backend" => backend = parse_backend(&next_arg(&mut args, "--backend")?)?,
                "--no-llm" => backend = SynopsisBackend::Disabled,
                "--help" | "-h" => return Err(QuizdomError::Usage(synopsis_usage())),
                other if !other.starts_with('-') => {
                    session_id = Some(normalize_session_id(other));
                }
                other => {
                    return Err(QuizdomError::Usage(format!(
                        "unknown argument: {other}\n{}",
                        synopsis_usage()
                    )))
                }
            }
        }

        Ok(Self {
            user_id,
            session_id,
            log_path,
            branch,
            backend,
        })
    }

    /// The single log file to read: an explicit `--log`, otherwise the `<id>`
    /// resolved against the user's sessions directory. Mirrors `session show`.
    fn log_path(&self) -> Result<PathBuf> {
        if let Some(log_path) = &self.log_path {
            return Ok(log_path.clone());
        }
        let session_id = self.session_id.as_ref().ok_or_else(|| {
            QuizdomError::Usage(format!(
                "session synopsis requires a session id\n{}",
                synopsis_usage()
            ))
        })?;
        Ok(PathBuf::from("data")
            .join("users")
            .join(&self.user_id)
            .join("sessions")
            .join(format!("{session_id}.jsonl")))
    }
}

fn synopsis_usage() -> String {
    "usage: quizdom session synopsis <session-id> [--user local-user] [--branch main] [--log path] [--backend claude-cli|anthropic|none] [--no-llm]"
        .to_string()
}

fn env_backend() -> SynopsisBackend {
    std::env::var("QUIZDOM_BACKEND")
        .ok()
        .and_then(|value| parse_backend(&value).ok())
        .unwrap_or(SynopsisBackend::ClaudeCli)
}

fn parse_backend(value: &str) -> Result<SynopsisBackend> {
    match value {
        "claude-cli" | "claude_cli" | "claude" => Ok(SynopsisBackend::ClaudeCli),
        "anthropic" => Ok(SynopsisBackend::Anthropic),
        "none" | "off" | "disabled" => Ok(SynopsisBackend::Disabled),
        other => Err(QuizdomError::Usage(format!(
            "unknown LLM backend: {other}; expected claude-cli, anthropic, or none"
        ))),
    }
}

fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| QuizdomError::Usage(format!("{name} requires a value")))
}

/// Produce a synopsis for `arc` with the command's configured backend, degrading
/// to the structural summary when the backend is disabled or unavailable.
fn synopsize_with_backend(backend: SynopsisBackend, arc: &SessionArc) -> SessionSynopsis {
    match backend {
        SynopsisBackend::Disabled => structural_synopsis(arc),
        SynopsisBackend::ClaudeCli => {
            let client = llm::ClaudeCliClient::from_env();
            synopsize(&client, arc)
        }
        SynopsisBackend::Anthropic => match llm::AnthropicClient::from_env() {
            Ok(client) => synopsize(&client, arc),
            // No API key / misconfigured: degrade to the structural summary
            // rather than failing the command.
            Err(_) => structural_synopsis(arc),
        },
    }
}

/// Entry point for the standalone `quizdom session synopsis <id>` command.
/// Resolves the session's log file, reads its arc, and renders a belief-neutral
/// global synopsis. Read-only: like `session show`, it never mutates anything.
pub fn run_session_synopsis(
    args: impl IntoIterator<Item = String>,
    output: &mut impl Write,
) -> Result<()> {
    let config = SynopsisConfig::parse(args)?;
    let path = config.log_path()?;
    if !path.exists() {
        return Err(QuizdomError::Usage(format!(
            "no session log at {}",
            path.display()
        )));
    }
    let file = std::fs::File::open(&path)?;
    let arc = arc_from_session_log(file, config.branch.as_deref())?;

    let header = if arc.session_id.is_empty() {
        "Session synopsis".to_string()
    } else if arc.user_id.is_empty() {
        format!("Synopsis for {}", arc.session_id)
    } else {
        format!("Synopsis for {} · user {}", arc.session_id, arc.user_id)
    };
    writeln!(output, "{header}")?;

    if arc.is_empty() {
        writeln!(output, "(no recorded positions to summarize)")?;
        return Ok(());
    }

    let synopsis = synopsize_with_backend(config.backend, &arc);
    render_synopsis(&synopsis, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::{LLMError, LLMFuture, Message, ToolDef};
    use std::cell::RefCell;

    struct MockClient {
        response: RefCell<Option<std::result::Result<String, LLMError>>>,
        last_prompt: RefCell<Option<String>>,
    }

    impl MockClient {
        fn ok(body: &str) -> Self {
            Self {
                response: RefCell::new(Some(Ok(body.to_string()))),
                last_prompt: RefCell::new(None),
            }
        }

        fn err() -> Self {
            Self {
                response: RefCell::new(Some(Err(LLMError::Provider("offline".to_string())))),
                last_prompt: RefCell::new(None),
            }
        }
    }

    impl LLMClient for MockClient {
        fn call<'a>(
            &'a self,
            _system: &'a str,
            messages: &'a [Message],
            _tools: &'a [ToolDef],
        ) -> LLMFuture<'a> {
            if let Some(message) = messages.first() {
                *self.last_prompt.borrow_mut() = Some(message.content.clone());
            }
            let response = self.response.borrow_mut().take();
            Box::pin(async move {
                match response {
                    Some(Ok(text)) => Ok((text, Vec::new())),
                    Some(Err(error)) => Err(error),
                    None => Err(LLMError::Provider("no mock response".to_string())),
                }
            })
        }
    }

    // A two-turn, two-branch session: a free-text position, a yes/no position,
    // a settled definition, and a surfaced tension — enough to exercise every
    // arc field and the branch filter.
    const SAMPLE_LOG: &str = r#"
{"event_type":"session_started","session_id":"sess-7","user_id":"ada","branch_id":"main","strategy":"deterministic"}
{"event_type":"question_presented","session_id":"sess-7","user_id":"ada","branch_id":"main","turn":1,"question_ref":"Q-1","question_text":"Is free will real?"}
{"event_type":"term_interpreted","session_id":"sess-7","user_id":"ada","branch_id":"main","turn":1,"term":"free","adopted_definition":"uncaused"}
{"event_type":"answer_recorded","session_id":"sess-7","user_id":"ada","branch_id":"main","turn":1,"question_ref":"Q-1","raw_answer":"yes, obviously","normalized_answer":"free-text"}
{"event_type":"contradiction_resolved","session_id":"sess-7","user_id":"ada","branch_id":"main","turn":1,"left_belief":"choices are caused","right_belief":"choices are free","kept_side":"right"}
{"event_type":"question_presented","session_id":"sess-7","user_id":"ada","branch_id":"main","turn":2,"question_ref":"Q-2","question_text":"Can a caused choice be free?"}
{"event_type":"answer_recorded","session_id":"sess-7","user_id":"ada","branch_id":"main","turn":2,"question_ref":"Q-2","raw_answer":"no","normalized_answer":"no"}
{"event_type":"question_presented","session_id":"sess-7","user_id":"ada","branch_id":"agree","turn":3,"question_ref":"Q-3","question_text":"Is determinism compatible with freedom?"}
{"event_type":"answer_recorded","session_id":"sess-7","user_id":"ada","branch_id":"agree","turn":3,"question_ref":"Q-3","raw_answer":"yes","normalized_answer":"yes"}
{"event_type":"session_ended","session_id":"sess-7","user_id":"ada","branch_id":"main","turn":2,"summary":"explored 3 questions"}
"#;

    fn arc(branch: Option<&str>) -> SessionArc {
        arc_from_session_log(SAMPLE_LOG.as_bytes(), branch).expect("arc should parse")
    }

    fn strings<const N: usize>(args: [&str; N]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn arc_extracts_positions_definitions_and_tensions() {
        let arc = arc(None);
        assert_eq!(arc.session_id, "sess-7");
        assert_eq!(arc.user_id, "ada");
        assert_eq!(arc.turns.len(), 3);
        assert_eq!(arc.turns[0].question, "Is free will real?");
        assert_eq!(arc.turns[0].position, "yes, obviously");
        assert_eq!(
            arc.definitions,
            vec![("free".to_string(), "uncaused".to_string())]
        );
        assert_eq!(arc.tensions.len(), 1);
        assert!(arc.tensions[0].contains("choices are caused"));
    }

    #[test]
    fn arc_branch_filter_keeps_one_branch() {
        let arc = arc(Some("main"));
        assert_eq!(arc.turns.len(), 2);
        assert!(arc.turns.iter().all(|turn| turn.branch == "main"));
    }

    #[test]
    fn arc_of_an_empty_log_is_empty() {
        let arc = arc_from_session_log("".as_bytes(), None).expect("empty arc");
        assert!(arc.is_empty());
    }

    #[test]
    fn synopsizes_from_the_llm_belief_neutrally() {
        let client = MockClient::ok(
            r#"{
                "positions": ["Free will is real", "A caused choice is not free"],
                "evolution": "Started confident, then drew a line at caused choices.",
                "consistency": "Tension between 'free' as uncaused and a wish to keep some freedom under causation.",
                "standing": "Holds free will is real but only when uncaused.",
                "open_threads": ["Whether any choice is truly uncaused"],
                "engagement": "Clear positions, but the key term shifts, so precision wavers."
            }"#,
        );
        let synopsis = synopsize(&client, &arc(None));
        assert!(!synopsis.degraded);
        assert_eq!(synopsis.positions.len(), 2);
        assert!(synopsis.evolution.contains("caused"));
        assert!(synopsis.consistency.contains("Tension"));
        assert_eq!(synopsis.open_threads.len(), 1);
        assert!(synopsis.engagement.contains("precision") || synopsis.engagement.contains("term"));
    }

    #[test]
    fn the_prompt_pins_belief_neutrality() {
        let client = MockClient::ok(r#"{"standing":"s"}"#);
        let _ = synopsize(&client, &arc(None));
        let prompt = client.last_prompt.borrow().clone().unwrap();
        assert!(prompt.contains("Do NOT supply an answer"));
        assert!(prompt.contains("grade which belief is right"));
        // The arc text the model sees is the user's own positions.
        assert!(prompt.contains("yes, obviously"));
    }

    #[test]
    fn never_supplies_the_users_position() {
        // A misbehaving model that echoes one of the user's positions verbatim
        // into a field must NOT have it pass through as guidance.
        let client = MockClient::ok(
            r#"{"positions":["yes, obviously"],"standing":"yes, obviously","evolution":"e","consistency":"c","open_threads":[],"engagement":"x"}"#,
        );
        let synopsis = synopsize(&client, &arc(None));
        assert!(
            synopsis.standing.contains("withheld"),
            "the verbatim position must be withheld, got: {}",
            synopsis.standing
        );
        assert!(
            synopsis.positions.iter().all(|p| p != "yes, obviously"),
            "positions must not echo the user's verbatim answer"
        );
    }

    #[test]
    fn degrades_to_a_structural_summary_when_offline() {
        let client = MockClient::err();
        let synopsis = synopsize(&client, &arc(None));
        assert!(synopsis.degraded);
        // The structural summary names the recorded positions, the in-session
        // tension, and where the user last stood — inventing no belief content.
        assert!(synopsis
            .positions
            .iter()
            .any(|p| p.contains("yes, obviously")));
        assert!(synopsis.consistency.contains("choices are caused"));
        assert!(synopsis.standing.contains("no") || synopsis.standing.contains("Most recent"));
    }

    #[test]
    fn degrades_when_the_llm_returns_unparseable_text() {
        let client = MockClient::ok("not json at all");
        let synopsis = synopsize(&client, &arc(None));
        assert!(synopsis.degraded);
    }

    #[test]
    fn degrades_when_the_llm_returns_an_empty_object() {
        let client = MockClient::ok("{}");
        let synopsis = synopsize(&client, &arc(None));
        assert!(synopsis.degraded);
    }

    #[test]
    fn structural_summary_lists_open_threads_for_unanswered_questions() {
        let mut arc = arc(None);
        arc.turns.push(SessionTurn {
            turn: 4,
            branch: "main".to_string(),
            question: "What would change your mind?".to_string(),
            position: String::new(),
        });
        let synopsis = structural_synopsis(&arc);
        assert!(synopsis
            .open_threads
            .iter()
            .any(|thread| thread.contains("What would change your mind?")));
    }

    #[test]
    fn render_marks_offline_and_lists_positions() {
        let synopsis = structural_synopsis(&arc(None));
        let mut out = Vec::new();
        render_synopsis(&synopsis, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("META (synopsis, offline)"));
        assert!(rendered.contains("Positions taken:"));
        assert!(rendered.contains("yes, obviously"));
        // Belief-neutral: it never tells the user which side is right.
        assert!(!rendered.to_lowercase().contains("you should believe"));
    }

    #[test]
    fn run_session_synopsis_renders_an_explicit_log_offline() {
        let path = temp_log(SAMPLE_LOG);
        let mut out = Vec::new();
        run_session_synopsis(
            strings([
                "session",
                "synopsis",
                "--log",
                path.to_str().unwrap(),
                "--no-llm",
            ]),
            &mut out,
        )
        .expect("run should succeed");
        std::fs::remove_file(&path).ok();
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("Synopsis for sess-7 · user ada"));
        assert!(rendered.contains("META (synopsis, offline)"));
        assert!(rendered.contains("Positions taken:"));
    }

    #[test]
    fn run_session_synopsis_empty_log_reports_nothing_to_summarize() {
        let path = temp_log("");
        let mut out = Vec::new();
        run_session_synopsis(
            strings([
                "session",
                "synopsis",
                "--log",
                path.to_str().unwrap(),
                "--no-llm",
            ]),
            &mut out,
        )
        .expect("run should succeed");
        std::fs::remove_file(&path).ok();
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("(no recorded positions to summarize)"));
    }

    #[test]
    fn run_session_synopsis_missing_log_is_a_usage_error() {
        let mut out = Vec::new();
        let error = run_session_synopsis(
            strings([
                "session",
                "synopsis",
                "--log",
                "/tmp/does-not-exist-syn.jsonl",
            ]),
            &mut out,
        )
        .unwrap_err();
        assert!(matches!(error, QuizdomError::Usage(_)));
    }

    #[test]
    fn parse_takes_positional_id_and_flags() {
        let config = SynopsisConfig::parse(strings([
            "session", "synopsis", "1234", "--user", "ada", "--branch", "main",
        ]))
        .expect("parse should succeed");
        assert_eq!(config.user_id, "ada");
        assert_eq!(config.session_id.as_deref(), Some("sess-1234"));
        assert_eq!(config.branch.as_deref(), Some("main"));
    }

    #[test]
    fn parse_no_llm_disables_the_backend() {
        let config = SynopsisConfig::parse(strings(["session", "synopsis", "sess-1", "--no-llm"]))
            .expect("parse");
        assert_eq!(config.backend, SynopsisBackend::Disabled);
    }

    #[test]
    fn log_path_resolves_id_against_user_dir() {
        let config =
            SynopsisConfig::parse(strings(["session", "synopsis", "sess-1", "--user", "ada"]))
                .unwrap();
        assert_eq!(
            config.log_path().expect("path"),
            PathBuf::from("data/users/ada/sessions/sess-1.jsonl")
        );
    }

    #[test]
    fn log_path_without_id_is_a_usage_error() {
        let config = SynopsisConfig::parse(strings(["session", "synopsis"])).unwrap();
        assert!(matches!(config.log_path(), Err(QuizdomError::Usage(_))));
    }

    /// Write `contents` to a unique temp file and return its path.
    fn temp_log(contents: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "quizdom-synopsis-{}-{}.jsonl",
            std::process::id(),
            nonce
        ));
        std::fs::write(&path, contents).expect("write temp log");
        path
    }
}
