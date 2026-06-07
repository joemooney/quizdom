use crate::bank::{AidaCliQuestionBank, QuestionBank};
use crate::contradiction::{
    beliefs_from_session_log, detect_graph_contradictions, AidaCliContradictionResolutionPersister,
    AidaCliContradictsEdges, Contradiction, ContradictionResolution,
    ContradictionResolutionPersister, ContradictsEdges,
};
use crate::error::{QuizdomError, Result};
use crate::honing::{
    definitions_for_loaded_terms, load_probed_terms, prompt_for_term_meaning,
    render_settled_term_definition, render_term_definitions, term_label, SettledTermDefinition,
};
use crate::input::{
    read_answer_or_end, render_breadcrumb, render_question, render_question_for, AnswerInput,
    FreeTextInput, InputContext,
};
use crate::model::{Answer, AnswerKind, Question, TermDefinition};
// trace:STORY-127 | ai:claude
use crate::observer::{read_exchange, structural_reading, Exchange, ExchangeReading};
// trace:STORY-128 | ai:claude
use crate::persist::{
    AidaCliGeneratedQuestionPersister, AidaCliQuestionReweighter,
    AidaCliUserAuthoredQuestionPersister, AidaCliUserSpecificTermPersister, QuestionLink,
    QuestionReweighter, UserAuthoredQuestionPersister, UserSpecificTermPersister,
};
#[cfg(test)]
use crate::persist::{
    NoopQuestionReweighter, NoopUserAuthoredQuestionPersister, NoopUserSpecificTermPersister,
};
use crate::strategy::{
    different_topic_punt_question, AnsweredQuestion, QualitySignal, SessionMode, StrategyContext,
};
use crate::strategy::{
    DeterministicNextQuestionStrategy, LlmNextQuestionStrategy, NextQuestionStrategy,
    WeightedNextQuestionStrategy,
};
use crate::synopsis::{
    arc_from_session_log, render_synopsis, structural_synopsis, synopsize, SessionArc,
    SessionSynopsis,
};
use chrono::Utc;
use llm::{AnthropicClient, ClaudeCliClient};
use serde_json::json;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

const DEFAULT_SEED: &str = "Q-23";
const DEFAULT_USER: &str = "local-user";

#[derive(Debug, Clone)]
pub(crate) struct CliConfig {
    pub(crate) command: SessionCommand,
    pub(crate) seed: String,
    pub(crate) user_id: String,
    pub(crate) session_id: String,
    pub(crate) session_id_provided: bool,
    pub(crate) log_path: PathBuf,
    pub(crate) log_path_provided: bool,
    pub(crate) branch_id: String,
    pub(crate) proposition: Option<String>,
    pub(crate) agree_seed: Option<String>,
    pub(crate) disagree_seed: Option<String>,
    pub(crate) strategy: StrategyKind,
    pub(crate) strategy_provided: bool,
    pub(crate) llm_backend: LlmBackendKind,
    // trace:STORY-159 | ai:claude
    /// The session GOAL/thesis set at start via `--goal <text>` (one of the
    /// three ways a goal can be set — the other two are the in-session command
    /// and the Observer proposal, handled in the loop). `None` means the session
    /// starts free-flowing. Belief-neutral: the claim/question being resolved.
    pub(crate) goal: Option<String>,
    // trace:STORY-161 | ai:claude
    /// The session MODE (the EPIC-158 toggle), set at start via `--mode
    /// socratic|debate` (default `Socratic`) and overridable in-session via
    /// `/mode debate`. In `Debate` the questioner steelmans the OPPOSING side and
    /// the verdict judges which CASE was better-argued; `Socratic` keeps the
    /// neutral-challenger default. Belief-neutral throughout — never which belief
    /// is true.
    pub(crate) mode: SessionMode,
    // trace:STORY-161 | ai:claude — whether `--mode` was passed explicitly, so a
    // resume restores the logged mode only when the user did not override it.
    pub(crate) mode_provided: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum StrategyKind {
    Deterministic,
    // trace:STORY-67 | ai:claude
    Weighted,
    Llm,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum LlmBackendKind {
    ClaudeCli,
    Anthropic,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum SessionCommand {
    Start,
    Resume,
    List,
    Fork,
}

impl CliConfig {
    pub(crate) fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut command = SessionCommand::Start;
        let mut seed = DEFAULT_SEED.to_string();
        let mut user_id = DEFAULT_USER.to_string();
        let mut session_id = format!("sess-{}", Utc::now().timestamp());
        let mut session_id_provided = false;
        let mut log_path = None;
        let mut log_path_provided = false;
        let mut branch_id = "main".to_string();
        let mut proposition = None;
        let mut agree_seed = None;
        let mut disagree_seed = None;
        let mut strategy = env_strategy();
        let mut strategy_provided = false;
        let mut llm_backend = env_llm_backend();
        // trace:STORY-159 | ai:claude
        let mut goal = None;
        // trace:STORY-161 | ai:claude
        let mut mode = SessionMode::default();
        let mut mode_provided = false;
        let mut args = args.into_iter().peekable();

        if matches!(args.peek().map(String::as_str), Some("session")) {
            args.next();
        }
        if matches!(args.peek().map(String::as_str), Some("--help" | "-h")) {
            return Err(QuizdomError::Usage(usage()));
        }
        if matches!(args.peek().map(String::as_str), Some("start")) {
            args.next();
        } else if matches!(args.peek().map(String::as_str), Some("resume")) {
            command = SessionCommand::Resume;
            args.next();
        } else if matches!(args.peek().map(String::as_str), Some("list")) {
            command = SessionCommand::List;
            args.next();
        } else if matches!(args.peek().map(String::as_str), Some("fork")) {
            command = SessionCommand::Fork;
            args.next();
        } else if let Some(positional) = args.peek().cloned() {
            if !positional.starts_with('-') {
                session_id = normalize_session_id(&positional);
                session_id_provided = true;
                command = SessionCommand::Resume;
                args.next();
                if matches!(args.peek().map(String::as_str), Some("resume")) {
                    args.next();
                }
            }
        }

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--seed" => seed = next_arg(&mut args, "--seed")?,
                "--user" => user_id = next_arg(&mut args, "--user")?,
                "--session" => {
                    session_id = normalize_session_id(&next_arg(&mut args, "--session")?);
                    session_id_provided = true;
                }
                "--log" => {
                    log_path = Some(PathBuf::from(next_arg(&mut args, "--log")?));
                    log_path_provided = true;
                }
                "--branch" => branch_id = next_arg(&mut args, "--branch")?,
                // trace:STORY-159 | ai:claude — the `--goal <text>` flag sets the
                // session goal at start (way 1 of 3). An empty value is rejected
                // by `next_arg`, so a bare `--goal` is a usage error.
                "--goal" => goal = Some(next_arg(&mut args, "--goal")?),
                // trace:STORY-161 | ai:claude — the `--mode socratic|debate` flag
                // sets the session mode at start. An unrecognized value is a usage
                // error (so a typo never silently falls back to the default).
                "--mode" => {
                    let value = next_arg(&mut args, "--mode")?;
                    mode = SessionMode::parse(&value).ok_or_else(|| {
                        QuizdomError::Usage(format!(
                            "unknown mode: {value}; expected socratic or debate"
                        ))
                    })?;
                    mode_provided = true;
                }
                "--proposition" => proposition = Some(next_arg(&mut args, "--proposition")?),
                "--agree-seed" => agree_seed = Some(next_arg(&mut args, "--agree-seed")?),
                "--disagree-seed" => disagree_seed = Some(next_arg(&mut args, "--disagree-seed")?),
                "--strategy" => {
                    strategy = parse_strategy(&next_arg(&mut args, "--strategy")?)?;
                    strategy_provided = true;
                    llm_backend = env_llm_backend();
                }
                "--help" | "-h" => return Err(QuizdomError::Usage(usage())),
                other if command == SessionCommand::Resume && !other.starts_with('-') => {
                    session_id = normalize_session_id(other);
                    session_id_provided = true;
                }
                other => {
                    return Err(QuizdomError::Usage(format!(
                        "unknown argument: {other}\n{}",
                        usage()
                    )))
                }
            }
        }

        let log_path = log_path.unwrap_or_else(|| session_log_path(&user_id, &session_id));

        Ok(Self {
            command,
            seed,
            user_id,
            session_id,
            session_id_provided,
            log_path,
            log_path_provided,
            branch_id,
            proposition,
            agree_seed,
            disagree_seed,
            strategy,
            strategy_provided,
            llm_backend,
            goal,
            mode,
            mode_provided,
        })
    }
}

// trace:STORY-80 | ai:claude
// Every session-end path prints the session id plus the exact command to get
// back in, so a finished session is never a dead end. BUG-71 restores the
// strategy/backend on resume, so the resume command needs no `--strategy` flag
// — the bare `quizdom session resume <id>` suffices.
//
// `preface` is the optional path-specific reason that carries information the
// id line does not (e.g. "No follow-up questions."); the plain user-quit paths
// pass `None` because "Session <id> ended." already says everything.
fn render_session_end(
    preface: Option<&str>,
    session_id: &str,
    output: &mut impl Write,
) -> Result<()> {
    if let Some(preface) = preface {
        writeln!(output, "{preface}")?;
    }
    writeln!(output, "Session {session_id} ended.")?;
    writeln!(output, "Resume:  quizdom session resume {session_id}")?;
    Ok(())
}

pub(crate) fn normalize_session_id(value: &str) -> String {
    // trace:BUG-70 | ai:codex
    if value.starts_with("sess-") {
        value.to_string()
    } else {
        format!("sess-{value}")
    }
}

fn env_strategy() -> StrategyKind {
    std::env::var("QUIZDOM_STRATEGY")
        .ok()
        .and_then(|value| parse_strategy(&value).ok())
        .unwrap_or(StrategyKind::Deterministic)
}

pub(crate) fn parse_strategy(value: &str) -> Result<StrategyKind> {
    match value {
        "deterministic" => Ok(StrategyKind::Deterministic),
        // trace:STORY-67 | ai:claude
        "weighted" => Ok(StrategyKind::Weighted),
        "llm" => Ok(StrategyKind::Llm),
        other => Err(QuizdomError::Usage(format!(
            "unknown strategy: {other}; expected deterministic, weighted, or llm"
        ))),
    }
}

impl StrategyKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Deterministic => "deterministic",
            Self::Weighted => "weighted",
            Self::Llm => "llm",
        }
    }
}

fn env_llm_backend() -> LlmBackendKind {
    std::env::var("QUIZDOM_BACKEND")
        .ok()
        .and_then(|value| parse_llm_backend(&value).ok())
        .unwrap_or(LlmBackendKind::ClaudeCli)
}

pub(crate) fn parse_llm_backend(value: &str) -> Result<LlmBackendKind> {
    match value {
        "claude-cli" | "claude_cli" | "claude" => Ok(LlmBackendKind::ClaudeCli),
        "anthropic" => Ok(LlmBackendKind::Anthropic),
        other => Err(QuizdomError::Usage(format!(
            "unknown LLM backend: {other}; expected claude-cli or anthropic"
        ))),
    }
}

impl LlmBackendKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCli => "claude-cli",
            Self::Anthropic => "anthropic",
        }
    }
}

fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| QuizdomError::Usage(format!("{name} requires a value")))
}

fn usage() -> String {
    [
        "usage: quizdom session <command> [options]",
        "",
        "Commands:",
        "  start                 Start a new session",
        "  resume [session-id]   Resume a session; omit session-id to resume latest",
        "  list                  List saved sessions for a user",
        "  show <session-id>     Pretty-print a saved session's full transcript",
        "  fork                  Fork a proposition into agree/disagree branches",
        "",
        "Options:",
        "  --seed Q-23                         Seed question for start",
        "  --goal text                         Goal/thesis to orient the session",
        "  --mode socratic|debate              Questioning mode (debate steelmans the opposing side)",
        "  --branch main                       Session branch to read/write",
        "  --strategy deterministic|weighted|llm  Follow-up selection strategy",
        "  --user local-user                   User id for session logs",
        "  --session sess-id                   Session id alias for resume",
        "  --log path                          Session log path",
        "  --proposition text                  Proposition to fork",
        "  --agree-seed Q --disagree-seed Q    Fork branch seed questions",
        "  -h, --help                          Show this help",
        "",
        "Examples:",
        "  quizdom session resume sess-1780256438",
        "  quizdom session resume 1780256438",
        "  quizdom session resume",
    ]
    .join("\n")
}

pub fn run_cli(
    args: impl IntoIterator<Item = String>,
    input: impl Read,
    mut output: impl Write,
) -> Result<()> {
    // trace:STORY-76 | ai:claude — gate styled output on a real TTY + NO_COLOR.
    crate::style::init_from_env();
    let config = CliConfig::parse(args)?;
    let bank = AidaCliQuestionBank::default();
    let deterministic = DeterministicNextQuestionStrategy;
    match config.command {
        SessionCommand::Start => match build_strategy(&config) {
            Some(strategy) => run_session_with_term_persister(
                &config,
                &bank,
                strategy.as_ref(),
                &AidaCliUserSpecificTermPersister::default(),
                input,
                &mut output,
            ),
            None => run_session_with_term_persister(
                &config,
                &bank,
                &deterministic,
                &AidaCliUserSpecificTermPersister::default(),
                input,
                &mut output,
            ),
        },
        SessionCommand::Resume => {
            let config = resolve_resume_config(config)?;
            match build_strategy(&config) {
                Some(strategy) => resume_session_with_term_persister(
                    &config,
                    &bank,
                    strategy.as_ref(),
                    &AidaCliUserSpecificTermPersister::default(),
                    input,
                    &mut output,
                ),
                None => resume_session_with_term_persister(
                    &config,
                    &bank,
                    &deterministic,
                    &AidaCliUserSpecificTermPersister::default(),
                    input,
                    &mut output,
                ),
            }
        }
        SessionCommand::List => list_sessions(&config, &mut output),
        SessionCommand::Fork => fork_session(&config, &mut output),
    }
}

pub(crate) fn resolve_resume_config(mut config: CliConfig) -> Result<CliConfig> {
    // trace:STORY-65 | ai:codex
    // trace:STORY-82 | ai:claude
    if !config.session_id_provided && !config.log_path_provided {
        // Bare resume targets the newest session that is NOT currently active.
        // With several explorations possibly running at once, attaching to the
        // most-recent-overall could collide with a live process; skip any
        // session whose active marker names a live PID.
        let dir = session_log_dir(&config.user_id);
        let summaries = session_summaries(&dir)?;
        if summaries.is_empty() {
            return Err(QuizdomError::Usage(format!(
                "no sessions found for user {}",
                config.user_id
            )));
        }
        let summary = summaries
            .into_iter()
            .find(|summary| !session_is_active(&summary.path))
            .ok_or_else(|| {
                QuizdomError::Usage(format!(
                    "no resumable sessions for user {} (all are currently active)",
                    config.user_id
                ))
            })?;
        config.session_id = summary.session_id;
        config.log_path = summary.path;
    } else if session_is_active(&config.log_path) {
        // Explicit resume of a live session would double-attach two processes
        // to one log; refuse. A stale marker (dead PID) is not active, so a
        // crashed session remains explicitly resumable.
        return Err(QuizdomError::Usage(format!(
            "session {} is currently active; refusing to resume an in-use session",
            config.session_id
        )));
    }
    // trace:BUG-71 | ai:codex
    if !config.strategy_provided {
        if let Some(metadata) = SessionStrategyMetadata::load(&config.log_path, &config.branch_id)?
        {
            config.strategy = metadata.strategy;
            if let Some(llm_backend) = metadata.llm_backend {
                config.llm_backend = llm_backend;
            }
            // trace:STORY-159 | ai:claude — restore the goal so a resumed session
            // keeps orienting toward the same thesis. An explicit `--goal` on the
            // resume command still wins (handled below).
            if config.goal.is_none() {
                config.goal = metadata.goal;
            }
            // trace:STORY-161 | ai:claude — restore the mode so a resumed session
            // keeps the same questioning style (debate stays debate). An explicit
            // `--mode` on the resume command wins (it overrides the default before
            // restore, so only fall back when the user did not pass one).
            if !config.mode_provided {
                config.mode = metadata.mode;
            }
        }
    }
    Ok(config)
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct SessionStrategyMetadata {
    strategy: StrategyKind,
    llm_backend: Option<LlmBackendKind>,
    // trace:STORY-159 | ai:claude — the goal set at start, restored on resume so
    // a resumed session keeps orienting toward the same thesis without re-passing
    // `--goal`. The most recent `goal_set` event (if any) overrides this.
    goal: Option<String>,
    // trace:STORY-161 | ai:claude — the mode set at start, restored on resume so a
    // resumed session keeps the same questioning style. The most recent `mode_set`
    // event (if any) overrides this.
    mode: SessionMode,
}

impl SessionStrategyMetadata {
    fn load(path: &Path, branch_id: &str) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let file = File::open(path)?;
        Self::from_reader(file, branch_id)
    }

    fn from_reader(reader: impl Read, branch_id: &str) -> Result<Option<Self>> {
        let reader = BufReader::new(reader);
        // trace:STORY-159 | ai:claude — the strategy/backend come from the start
        // event, but the goal can be UPDATED in-session (a `goal_set` event), so
        // the whole branch is scanned and the most recent goal wins. The start
        // event still gates whether any metadata is returned at all.
        let mut started: Option<Self> = None;
        let mut latest_goal: Option<String> = None;
        // trace:STORY-161 | ai:claude — the most recent mode (start or `mode_set`)
        // wins, mirroring the goal restore.
        let mut latest_mode: Option<SessionMode> = None;
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&line)
                .map_err(|error| QuizdomError::Parse(error.to_string()))?;
            if event_branch(&value) != branch_id {
                continue;
            }
            let goal_field = value
                .get("goal")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|goal| !goal.is_empty())
                .map(str::to_string);
            // trace:STORY-161 | ai:claude — an unrecognized mode token is ignored,
            // leaving the default Socratic.
            let mode_field = value
                .get("mode")
                .and_then(Value::as_str)
                .and_then(SessionMode::parse);
            match value.get("event_type").and_then(Value::as_str) {
                Some("session_started") => {
                    let Some(strategy_value) = value.get("strategy").and_then(Value::as_str) else {
                        return Ok(None);
                    };
                    let strategy = parse_strategy(strategy_value)?;
                    let llm_backend = value
                        .get("llm_backend")
                        .and_then(Value::as_str)
                        .map(parse_llm_backend)
                        .transpose()?;
                    if let Some(goal) = goal_field.clone() {
                        latest_goal = Some(goal);
                    }
                    if let Some(mode) = mode_field {
                        latest_mode = Some(mode);
                    }
                    started = Some(Self {
                        strategy,
                        llm_backend,
                        goal: None,
                        mode: SessionMode::default(),
                    });
                }
                // trace:STORY-159 | ai:claude — an in-session / Observer-proposed
                // goal logged after start; the most recent one wins.
                Some("goal_set") => {
                    if let Some(goal) = goal_field {
                        latest_goal = Some(goal);
                    }
                }
                // trace:STORY-161 | ai:claude — an in-session mode toggle logged
                // after start; the most recent one wins.
                Some("mode_set") => {
                    if let Some(mode) = mode_field {
                        latest_mode = Some(mode);
                    }
                }
                _ => {}
            }
        }
        if let Some(mut metadata) = started {
            metadata.goal = latest_goal;
            metadata.mode = latest_mode.unwrap_or_default();
            return Ok(Some(metadata));
        }
        Ok(None)
    }
}

fn llm_model_for_log(backend: LlmBackendKind) -> Option<String> {
    match backend {
        LlmBackendKind::ClaudeCli => std::env::var("QUIZDOM_MODEL").ok(),
        LlmBackendKind::Anthropic => {
            Some(std::env::var("QUIZDOM_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".to_string()))
        }
    }
}

fn session_log_dir(user_id: &str) -> PathBuf {
    PathBuf::from("data")
        .join("users")
        .join(user_id)
        .join("sessions")
}

fn session_log_path(user_id: &str, session_id: &str) -> PathBuf {
    session_log_dir(user_id).join(format!("{session_id}.jsonl"))
}

// trace:STORY-82 | ai:claude
// Liveness marker for a running session. Sessions are JSONL logs with no
// inherent process-liveness signal, so we track "active" out-of-band: a
// `<session>.active` file holding the owning PID sits next to the log. The
// marker is written on session start and removed on clean end; a marker left
// behind by a dead process is STALE and treated as resumable.
fn session_active_marker_path(log_path: &Path) -> PathBuf {
    log_path.with_extension("active")
}

// A session is active iff its marker exists AND records a live PID. A missing
// marker, an unparseable one, or a marker naming a dead PID (stale, e.g. from a
// crashed/killed process) all count as inactive — i.e. safe to resume.
fn session_is_active(log_path: &Path) -> bool {
    match fs::read_to_string(session_active_marker_path(log_path)) {
        Ok(contents) => contents
            .trim()
            .parse::<u32>()
            .map(process_is_alive)
            .unwrap_or(false),
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
fn process_is_alive(pid: u32) -> bool {
    pid != 0 && Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(target_os = "linux"))]
fn process_is_alive(pid: u32) -> bool {
    // Non-Linux fallback: without /proc we cannot cheaply probe liveness, so we
    // assume the recorded PID is live (conservative — never silently double-
    // attaches). The marker is still cleared on clean end via the RAII guard.
    pid != 0
}

// RAII guard that publishes the active marker on session start and clears it on
// clean end (any return or unwind). A SIGKILL leaves the marker behind, which
// `session_is_active` then recognises as stale via the dead PID.
struct SessionActiveGuard {
    marker_path: PathBuf,
}

impl SessionActiveGuard {
    fn acquire(log_path: &Path) -> Result<Self> {
        let marker_path = session_active_marker_path(log_path);
        if let Some(parent) = marker_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&marker_path, std::process::id().to_string())?;
        Ok(Self { marker_path })
    }
}

impl Drop for SessionActiveGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.marker_path);
    }
}

pub(crate) fn list_sessions(config: &CliConfig, output: &mut impl Write) -> Result<()> {
    let summaries = session_summaries(&session_log_dir(&config.user_id))?;
    writeln!(output, "Sessions for user {}:", config.user_id)?;
    if summaries.is_empty() {
        writeln!(output, "(none)")?;
        return Ok(());
    }
    writeln!(
        output,
        "SESSION\tSTARTED\tLAST_ACTIVE\tBRANCH\tLAST_ANSWERED"
    )?;
    for summary in summaries {
        writeln!(
            output,
            "{}\t{}\t{}\t{}\t{}",
            summary.session_id,
            summary.started_at.unwrap_or_else(|| "-".to_string()),
            summary.last_active_at.unwrap_or_else(|| "-".to_string()),
            summary.branch_id.unwrap_or_else(|| "-".to_string()),
            summary
                .last_question_answered
                .unwrap_or_else(|| "(no answers)".to_string())
        )?;
    }
    Ok(())
}

fn build_strategy(config: &CliConfig) -> Option<Box<dyn NextQuestionStrategy>> {
    match config.strategy {
        StrategyKind::Deterministic => None,
        // trace:STORY-67 | ai:claude
        StrategyKind::Weighted => {
            Some(Box::new(WeightedNextQuestionStrategy::from_entropy())
                as Box<dyn NextQuestionStrategy>)
        }
        StrategyKind::Llm => match config.llm_backend {
            LlmBackendKind::ClaudeCli => {
                let client = ClaudeCliClient::from_env();
                Some(
                    Box::new(LlmNextQuestionStrategy::with_generated_question_persister(
                        client,
                        AidaCliGeneratedQuestionPersister::default(),
                    )) as Box<dyn NextQuestionStrategy>,
                )
            }
            LlmBackendKind::Anthropic => AnthropicClient::from_env().ok().map(|client| {
                Box::new(LlmNextQuestionStrategy::with_generated_question_persister(
                    client,
                    AidaCliGeneratedQuestionPersister::default(),
                )) as Box<dyn NextQuestionStrategy>
            }),
        },
    }
}

// trace:STORY-127 | ai:claude
/// The in-session Observer: produces a belief-neutral reading of an exchange
/// when the `?` key is pressed. Holds the LLM backend (default claude-cli) when
/// one is available, and degrades to a pure structural note otherwise — so the
/// `?` key always does something, online or off.
enum ObserverEngine {
    ClaudeCli(ClaudeCliClient),
    Anthropic(AnthropicClient),
    /// No backend configured / available — structural-only readings.
    Offline,
}

impl ObserverEngine {
    /// Build the observer for a session from its configured LLM backend.
    ///
    /// The default backend is claude-cli (always constructible); the Anthropic
    /// backend degrades to `Offline` when its API key is absent, so an offline
    /// machine still gets the structural note rather than a failure.
    fn for_config(config: &CliConfig) -> Self {
        match config.llm_backend {
            LlmBackendKind::ClaudeCli => Self::ClaudeCli(ClaudeCliClient::from_env()),
            LlmBackendKind::Anthropic => match AnthropicClient::from_env() {
                Ok(client) => Self::Anthropic(client),
                Err(_) => Self::Offline,
            },
        }
    }

    /// Read an exchange, using the LLM when present and falling back to the
    /// structural note when offline.
    fn read(&self, exchange: &Exchange) -> ExchangeReading {
        match self {
            Self::ClaudeCli(client) => read_exchange(client, exchange),
            Self::Anthropic(client) => read_exchange(client, exchange),
            Self::Offline => structural_reading(exchange),
        }
    }

    // trace:STORY-128 | ai:claude
    /// Read a whole-session arc, using the LLM when present and falling back to
    /// the structural summary when offline. The GLOBAL counterpart to [`read`].
    fn synopsize(&self, arc: &SessionArc) -> SessionSynopsis {
        match self {
            Self::ClaudeCli(client) => synopsize(client, arc),
            Self::Anthropic(client) => synopsize(client, arc),
            Self::Offline => structural_synopsis(arc),
        }
    }

    // trace:STORY-159 | ai:claude
    /// Ask the Observer whether a thesis has crystallized into a proposable
    /// session goal, from the `positions` recorded so far. Returns `None` offline
    /// (no LLM to detect a crystallized thesis) — the session stays free-flowing
    /// rather than fabricating a goal. The GOAL-proposal counterpart to [`read`].
    fn propose_goal(&self, positions: &[String]) -> Option<crate::observer::GoalProposal> {
        match self {
            Self::ClaudeCli(client) => crate::observer::propose_goal(client, positions),
            Self::Anthropic(client) => crate::observer::propose_goal(client, positions),
            Self::Offline => None,
        }
    }

    // trace:STORY-160 | ai:claude
    /// The challenger's CLOSING statement: its strongest remaining structural
    /// objection to the user's just-rested position, oriented by the goal. Uses
    /// the LLM when present and falls back to the structural objection offline, so
    /// the closing ritual always has a challenger turn. The CLOSING-PHASE
    /// counterpart to [`read`]. Belief-neutral: it presses on the structure of the
    /// case, never asserting a belief is true.
    fn closing_objection(
        &self,
        position: &str,
        goal: Option<&str>,
    ) -> crate::observer::ClosingObjection {
        match self {
            Self::ClaudeCli(client) => {
                crate::observer::read_closing_objection(client, position, goal)
            }
            Self::Anthropic(client) => {
                crate::observer::read_closing_objection(client, position, goal)
            }
            Self::Offline => crate::observer::structural_objection(position, goal),
        }
    }
}

// trace:STORY-128 | ai:claude
/// Build and render a belief-neutral synopsis of the WHOLE session so far when
/// the `S` key is pressed in-session. Reads the live JSONL log the session is
/// writing to (the logger flushes after every event, so it is current),
/// summarizes the arc through the observer engine, and renders it as the same
/// META voice the per-exchange observer uses. Non-destructive: the caller
/// re-presents the SAME question afterwards, so this only reads + writes.
///
/// A log that cannot be read yet (e.g. the first turn, before anything is
/// flushed) degrades to a short note rather than failing the keypress.
fn render_session_synopsis(
    observer: &ObserverEngine,
    log_path: &Path,
    branch: Option<&str>,
    output: &mut impl Write,
) -> Result<Option<(SessionSynopsis, SessionArc)>> {
    let arc = match File::open(log_path) {
        Ok(file) => arc_from_session_log(file, branch).unwrap_or_default(),
        Err(_) => SessionArc::default(),
    };
    if arc.is_empty() {
        writeln!(
            output,
            "{}",
            crate::style::paint(
                crate::style::meta(),
                "\nMETA (synopsis) — nothing recorded yet to summarize."
            )
        )?;
        return Ok(None);
    }
    let synopsis = {
        let _spinner = crate::spinner::Spinner::start("synopsizing");
        observer.synopsize(&arc)
    };
    render_synopsis(&synopsis, output)?;
    // trace:STORY-156 | ai:claude — hand the synopsis + arc back so the caller
    // can offer the conclude path when the score crossed the well-rounded
    // threshold. Callers that don't conclude (the dead-end menu, the review
    // pane) simply ignore the return.
    Ok(Some((synopsis, arc)))
}

// trace:STORY-156 | ai:claude
/// Prompt the user to accept the offer to conclude (printed by the synopsis when
/// the position crossed the well-rounded threshold). Returns `true` to conclude,
/// `false` to keep exploring.
///
/// Agency is preserved: the DEFAULT is to keep exploring. Only an explicit
/// affirmative (`c`/`conclude`/`y`/`yes`) concludes; anything else — including a
/// blank line, `keep`, or `no` — keeps probing. Degrades gracefully on a
/// non-TTY / EOF prompt (no answer): treated as "keep exploring", so a piped or
/// offline run never gets stuck or auto-concludes against the user's wishes.
fn prompt_to_conclude(
    input: &mut impl BufRead,
    free_text_input: &mut FreeTextInput,
    output: &mut impl Write,
) -> Result<bool> {
    let prompt = "Conclude with a summary? [c]onclude / [k]eep exploring (default keep): ";
    let choice = match free_text_input.read_line(input, output, prompt)? {
        Some(line) => line.trim().to_ascii_lowercase(),
        // EOF / non-TTY: do not conclude on the user's behalf.
        None => return Ok(false),
    };
    Ok(matches!(choice.as_str(), "c" | "conclude" | "y" | "yes"))
}

// trace:STORY-159 | ai:claude
/// Apply an in-session goal command. A non-empty `text` SETS the live goal and
/// logs a `goal_set` event (so resume restores it and the arc/synopsis orient to
/// it); a bare `/goal` (empty `text`) just SHOWS the current goal without
/// changing it — a goal is never cleared, only replaced. `source` records who
/// set it (`"user"` for the command, `"observer"` for an accepted proposal).
/// Belief-neutral throughout: the goal is the question being settled.
fn set_goal_in_session(
    goal: &mut Option<String>,
    text: &str,
    source: &str,
    config: &CliConfig,
    logger: &mut SessionLogger,
    turn: u64,
    output: &mut impl Write,
) -> Result<()> {
    let text = text.trim();
    if text.is_empty() {
        match goal.as_deref() {
            Some(current) => writeln!(output, "Current goal: {current}")?,
            None => writeln!(
                output,
                "No goal set yet — state one with `/goal <the question you're resolving>`."
            )?,
        }
        return Ok(());
    }
    *goal = Some(text.to_string());
    logger.goal_set(
        &config.session_id,
        &config.user_id,
        &config.branch_id,
        turn,
        text,
        source,
    )?;
    writeln!(
        output,
        "Goal set: {text}\n(Questions and the roundedness score now orient toward resolving it.)"
    )?;
    Ok(())
}

// trace:STORY-161 | ai:claude
/// Apply an in-session `/mode` toggle. A non-empty `token` SETS the live mode and
/// logs a `mode_set` event (so resume restores it and the verdict path frames the
/// debate); a bare `/mode` (empty `token`) just SHOWS the current mode without
/// changing it. An unrecognized token is reported and leaves the mode unchanged
/// (the session never silently falls back). Belief-neutral throughout: debate
/// steelmans the OPPOSING side's CRAFT, never asserting which belief is true.
fn set_mode_in_session(
    mode: &mut SessionMode,
    token: &str,
    config: &CliConfig,
    logger: &mut SessionLogger,
    turn: u64,
    output: &mut impl Write,
) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        writeln!(output, "Current mode: {}", mode.as_str())?;
        return Ok(());
    }
    let Some(new_mode) = SessionMode::parse(token) else {
        writeln!(
            output,
            "Unknown mode: {token} (expected socratic or debate). Mode unchanged ({}).",
            mode.as_str()
        )?;
        return Ok(());
    };
    *mode = new_mode;
    logger.mode_set(
        &config.session_id,
        &config.user_id,
        &config.branch_id,
        turn,
        new_mode,
    )?;
    let note = match new_mode {
        SessionMode::Debate => "(The questioner now steelmans the OPPOSING side; the verdict will judge which CASE was better-argued — never which belief is true.)",
        SessionMode::Socratic => "(The questioner is again a neutral challenger of your OWN position.)",
    };
    writeln!(output, "Mode set: {}\n{note}", new_mode.as_str())?;
    Ok(())
}

// trace:STORY-159 | ai:claude
/// When no goal is set yet, ask the Observer whether a thesis has crystallized
/// and, if so, OFFER it as the session goal. The user decides (agency: the
/// default is to keep exploring free-flowing). Accepting sets the goal exactly as
/// the `/goal` command would (logged `source:"observer"`). Degrades gracefully:
/// offline / no crystallized thesis / EOF prompt → no goal is set, the session
/// stays free-flowing. Belief-neutral: the proposed goal is a QUESTION to settle,
/// never a belief to adopt.
#[allow(clippy::too_many_arguments)]
fn maybe_propose_goal(
    goal: &mut Option<String>,
    observer: &ObserverEngine,
    arc: &SessionArc,
    config: &CliConfig,
    logger: &mut SessionLogger,
    turn: u64,
    input: &mut impl BufRead,
    free_text_input: &mut FreeTextInput,
    output: &mut impl Write,
) -> Result<()> {
    // Only propose when free-flowing — a session that already has a goal does not
    // get nagged with another.
    if goal.is_some() {
        return Ok(());
    }
    let positions: Vec<String> = arc
        .turns
        .iter()
        .filter(|turn| !turn.position.is_empty())
        .map(|turn| {
            if turn.question.is_empty() {
                turn.position.clone()
            } else {
                format!("On \"{}\": {}", turn.question, turn.position)
            }
        })
        .collect();
    let proposal = {
        let _spinner = crate::spinner::Spinner::start("reading for a thesis");
        observer.propose_goal(&positions)
    };
    let Some(proposal) = proposal else {
        return Ok(());
    };
    writeln!(
        output,
        "\n{}",
        crate::style::paint(
            crate::style::meta(),
            &format!(
                "META (observer) — it sounds like you're trying to settle: {}",
                proposal.goal
            )
        )
    )?;
    if !proposal.rationale.trim().is_empty() {
        writeln!(
            output,
            "{}",
            crate::style::paint(
                crate::style::meta(),
                &format!("  Why: {}", proposal.rationale.trim())
            )
        )?;
    }
    let prompt = "Make that the session goal? [y]es / [k]eep exploring (default keep): ";
    let accepted = match free_text_input.read_line(input, output, prompt)? {
        Some(line) => matches!(
            line.trim().to_ascii_lowercase().as_str(),
            "y" | "yes" | "g" | "goal"
        ),
        // EOF / non-TTY: never set a goal on the user's behalf.
        None => false,
    };
    if accepted {
        set_goal_in_session(
            goal,
            &proposal.goal,
            "observer",
            config,
            logger,
            turn,
            output,
        )?;
    }
    Ok(())
}

// trace:STORY-160 | ai:claude
/// Which party can call "terminate" in the closing ritual. Belief-neutral: this
/// is about WHO speaks last, never which belief is right.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ClosingParty {
    /// The person whose case is being rested.
    User,
    /// The neutral challenger pressing the strongest remaining objection.
    Challenger,
}

impl ClosingParty {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Challenger => "challenger",
        }
    }
}

// trace:STORY-160 | ai:claude
/// The FAIRNESS RULE, distilled: the party that calls "terminate" does NOT get
/// the last word (except to say "terminate") — the OTHER side makes the final
/// closing statement first. So the final word goes to whoever did NOT terminate.
/// Pure + total, so the rule itself is unit-testable for both terminators.
fn final_word_speaker(terminator: ClosingParty) -> ClosingParty {
    match terminator {
        ClosingParty::User => ClosingParty::Challenger,
        ClosingParty::Challenger => ClosingParty::User,
    }
}

// trace:STORY-160 | ai:claude
/// The outcome of running the closing ritual: the session always ENDS after the
/// ritual (a rested case either reaches a verdict or is terminated), so the
/// caller breaks out of the main loop. Carries the end summary for the log.
struct ClosingOutcome {
    summary: &'static str,
}

// trace:STORY-160 | ai:claude
/// Run the CLOSING RITUAL after either party rests their case (STORY-160).
///
/// A PHASE TRANSITION: the question/answer loop is over. Here the exchange is
/// CLOSING STATEMENTS — the user states their final/settled position, and the
/// challenger answers with its strongest remaining (structural) OBJECTION —
/// back and forth — until someone requests a FINAL VERDICT (`verdict`) or calls
/// `terminate`.
///
/// FAIRNESS RULE: the party that calls `terminate` forfeits the last word. When
/// the USER terminates, the CHALLENGER makes the final closing statement
/// (objection) before the verdict; when the CHALLENGER terminates (it rests with
/// no remaining objection), the USER makes the final closing statement first.
///
/// Belief-neutral throughout: the closing statements and the verdict assess the
/// STRUCTURE of the case (consistency / clarity / completeness / coherence) and
/// roundedness w.r.t. the goal — never which belief is true. Degrades gracefully
/// offline (the challenger's objection and the verdict both fall back to
/// structural notes) and on a non-TTY / EOF prompt (treated as a request for the
/// verdict, so a piped run renders the verdict rather than hanging).
#[allow(clippy::too_many_arguments)]
fn run_closing_phase(
    config: &CliConfig,
    observer: &ObserverEngine,
    goal: Option<&str>,
    rester: ClosingParty,
    logger: &mut SessionLogger,
    turn: u64,
    input: &mut impl BufRead,
    free_text_input: &mut FreeTextInput,
    output: &mut impl Write,
) -> Result<ClosingOutcome> {
    logger.phase_changed(
        &config.session_id,
        &config.user_id,
        &config.branch_id,
        turn,
        "closing",
        rester.as_str(),
    )?;
    render_closing_banner(output)?;

    // The most recent settled position the user stated, fed to the challenger so
    // its objection presses on THIS case. Empty until the user makes a statement.
    let mut last_position = String::new();

    loop {
        // The user makes (the next) closing statement: their final / settled
        // position. They can instead request the verdict or call terminate.
        render_closing_user_prompt(output)?;
        let line = free_text_input.read_line(input, output, "> ")?;
        let raw = match line {
            Some(raw) => raw,
            // EOF / non-TTY: do not hang. Render the verdict on what we have.
            None => {
                return finish_with_verdict(config, observer, goal, output);
            }
        };
        if crate::input::is_verdict_command(&raw) {
            // A direct request for the FINAL VERDICT — no terminator, no forfeited
            // last word; render the belief-neutral assessment and end.
            return finish_with_verdict(config, observer, goal, output);
        }
        if crate::input::is_terminate_command(&raw) {
            // The USER terminates: the fairness rule gives the CHALLENGER the final
            // word (its strongest remaining objection) before the verdict. The user
            // does NOT get to add another statement.
            let final_speaker = final_word_speaker(ClosingParty::User);
            debug_assert_eq!(final_speaker, ClosingParty::Challenger);
            render_terminate_note(ClosingParty::User, output)?;
            let objection = {
                let _spinner = crate::spinner::Spinner::start("closing objection");
                observer.closing_objection(&last_position, goal)
            };
            render_closing_objection(&objection, true, output)?;
            logger.closing_statement(
                &config.session_id,
                &config.user_id,
                &config.branch_id,
                turn,
                ClosingParty::Challenger.as_str(),
                &objection.objection,
                true,
            )?;
            return finish_with_verdict(config, observer, goal, output);
        }
        if crate::input::is_end_command(&raw) {
            // A plain quit during the closing ritual: end without a verdict (the
            // user can resume and rest again). Belief-neutral: nothing is judged.
            return Ok(ClosingOutcome {
                summary: "User quit during the closing ritual.",
            });
        }
        // An ordinary line is the user's closing STATEMENT (their settled
        // position). Record it and let the challenger answer with its objection.
        let statement = raw.trim();
        if statement.is_empty() {
            continue;
        }
        last_position = statement.to_string();
        render_recorded_user_statement(statement, output)?;
        logger.closing_statement(
            &config.session_id,
            &config.user_id,
            &config.branch_id,
            turn,
            ClosingParty::User.as_str(),
            statement,
            false,
        )?;
        let objection = {
            let _spinner = crate::spinner::Spinner::start("closing objection");
            observer.closing_objection(&last_position, goal)
        };
        render_closing_objection(&objection, false, output)?;
        logger.closing_statement(
            &config.session_id,
            &config.user_id,
            &config.branch_id,
            turn,
            ClosingParty::Challenger.as_str(),
            &objection.objection,
            false,
        )?;
        // Loop: the user gets to answer the objection with another closing
        // statement, or request the verdict / terminate.
    }
}

// trace:STORY-160 | ai:claude
/// Render the verdict and end the closing ritual. The verdict is the
/// belief-neutral roundedness assessment (EPIC-154) measured w.r.t. the goal:
/// it reuses the same synopsis engine the `S` key uses, so the closing verdict
/// and the in-session synopsis speak with one voice. Returns the end outcome.
fn finish_with_verdict(
    config: &CliConfig,
    observer: &ObserverEngine,
    goal: Option<&str>,
    output: &mut impl Write,
) -> Result<ClosingOutcome> {
    render_verdict(
        observer,
        &config.log_path,
        Some(&config.branch_id),
        goal,
        output,
    )?;
    Ok(ClosingOutcome {
        summary: "Closing ritual reached a final verdict.",
    })
}

// trace:STORY-160 | ai:claude
/// Render the FINAL VERDICT: the belief-neutral roundedness assessment of the
/// rested case, measured w.r.t. the goal. Reuses [`arc_from_session_log`] +
/// the observer synopsis (so the goal in the log orients the score) and the
/// existing [`render_synopsis`] block. Adds a verdict header that pins the
/// belief-neutral contract: it assesses STRUCTURE (and, where relevant, which
/// CASE was better-argued), NEVER which belief is true.
fn render_verdict(
    observer: &ObserverEngine,
    log_path: &Path,
    branch: Option<&str>,
    goal: Option<&str>,
    output: &mut impl Write,
) -> Result<()> {
    let meta = crate::style::meta();
    let arc = match File::open(log_path) {
        Ok(file) => arc_from_session_log(file, branch).unwrap_or_default(),
        Err(_) => SessionArc::default(),
    };
    // trace:STORY-161 | ai:claude — the verdict header is mode-aware: in DEBATE
    // mode it pins the WHICH-CASE-WAS-BETTER-ARGUED contract (argument STRUCTURE),
    // in Socratic it assesses the user's OWN case. Belief-neutral in both — never
    // which belief is true. The mode is read from the log (the arc), so a resumed
    // debate session renders the debate verdict.
    let header = match arc.mode {
        SessionMode::Debate => {
            "META (final verdict) — debate mode: which CASE was better-ARGUED (the argument STRUCTURE of each side: consistency / clarity / completeness / coherence), NOT which belief is true:"
        }
        SessionMode::Socratic => {
            "META (final verdict) — a belief-neutral assessment of your case's STRUCTURE (consistency / clarity / completeness / coherence), NOT whether your belief is true:"
        }
    };
    writeln!(output, "\n{}", crate::style::paint(meta, header))?;
    if let Some(goal) = goal.map(str::trim).filter(|g| !g.is_empty()) {
        writeln!(
            output,
            "{}",
            crate::style::paint(meta, &format!("  Resolving: {goal}"))
        )?;
    }
    if arc.is_empty() {
        writeln!(
            output,
            "{}",
            crate::style::paint(
                meta,
                "  Nothing recorded yet to assess — rest a case with at least one position first."
            )
        )?;
        return Ok(());
    }
    let synopsis = {
        let _spinner = crate::spinner::Spinner::start("rendering the verdict");
        observer.synopsize(&arc)
    };
    render_synopsis(&synopsis, output)?;
    Ok(())
}

// trace:STORY-160 | ai:claude
/// The closing-phase opening banner: announce the PHASE TRANSITION so the user
/// knows the exchange is now closing statements, not questions.
fn render_closing_banner(output: &mut impl Write) -> Result<()> {
    writeln!(
        output,
        "\n{}",
        crate::style::paint(
            crate::style::meta(),
            "META (closing) — case rested. The questioning is over; this is the closing ritual.\n  Make your closing statements (your final, settled position). The challenger answers each with its strongest remaining objection.\n  Type `verdict` for the final belief-neutral assessment, or `terminate` to end (the terminator forfeits the last word)."
        )
    )?;
    Ok(())
}

// trace:STORY-160 | ai:claude
/// Prompt the user for their next closing statement.
fn render_closing_user_prompt(output: &mut impl Write) -> Result<()> {
    writeln!(
        output,
        "{}",
        crate::style::paint(
            crate::style::control(),
            "Your closing statement (or `verdict` / `terminate`):"
        )
    )?;
    Ok(())
}

// trace:STORY-160 | ai:claude
/// Echo the user's recorded closing statement back as a labeled closing turn.
fn render_recorded_user_statement(statement: &str, output: &mut impl Write) -> Result<()> {
    writeln!(
        output,
        "{}",
        crate::style::paint(
            crate::style::meta(),
            &format!("  You (closing): {statement}")
        )
    )?;
    Ok(())
}

// trace:STORY-160 | ai:claude
/// Render the challenger's closing objection as a belief-neutral META voice. The
/// `final_word` flag labels the statement made under the fairness rule (the
/// other side's last word after a terminate).
fn render_closing_objection(
    objection: &crate::observer::ClosingObjection,
    final_word: bool,
    output: &mut impl Write,
) -> Result<()> {
    let meta = crate::style::meta();
    let header = match (objection.degraded, final_word) {
        (true, true) => {
            "Challenger (closing, offline) — final objection (you forfeited the last word):"
        }
        (true, false) => "Challenger (closing, offline) — strongest remaining objection:",
        (false, true) => "Challenger (closing) — final objection (you forfeited the last word):",
        (false, false) => "Challenger (closing) — strongest remaining objection:",
    };
    writeln!(
        output,
        "{}",
        crate::style::paint(meta, &format!("  {header}"))
    )?;
    writeln!(
        output,
        "{}",
        crate::style::paint(meta, &format!("    {}", objection.objection))
    )?;
    Ok(())
}

// trace:STORY-160 | ai:claude
/// Note that a party terminated, naming the fairness rule before the other side
/// makes its final closing statement.
fn render_terminate_note(terminator: ClosingParty, output: &mut impl Write) -> Result<()> {
    let other = final_word_speaker(terminator);
    let body = match (terminator, other) {
        (ClosingParty::User, _) => {
            "You called terminate — by the fairness rule you forfeit the last word; the challenger makes the final closing statement first."
        }
        (ClosingParty::Challenger, _) => {
            "The challenger rested — by the fairness rule it forfeits the last word; you make the final closing statement first."
        }
    };
    writeln!(
        output,
        "{}",
        crate::style::paint(crate::style::meta(), &format!("  {body}"))
    )?;
    Ok(())
}

// trace:STORY-127 | ai:claude
/// Assemble the [`Exchange`] the observer reads when `?` is pressed at the
/// frontier on `current`.
///
/// The rebuttal is `current` (the question now challenging the user); the prior
/// question + answer is the most recent step on the path that led here. At the
/// seed (an empty path) there is no prior turn, so the current question stands
/// as its own framing with no answer — the structural note handles that.
fn exchange_for_frontier(current: &Question, recent_path: &[AnsweredQuestion]) -> Exchange {
    match recent_path.last() {
        Some(prior) => Exchange {
            question: prior.question_text.clone(),
            answer: prior.raw_answer.clone(),
            rebuttal: current.title.clone(),
        },
        None => Exchange {
            question: current.title.clone(),
            answer: String::new(),
            rebuttal: current.title.clone(),
        },
    }
}

// trace:STORY-127 | ai:claude
/// Render an [`ExchangeReading`] as a clearly-labeled META voice, visually
/// distinct from the question (style::meta). Belief-neutral and clarify-only:
/// it restates the rebuttal, names the tension, diagnoses the mismatch, and
/// lists the dimensions a precise answer must address — it never supplies an
/// answer. Pure over the buffer + reading, so it is unit-testable without a
/// live LLM. The caller re-presents the SAME question afterwards (non-
/// destructive), so this only writes; it never consumes input or mutates state.
fn render_exchange_reading(reading: &ExchangeReading, output: &mut impl Write) -> Result<()> {
    let header = if reading.degraded {
        "META (observer, offline) — a belief-neutral reading of this exchange:"
    } else {
        "META (observer) — a belief-neutral reading of this exchange:"
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
    line("In plainer terms", &reading.plain_rebuttal, output)?;
    line("The tension", &reading.tension, output)?;
    line("Asked vs answered", &reading.mismatch, output)?;
    if !reading.dimensions.is_empty() {
        writeln!(
            output,
            "{}",
            crate::style::paint(crate::style::meta(), "  A precise answer would address:")
        )?;
        for dimension in &reading.dimensions {
            writeln!(
                output,
                "{}",
                crate::style::paint(crate::style::meta(), &format!("    - {dimension}"))
            )?;
        }
    }
    line("Engagement", &reading.engagement, output)?;
    Ok(())
}

// trace:STORY-163 | ai:claude
/// Render the graceful placeholder for the `/help` channel.
///
/// STORY-163 wires `/help` through the palette + the command recognizer; the
/// belief-neutral, TOOL-CONTEXT LLM answer is STORY-164's job. Until that lands,
/// selecting / typing `/help` is NON-DESTRUCTIVE: it prints this note (in the
/// secondary META voice, so it never reads as the question) and the caller
/// re-presents the SAME question. The note is belief-neutral by construction —
/// it talks only about the TOOL, never about any belief — and points the user at
/// the palette's per-command `?` help, which is already live, so the channel is
/// useful even before the LLM leg ships.
fn render_help_placeholder(question: &str, output: &mut impl Write) -> Result<()> {
    let header = "META (/help) — process help (belief-neutral; about the tool, not your belief):";
    writeln!(
        output,
        "\n{}",
        crate::style::paint(crate::style::meta(), header)
    )?;
    let body = if question.trim().is_empty() {
        "Ask how the tool works — controls, the flow, what a feature does, how to rest your case. \
Free-form answers from the tool's design arrive with /help <question> (coming soon); for now, \
open the palette with '/' and press '?' on any command for its detailed help."
            .to_string()
    } else {
        format!(
            "Your question — \"{}\" — is a process question (how the tool works), and is answered \
belief-neutrally from the tool's design. The free-form /help answer is coming soon; for now, \
open the palette with '/' and press '?' on any command for its detailed help.",
            question.trim()
        )
    };
    writeln!(
        output,
        "{}",
        crate::style::paint(crate::style::meta(), &format!("  {body}"))
    )?;
    Ok(())
}

// trace:STORY-163 | ai:claude
/// Render the graceful placeholder for the `/tutor` articulation & nuance coach.
///
/// STORY-163 wires `/tutor` through the palette + recognizer; the coaching LLM
/// engine (reflect + sharpen the user's OWN point, surface the missing nuance,
/// never supply the belief) is STORY-165's job. Until that lands, selecting /
/// typing `/tutor` is NON-DESTRUCTIVE: it prints this note in the META voice and
/// the caller re-presents the SAME question. Belief-neutral by construction — it
/// promises to sharpen the user's own point and name missing nuance, and never
/// supplies a belief or takes a side.
fn render_tutor_placeholder(text: &str, output: &mut impl Write) -> Result<()> {
    let header =
        "META (/tutor) — articulation & nuance coach (sharpens YOUR point; never supplies it):";
    writeln!(
        output,
        "\n{}",
        crate::style::paint(crate::style::meta(), header)
    )?;
    let body = "/tutor reflects your own half-formed view back more precisely, teaches the \
relevant distinction, and names the nuance you have not yet addressed — without ever telling you \
what to believe. The coaching engine is coming soon; for now, use /observe for a belief-neutral \
reading of the current exchange.";
    writeln!(
        output,
        "{}",
        crate::style::paint(crate::style::meta(), &format!("  {body}"))
    )?;
    // trace:STORY-163 | ai:claude — echo back the point the user typed after
    // /tutor (when any) as the thing to be sharpened — belief-neutral: it reflects
    // the user's OWN words, never supplying a belief. STORY-165 replaces this with
    // the coaching engine that sharpens it.
    let text = text.trim();
    if !text.is_empty() {
        writeln!(
            output,
            "{}",
            crate::style::paint(
                crate::style::meta(),
                &format!("  The point you're reaching for: \"{text}\"")
            )
        )?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn run_session(
    config: &CliConfig,
    bank: &dyn QuestionBank,
    strategy: &dyn NextQuestionStrategy,
    input: impl Read,
    output: &mut impl Write,
) -> Result<()> {
    // trace:STORY-17 | ai:codex
    run_session_with_term_persister(
        config,
        bank,
        strategy,
        &NoopUserSpecificTermPersister,
        input,
        output,
    )
}

pub(crate) fn run_session_with_term_persister(
    config: &CliConfig,
    bank: &dyn QuestionBank,
    strategy: &dyn NextQuestionStrategy,
    term_persister: &dyn UserSpecificTermPersister,
    input: impl Read,
    output: &mut impl Write,
) -> Result<()> {
    let contradiction_edges = AidaCliContradictsEdges::default();
    let contradiction_resolution_persister = AidaCliContradictionResolutionPersister::default();
    let question_reweighter = AidaCliQuestionReweighter::default();
    // trace:STORY-88 | ai:claude — real persister for the in-session quick-add.
    let user_authored_persister = AidaCliUserAuthoredQuestionPersister::default();
    run_session_from_current(
        config,
        bank,
        strategy,
        term_persister,
        &contradiction_edges,
        &contradiction_resolution_persister,
        &question_reweighter,
        &user_authored_persister,
        input,
        output,
        0,
        true,
        Vec::new(),
    )
}

#[cfg(test)]
pub(crate) fn run_session_with_contradiction_edges(
    config: &CliConfig,
    bank: &dyn QuestionBank,
    strategy: &dyn NextQuestionStrategy,
    edges: &dyn ContradictsEdges,
    input: impl Read,
    output: &mut impl Write,
) -> Result<()> {
    run_session_with_contradiction_edges_and_resolution_persister(
        config,
        bank,
        strategy,
        edges,
        &crate::contradiction::NoopContradictionResolutionPersister,
        input,
        output,
    )
}

#[cfg(test)]
pub(crate) fn run_session_with_contradiction_edges_and_resolution_persister(
    config: &CliConfig,
    bank: &dyn QuestionBank,
    strategy: &dyn NextQuestionStrategy,
    edges: &dyn ContradictsEdges,
    resolution_persister: &dyn ContradictionResolutionPersister,
    input: impl Read,
    output: &mut impl Write,
) -> Result<()> {
    run_session_from_current(
        config,
        bank,
        strategy,
        &NoopUserSpecificTermPersister,
        edges,
        resolution_persister,
        &NoopQuestionReweighter,
        &NoopUserAuthoredQuestionPersister,
        input,
        output,
        0,
        true,
        Vec::new(),
    )
}

// trace:STORY-88 | ai:claude
#[cfg(test)]
pub(crate) fn run_session_with_user_authored_persister(
    config: &CliConfig,
    bank: &dyn QuestionBank,
    strategy: &dyn NextQuestionStrategy,
    user_authored_persister: &dyn UserAuthoredQuestionPersister,
    input: impl Read,
    output: &mut impl Write,
) -> Result<()> {
    run_session_from_current(
        config,
        bank,
        strategy,
        &NoopUserSpecificTermPersister,
        &AidaCliContradictsEdges::default(),
        &crate::contradiction::NoopContradictionResolutionPersister,
        &NoopQuestionReweighter,
        user_authored_persister,
        input,
        output,
        0,
        true,
        Vec::new(),
    )
}

#[cfg(test)]
pub(crate) fn run_session_with_question_reweighter(
    config: &CliConfig,
    bank: &dyn QuestionBank,
    strategy: &dyn NextQuestionStrategy,
    question_reweighter: &dyn QuestionReweighter,
    input: impl Read,
    output: &mut impl Write,
) -> Result<()> {
    run_session_from_current(
        config,
        bank,
        strategy,
        &NoopUserSpecificTermPersister,
        &AidaCliContradictsEdges::default(),
        &crate::contradiction::NoopContradictionResolutionPersister,
        question_reweighter,
        &NoopUserAuthoredQuestionPersister,
        input,
        output,
        0,
        true,
        Vec::new(),
    )
}

#[allow(clippy::too_many_arguments)]
fn run_session_from_current(
    config: &CliConfig,
    bank: &dyn QuestionBank,
    strategy: &dyn NextQuestionStrategy,
    term_persister: &dyn UserSpecificTermPersister,
    contradiction_edges: &dyn ContradictsEdges,
    contradiction_resolution_persister: &dyn ContradictionResolutionPersister,
    question_reweighter: &dyn QuestionReweighter,
    user_authored_persister: &dyn UserAuthoredQuestionPersister,
    input: impl Read,
    output: &mut impl Write,
    mut turn: u64,
    write_start_event: bool,
    mut recent_path: Vec<AnsweredQuestion>,
) -> Result<()> {
    let mut input = BufReader::new(input);
    let mut free_text_input = FreeTextInput::from_stdin()?;
    // trace:STORY-82 | ai:claude
    // Mark this session active for its whole lifetime; the guard clears the
    // marker on clean end so concurrent bare-resume never picks a live session.
    let active_guard = SessionActiveGuard::acquire(&config.log_path)?;
    // trace:STORY-127 | ai:claude
    // Build the observer once for the session; its `?` reading is independent of
    // the question-selection strategy and degrades to a structural note offline.
    let observer = ObserverEngine::for_config(config);
    let mut logger = SessionLogger::open(&config.log_path)?;
    // trace:STORY-81 | ai:claude
    // Track whether THIS run recorded any answer. A fresh start that quits
    // before answering anything leaves an empty, un-resumable log; we discard
    // it below so it never clutters `session list`. Resumed sessions
    // (`write_start_event == false`) already carry prior answers, so they are
    // meaningful even when the resumed run adds nothing.
    let mut answer_recorded = false;
    // trace:STORY-159 | ai:claude
    // The live session GOAL/thesis. Seeded from `--goal` / the resumed start
    // (config.goal) and updated in-session by the `/goal` command or an accepted
    // Observer proposal. When set it ORIENTS the next-question prompt (via
    // StrategyContext) and the roundedness score (via the arc), and shows in the
    // breadcrumb. `None` = free-flowing. Belief-neutral: the question being
    // resolved, never a belief.
    let mut goal: Option<String> = config.goal.clone();
    // trace:STORY-161 | ai:claude
    // The live session MODE. Seeded from `--mode` / the resumed start
    // (config.mode) and updated in-session by the `/mode` toggle. It drives the
    // next-question prompt (via StrategyContext) and is logged so the verdict path
    // and resume read the same mode. Belief-neutral: debate argues craft, never
    // which belief is true.
    let mut mode: SessionMode = config.mode;
    let mut current = bank.load_question(&config.seed)?;
    let mut settled_terms = Vec::new();
    let mut surfaced_contradictions = BTreeSet::new();
    let mut pending_revision: Option<(usize, Question, Answer)> = None;
    // trace:STORY-80 | ai:claude
    // A user quit at the frontier defers its end message until after the loop:
    // an empty fresh session is discarded (STORY-81), and pointing the user at
    // a resume command for a log we are about to delete would be a lie. We print
    // the id + resume footer once we know the session survives.
    let mut ended_at_frontier = false;
    // trace:STORY-156 | ai:claude — set when the user accepted the offer to
    // conclude at the well-rounded threshold, so the end footer can preface the
    // resume line with a graceful convergence note (vs a plain quit).
    let mut concluded = false;

    if write_start_event {
        logger.session_started(
            &config.session_id,
            &config.user_id,
            &config.branch_id,
            &current.id,
            config.strategy,
            config.llm_backend,
            // trace:STORY-159 | ai:claude
            config.goal.as_deref(),
            // trace:STORY-161 | ai:claude
            config.mode,
        )?;
    }

    loop {
        let (answered_turn, answer) = if let Some((index, revised_question, revised_answer)) =
            pending_revision.take()
        {
            // trace:STORY-69 | ai:codex
            truncate_session_path(
                config,
                &mut logger,
                index as u64,
                &mut recent_path,
                &mut surfaced_contradictions,
            )?;
            current = revised_question;
            turn = index as u64;
            logger.question_presented(
                &config.session_id,
                &config.user_id,
                &config.branch_id,
                turn,
                &current,
            )?;
            (turn, revised_answer)
        } else {
            let answered_turn = turn;
            logger.question_presented(
                &config.session_id,
                &config.user_id,
                &config.branch_id,
                answered_turn,
                &current,
            )?;
            // trace:STORY-78 | ai:claude
            // Lead each frontier turn with the orientation breadcrumb so a
            // user deep in a long session always sees current topic, how far
            // they've explored (depth = answered questions so far on this
            // path), and which branch they're on.
            // trace:STORY-159 | ai:claude — surface the live goal in the
            // breadcrumb so the user always sees the thesis being resolved.
            render_breadcrumb(
                &current,
                recent_path.len(),
                &config.branch_id,
                goal.as_deref(),
                output,
            )?;
            let probed_terms = load_probed_terms(bank, &current);
            if let Some(settled) = settled_definition_for(&probed_terms, &settled_terms) {
                render_settled_term_definition(settled, output)?;
            } else {
                render_term_definitions(&probed_terms, output)?;
            }
            render_question_for(&current, InputContext::Frontier, output)?;
            let answer = match read_answer_or_end(
                &current.answer_kind,
                InputContext::Frontier,
                &mut input,
                &mut free_text_input,
                output,
            )? {
                AnswerInput::Answer(answer) => answer,
                AnswerInput::Back => {
                    match browse_answered_path(
                        bank,
                        &recent_path,
                        // trace:STORY-128 | ai:claude
                        &ReviewContext {
                            observer: &observer,
                            log_path: &config.log_path,
                            branch: &config.branch_id,
                        },
                        &mut input,
                        &mut free_text_input,
                        output,
                    )? {
                        ReviewOutcome::Frontier => continue,
                        ReviewOutcome::Revised {
                            index,
                            question,
                            answer,
                        } => {
                            pending_revision = Some((index, question, answer));
                            continue;
                        }
                        ReviewOutcome::End => {
                            // trace:STORY-80 | ai:claude
                            ended_at_frontier = true;
                            logger.session_ended(
                                &config.session_id,
                                &config.user_id,
                                &config.branch_id,
                                answered_turn,
                                "User ended session.",
                            )?;
                            break;
                        }
                    }
                }
                AnswerInput::Add => {
                    // trace:STORY-88 | ai:claude
                    // Quick-add: author a new question mid-exploration and
                    // link it as a `begets` follow-on from the CURRENT node,
                    // then re-present the current question so the user
                    // resumes exactly where they paused. The persisted
                    // Q-object is tagged `source:user-authored` (STORY-85)
                    // and shows up as a begets successor in later sessions.
                    quick_add_from_current(
                        bank,
                        strategy,
                        user_authored_persister,
                        &current,
                        &mut input,
                        output,
                    )?;
                    continue;
                }
                AnswerInput::Observe => {
                    // trace:STORY-127 | ai:claude
                    // Non-destructive observer: read the current exchange as
                    // a belief-neutral META voice, then re-present the SAME
                    // question (like eXplore). Nothing is logged or mutated.
                    let exchange = exchange_for_frontier(&current, &recent_path);
                    let reading = {
                        let _spinner = crate::spinner::Spinner::start("observing");
                        observer.read(&exchange)
                    };
                    render_exchange_reading(&reading, output)?;
                    continue;
                }
                AnswerInput::Synopsis => {
                    // trace:STORY-128 | ai:claude
                    // Non-destructive GLOBAL synopsis: read the whole session
                    // log so far as a belief-neutral META voice, then
                    // re-present the SAME question (like Observe). Nothing is
                    // logged or mutated.
                    let rendered = render_session_synopsis(
                        &observer,
                        &config.log_path,
                        Some(&config.branch_id),
                        output,
                    )?;
                    // trace:STORY-156 | ai:claude
                    // CONVERGENCE terminal: when the synopsis crossed the
                    // well-rounded threshold it OFFERED to conclude. Prompt
                    // the user (agency preserved). Accepting prints a final
                    // belief-neutral summary of their OWN position and ends
                    // the session gracefully with the resume footer; declining
                    // (or any non-conclude path / offline) just re-presents the
                    // SAME question and keeps exploring.
                    if let Some((synopsis, arc)) = rendered {
                        // trace:STORY-159 | ai:claude
                        // OBSERVER-PROPOSED goal (way 3 of 3): if the session
                        // is still free-flowing, let the Observer read the arc
                        // and OFFER a goal when a thesis has crystallized. The
                        // user decides; declining keeps exploring. Offered
                        // before the conclude prompt so a goal can orient the
                        // remaining exploration.
                        if goal.is_none() {
                            maybe_propose_goal(
                                &mut goal,
                                &observer,
                                &arc,
                                config,
                                &mut logger,
                                answered_turn,
                                &mut input,
                                &mut free_text_input,
                                output,
                            )?;
                        }
                        if synopsis.offers_conclude()
                            && prompt_to_conclude(&mut input, &mut free_text_input, output)?
                        {
                            crate::synopsis::render_conclusion(&synopsis, &arc, output)?;
                            ended_at_frontier = true;
                            concluded = true;
                            logger.session_ended(
                                &config.session_id,
                                &config.user_id,
                                &config.branch_id,
                                answered_turn,
                                "User concluded at the well-rounded threshold.",
                            )?;
                            break;
                        }
                    }
                    continue;
                }
                AnswerInput::Forward => continue,
                AnswerInput::Goal(text) => {
                    // trace:STORY-159 | ai:claude
                    // In-session goal (way 2 of 3): the user states the
                    // thesis. A non-empty text SETS the goal — logged as a
                    // `goal_set` event (so resume restores it and the arc /
                    // synopsis orient to it) — then the SAME question is
                    // re-presented, now oriented toward the goal. A bare
                    // `/goal` (empty text) just SHOWS the current goal; it
                    // never clears one (agency: a goal is removed only by
                    // setting a new one). Non-destructive: nothing else
                    // changes. Belief-neutral: the goal is the question being
                    // settled, never a belief.
                    set_goal_in_session(
                        &mut goal,
                        &text,
                        "user",
                        config,
                        &mut logger,
                        answered_turn,
                        output,
                    )?;
                    continue;
                }
                AnswerInput::Mode(token) => {
                    // trace:STORY-161 | ai:claude
                    // In-session mode toggle: `/mode debate` makes the
                    // questioner steelman the OPPOSING side; `/mode socratic`
                    // returns to the neutral-challenger default. A non-empty
                    // token SETS the mode (logged as a `mode_set` event so
                    // resume restores it and the verdict path frames the
                    // debate); a bare `/mode` SHOWS the current mode. Then the
                    // SAME question is re-presented under the new mode.
                    // Belief-neutral: debate argues craft, never which belief is
                    // true.
                    set_mode_in_session(
                        &mut mode,
                        &token,
                        config,
                        &mut logger,
                        answered_turn,
                        output,
                    )?;
                    continue;
                }
                AnswerInput::Rest => {
                    // trace:STORY-160 | ai:claude
                    // "Rest your case": a PHASE TRANSITION out of the
                    // question/answer loop into the CLOSING ritual. The user
                    // (the only party who rests at the frontier) states their
                    // settled position(s); the challenger answers each with its
                    // strongest remaining objection; the ritual ends on
                    // `verdict` or `terminate` (terminator forfeits the last
                    // word). The session ALWAYS ends after the ritual, so this
                    // breaks the main loop with the closing outcome.
                    let outcome = run_closing_phase(
                        config,
                        &observer,
                        goal.as_deref(),
                        ClosingParty::User,
                        &mut logger,
                        answered_turn,
                        &mut input,
                        &mut free_text_input,
                        output,
                    )?;
                    // trace:STORY-160 | ai:claude — a rested case is meaningful
                    // session activity even if no question was answered, so the
                    // log survives (it carries the closing statements + verdict)
                    // rather than being discarded as empty (STORY-81).
                    answer_recorded = true;
                    ended_at_frontier = true;
                    logger.session_ended(
                        &config.session_id,
                        &config.user_id,
                        &config.branch_id,
                        answered_turn,
                        outcome.summary,
                    )?;
                    break;
                }
                AnswerInput::Verdict => {
                    // trace:STORY-160 | ai:claude
                    // A direct request for the FINAL VERDICT at the frontier:
                    // rest the case and render the belief-neutral roundedness
                    // verdict immediately (no objection exchange). Logged as a
                    // closing phase transition so the log shows the ritual.
                    logger.phase_changed(
                        &config.session_id,
                        &config.user_id,
                        &config.branch_id,
                        answered_turn,
                        "closing",
                        ClosingParty::User.as_str(),
                    )?;
                    let outcome = finish_with_verdict(config, &observer, goal.as_deref(), output)?;
                    // trace:STORY-160 | ai:claude — keep the rested/verdict
                    // session (it carries the phase transition + verdict).
                    answer_recorded = true;
                    ended_at_frontier = true;
                    logger.session_ended(
                        &config.session_id,
                        &config.user_id,
                        &config.branch_id,
                        answered_turn,
                        outcome.summary,
                    )?;
                    break;
                }
                AnswerInput::Terminate => {
                    // trace:STORY-160 | ai:claude
                    // `terminate` at the frontier: the user rests AND
                    // terminates in one step. By the fairness rule the user
                    // forfeits the last word, so the CHALLENGER makes the final
                    // closing statement (its strongest remaining objection)
                    // before the verdict renders.
                    logger.phase_changed(
                        &config.session_id,
                        &config.user_id,
                        &config.branch_id,
                        answered_turn,
                        "closing",
                        ClosingParty::User.as_str(),
                    )?;
                    render_closing_banner(output)?;
                    render_terminate_note(ClosingParty::User, output)?;
                    let objection = {
                        let _spinner = crate::spinner::Spinner::start("closing objection");
                        observer.closing_objection("", goal.as_deref())
                    };
                    render_closing_objection(&objection, true, output)?;
                    logger.closing_statement(
                        &config.session_id,
                        &config.user_id,
                        &config.branch_id,
                        answered_turn,
                        ClosingParty::Challenger.as_str(),
                        &objection.objection,
                        true,
                    )?;
                    let outcome = finish_with_verdict(config, &observer, goal.as_deref(), output)?;
                    // trace:STORY-160 | ai:claude — keep the terminated session.
                    answer_recorded = true;
                    ended_at_frontier = true;
                    logger.session_ended(
                        &config.session_id,
                        &config.user_id,
                        &config.branch_id,
                        answered_turn,
                        outcome.summary,
                    )?;
                    break;
                }
                AnswerInput::Help(question) => {
                    // trace:STORY-163 | ai:claude — non-destructive process-help
                    // channel; render the graceful placeholder and re-present the
                    // SAME question (like Observe). The LLM answer lands in STORY-164.
                    render_help_placeholder(&question, output)?;
                    continue;
                }
                AnswerInput::Tutor(text) => {
                    // trace:STORY-163 | ai:claude — non-destructive articulation
                    // coach; render the graceful placeholder and re-present the SAME
                    // question. The coaching engine lands in STORY-165.
                    render_tutor_placeholder(&text, output)?;
                    continue;
                }
                AnswerInput::End => {
                    // trace:STORY-80 | ai:claude
                    ended_at_frontier = true;
                    logger.session_ended(
                        &config.session_id,
                        &config.user_id,
                        &config.branch_id,
                        answered_turn,
                        "User ended session.",
                    )?;
                    break;
                }
            };
            (answered_turn, answer)
        };
        let probed_terms = load_probed_terms(bank, &current);
        if answer.normalized == "explore" {
            // trace:STORY-52 | ai:codex
            if let Some(settled) = settled_definition_for(&probed_terms, &settled_terms) {
                render_settled_term_definition(settled, output)?;
            } else if let Some(settled) = prompt_for_term_meaning(
                &probed_terms,
                strategy,
                term_persister,
                &mut input,
                &mut free_text_input,
                output,
            )? {
                logger.term_interpreted(
                    &config.session_id,
                    &config.user_id,
                    &config.branch_id,
                    answered_turn,
                    &settled,
                    &probed_terms,
                )?;
                settled_terms.push(settled);
            }
            continue;
        }
        logger.answer_recorded(
            &config.session_id,
            &config.user_id,
            &config.branch_id,
            answered_turn,
            &current,
            &answer,
        )?;
        // trace:STORY-81 | ai:claude
        answer_recorded = true;
        if answer.normalized == "punt" {
            // trace:STORY-53 | ai:codex
            let _updated =
                question_reweighter.reweight_question(&current, QualitySignal::Punted)?;
            match different_topic_punt_question(&current, &recent_path, bank)? {
                Some(next) => {
                    logger.next_question_selected(
                        &config.session_id,
                        &config.user_id,
                        &config.branch_id,
                        answered_turn,
                        &current.id,
                        &next.id,
                        "Punt selected a different-topic question.",
                    )?;
                    recent_path.push(AnsweredQuestion {
                        question_ref: current.id.clone(),
                        question_text: current.title.clone(),
                        raw_answer: answer.raw,
                        normalized_answer: answer.normalized,
                    });
                    current = next;
                    turn += 1;
                    continue;
                }
                None => {
                    // trace:BUG-136 | ai:claude
                    // Punt found no different-topic target — offer the dead-end
                    // menu instead of forcing an exit.
                    let context = StrategyContext {
                        answer: answer.clone(),
                        recent_path: recent_path.clone(),
                        // trace:STORY-159 | ai:claude — orient toward the goal.
                        goal: goal.clone(),
                        // trace:STORY-161 | ai:claude — carry the live mode.
                        mode,
                    };
                    match dead_end_menu(
                        bank,
                        strategy,
                        user_authored_persister,
                        &observer,
                        &config.log_path,
                        &config.branch_id,
                        &current,
                        &context,
                        &recent_path,
                        &mut input,
                        &mut free_text_input,
                        output,
                    )? {
                        DeadEndOutcome::Continue(next) => {
                            logger.next_question_selected(
                                &config.session_id,
                                &config.user_id,
                                &config.branch_id,
                                answered_turn,
                                &current.id,
                                &next.id,
                                "Continued past a punt dead end via the menu.",
                            )?;
                            recent_path.push(AnsweredQuestion {
                                question_ref: current.id.clone(),
                                question_text: current.title.clone(),
                                raw_answer: answer.raw,
                                normalized_answer: answer.normalized,
                            });
                            current = next;
                            turn += 1;
                            continue;
                        }
                        DeadEndOutcome::Quit => {
                            // trace:STORY-80 | ai:claude
                            ended_at_frontier = true;
                            logger.session_ended(
                                &config.session_id,
                                &config.user_id,
                                &config.branch_id,
                                answered_turn,
                                "User quit at the dead-end menu (no punt target).",
                            )?;
                            break;
                        }
                    }
                }
            }
        }
        if let Some(contradiction) = next_live_contradiction(
            &config.log_path,
            &config.branch_id,
            contradiction_edges,
            &mut surfaced_contradictions,
        )? {
            // trace:STORY-58 | ai:codex
            turn += 1;
            if ask_contradiction_follow_up(
                config,
                &mut logger,
                turn,
                &contradiction,
                &mut input,
                &mut free_text_input,
                contradiction_resolution_persister,
                output,
            )? {
                break;
            }
        }
        if matches!(current.answer_kind, AnswerKind::FreeText) {
            let flagged_terms = strategy.loaded_terms(&current, &answer).unwrap_or_default();
            let definitions = definitions_for_loaded_terms(&probed_terms, &flagged_terms);
            if let Some(settled) = settled_definition_for(&definitions, &settled_terms) {
                render_settled_term_definition(settled, output)?;
            } else {
                render_term_definitions(&definitions, output)?;
            }
        }
        let context = StrategyContext {
            answer: answer.clone(),
            recent_path: recent_path.clone(),
            // trace:STORY-159 | ai:claude — the next-question selection orients
            // toward the live goal so questions aim at resolving it.
            goal: goal.clone(),
            // trace:STORY-161 | ai:claude — and follows the live mode so debate
            // mode steelmans the opposing side.
            mode,
        };

        // trace:BUG-100 | ai:claude
        // Hold one 'thinking' spinner across the ENTIRE next-question
        // computation — candidate gathering (`aida rel list` / `aida show`),
        // the blocking LLM call, and persistence (`aida add` / `aida rel add`).
        // STORY-83 scoped the spinner to just the LLM call, leaving frozen gaps
        // for the surrounding AIDA shell-outs before and after it. The guard is
        // dropped before any output for the next question is printed, so the
        // spinner line is cleared cleanly. TTY-only / stderr behavior is
        // inherited from the spinner util, so piped output is unchanged.
        let next_question = {
            let _spinner = crate::spinner::Spinner::start("thinking");
            strategy.next_question(&current, &context, bank)?
        };
        match next_question {
            Some(next) => {
                logger.next_question_selected(
                    &config.session_id,
                    &config.user_id,
                    &config.branch_id,
                    answered_turn,
                    &current.id,
                    &next.id,
                    "Configured next-question strategy selected the follow-up.",
                )?;
                recent_path.push(AnsweredQuestion {
                    question_ref: current.id.clone(),
                    question_text: current.title.clone(),
                    raw_answer: answer.raw,
                    normalized_answer: answer.normalized,
                });
                current = next;
                turn += 1;
            }
            None => {
                // trace:BUG-136 | ai:claude
                // No begets successor is not a forced exit: offer the dead-end
                // menu (generate / punt / add / synopsis / quit). Continue from
                // the chosen question, or end with the deferred footer on quit.
                match dead_end_menu(
                    bank,
                    strategy,
                    user_authored_persister,
                    &observer,
                    &config.log_path,
                    &config.branch_id,
                    &current,
                    &context,
                    &recent_path,
                    &mut input,
                    &mut free_text_input,
                    output,
                )? {
                    DeadEndOutcome::Continue(next) => {
                        logger.next_question_selected(
                            &config.session_id,
                            &config.user_id,
                            &config.branch_id,
                            answered_turn,
                            &current.id,
                            &next.id,
                            "Continued past a dead end via the menu.",
                        )?;
                        recent_path.push(AnsweredQuestion {
                            question_ref: current.id.clone(),
                            question_text: current.title.clone(),
                            raw_answer: answer.raw,
                            normalized_answer: answer.normalized,
                        });
                        current = next;
                        turn += 1;
                    }
                    DeadEndOutcome::Quit => {
                        // trace:STORY-80 | ai:claude — surviving session still
                        // gets the deferred id + resume footer after the loop.
                        ended_at_frontier = true;
                        logger.session_ended(
                            &config.session_id,
                            &config.user_id,
                            &config.branch_id,
                            answered_turn,
                            "User quit at the dead-end menu (no begets successor).",
                        )?;
                        break;
                    }
                }
            }
        }
    }

    // trace:STORY-81 | ai:claude
    // Discard an empty session: a fresh start that ended without recording a
    // single answer is meaningless to resume and only clutters `session list`.
    // Close our handles (drop logger, then the active guard so its marker is
    // gone) before removing the log + marker so nothing is left on disk.
    let discarded = write_start_event && !answer_recorded;
    if discarded {
        drop(logger);
        drop(active_guard);
        discard_empty_session(&config.log_path)?;
    }

    // trace:STORY-80 | ai:claude
    // Emit the deferred end footer for a user quit at the frontier. A discarded
    // empty session prints a plain "Session ended." with no resume hint (the log
    // is gone, so resuming would fail); a surviving session gets the id + the
    // exact resume command so the user always has a way back in.
    if ended_at_frontier {
        if discarded {
            writeln!(output, "Session ended.")?;
        } else {
            // trace:STORY-156 | ai:claude — a concluded session reached a GOOD
            // (convergence) terminal, so preface the resume footer with a
            // belief-neutral note rather than ending silently. Resume still works
            // — concluding marks a coherent stopping point, not a closed door.
            let preface = if concluded {
                Some("Concluded — your position is well-rounded. You can still resume to probe edge cases.")
            } else {
                None
            };
            render_session_end(preface, &config.session_id, output)?;
        }
    }

    Ok(())
}

// trace:STORY-81 | ai:claude
// Remove an empty session's log plus any liveness marker left next to it.
// Missing files are not an error (the active guard may have already cleared
// its marker on drop), so absent paths are ignored.
fn discard_empty_session(log_path: &Path) -> Result<()> {
    remove_if_present(log_path)?;
    remove_if_present(&session_active_marker_path(log_path))?;
    Ok(())
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn next_live_contradiction(
    log_path: &Path,
    branch_id: &str,
    edges: &dyn ContradictsEdges,
    surfaced: &mut BTreeSet<(String, String)>,
) -> Result<Option<Contradiction>> {
    let file = File::open(log_path)?;
    let beliefs = beliefs_from_session_log(file, Some(branch_id))?;
    let contradictions = detect_graph_contradictions(&beliefs, edges).unwrap_or_default();
    for contradiction in contradictions {
        let pair = contradiction_pair_key(&contradiction);
        if surfaced.insert(pair) {
            return Ok(Some(contradiction));
        }
    }
    Ok(None)
}

fn contradiction_pair_key(contradiction: &Contradiction) -> (String, String) {
    if contradiction.left <= contradiction.right {
        (contradiction.left.clone(), contradiction.right.clone())
    } else {
        (contradiction.right.clone(), contradiction.left.clone())
    }
}

fn ask_contradiction_follow_up(
    config: &CliConfig,
    logger: &mut SessionLogger,
    turn: u64,
    contradiction: &Contradiction,
    input: &mut impl BufRead,
    free_text_input: &mut FreeTextInput,
    resolution_persister: &dyn ContradictionResolutionPersister,
    output: &mut impl Write,
) -> Result<bool> {
    let question = Question {
        id: format!("contradiction-{turn}"),
        title: format!(
            "You leaned {} and also {} -- these seem to conflict; which holds, or how do you reconcile them?",
            contradiction.left, contradiction.right
        ),
        tags: vec!["runtime:contradiction".to_string()],
        answer_kind: AnswerKind::FreeText,
        weight: 0,
    };
    logger.question_presented(
        &config.session_id,
        &config.user_id,
        &config.branch_id,
        turn,
        &question,
    )?;
    render_question(&question, output)?;
    match read_answer_or_end(
        &question.answer_kind,
        InputContext::Frontier,
        input,
        free_text_input,
        output,
    )? {
        AnswerInput::Answer(answer) => {
            let resolution = resolution_persister.persist_resolution(contradiction, &answer.raw)?;
            logger.contradiction_resolved(
                &config.session_id,
                &config.user_id,
                &config.branch_id,
                turn,
                contradiction,
                &answer,
                resolution.as_ref(),
            )?;
            logger.answer_recorded(
                &config.session_id,
                &config.user_id,
                &config.branch_id,
                turn,
                &question,
                &answer,
            )?;
            Ok(false)
        }
        AnswerInput::End => {
            // trace:STORY-80 | ai:claude
            render_session_end(None, &config.session_id, output)?;
            logger.session_ended(
                &config.session_id,
                &config.user_id,
                &config.branch_id,
                turn,
                "User ended session.",
            )?;
            Ok(true)
        }
        // trace:STORY-88 | ai:claude — quick-add is offered only at the plain
        // frontier prompt, not on the contradiction follow-up; treat any stray
        // Add/Back/Forward here as a no-op that re-presents nothing.
        // trace:STORY-127 | ai:claude — likewise the observer control: the
        // contradiction prompt is a transient runtime question, so a stray `?`
        // here is a no-op rather than a reading of a synthetic exchange.
        // trace:STORY-128 | ai:claude — and the synopsis control: this transient
        // runtime prompt has no observer engine in scope, so a stray `S` here is
        // a no-op rather than a whole-session reading.
        // trace:STORY-159 | ai:claude — likewise the goal command: this transient
        // runtime contradiction prompt has no goal state in scope, so a stray
        // `/goal` here is a no-op rather than re-orienting mid-resolution.
        // trace:STORY-160 | ai:claude — the closing-ritual controls (rest /
        // verdict / terminate) likewise have no closing-phase state in scope on a
        // transient contradiction follow-up, so a stray one here is a no-op rather
        // than opening the closing ritual mid-resolution.
        AnswerInput::Add
        | AnswerInput::Back
        | AnswerInput::Forward
        | AnswerInput::Observe
        | AnswerInput::Synopsis
        | AnswerInput::Goal(_)
        // trace:STORY-161 | ai:claude — a stray `/mode` toggle on a transient
        // contradiction follow-up is a no-op here (no loop mode state in scope).
        | AnswerInput::Mode(_)
        | AnswerInput::Rest
        | AnswerInput::Verdict
        | AnswerInput::Terminate
        // trace:STORY-163 | ai:claude — a stray `/help` / `/tutor` on a transient
        // contradiction follow-up is a no-op here (no LLM channel state in scope).
        | AnswerInput::Help(_)
        | AnswerInput::Tutor(_) => Ok(false),
    }
}

// trace:STORY-88 | ai:claude
/// In-session quick-add: author a new question mid-exploration and link it as a
/// `begets` follow-on from `current`.
///
/// Runs the shared STORY-87 authoring core ([`crate::question_add::author_question`]):
/// prompt for the text + answer shape, run the DEDUP/REFINE approve flow over
/// the current bank snapshot, and persist the result tagged
/// `source:user-authored` (STORY-85) with a `begets` edge from `current`. The
/// new Q-object therefore surfaces as a begets successor in later sessions.
///
/// Degrades gracefully: a bank read failure simply yields an empty dedup
/// snapshot (no duplicate), and the offline / non-TTY paths are inherited from
/// the authoring core, which reads every prompt from `input`.
fn quick_add_from_current(
    bank: &dyn QuestionBank,
    strategy: &dyn NextQuestionStrategy,
    user_authored_persister: &dyn UserAuthoredQuestionPersister,
    current: &Question,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<()> {
    writeln!(
        output,
        "Quick-add: authoring a new question linked from {}.",
        current.id
    )?;
    // The dedup search is pure over the in-memory snapshot; an AIDA hiccup just
    // yields no duplicate rather than aborting the session.
    let existing = bank.all_questions().unwrap_or_default();
    let topic = quick_add_topic(current);
    let link = QuestionLink::Begets {
        origin_id: current.id.clone(),
    };
    crate::question_add::author_question(
        &existing,
        strategy,
        user_authored_persister,
        &topic,
        &link,
        input,
        output,
    )?;
    Ok(())
}

// trace:STORY-88 | ai:claude
/// Topic for a quick-added question: inherit the current node's `topic:<slug>`
/// tag so the follow-on lands in the same cluster, falling back to a stable
/// placeholder when the current question carries no topic (e.g. a runtime
/// contradiction prompt).
fn quick_add_topic(current: &Question) -> String {
    current
        .tags
        .iter()
        .find_map(|tag| tag.strip_prefix("topic:"))
        .map(str::trim)
        .filter(|topic| !topic.is_empty())
        .unwrap_or("user-authored")
        .to_string()
}

// trace:BUG-136 | ai:claude
/// What the dead-end menu resolved to: continue the session from a freshly
/// chosen question, or quit.
enum DeadEndOutcome {
    Continue(Question),
    Quit,
}

// trace:BUG-136 | ai:claude
/// Render the dead-end menu. Plain text so it reads correctly under NO_COLOR /
/// non-TTY (matching the rest of the session controls).
fn render_dead_end_menu(output: &mut impl Write) -> Result<()> {
    writeln!(
        output,
        "\nNo further questions on this path — but you're not stuck. What next?"
    )?;
    writeln!(
        output,
        "  [G] Generate a fresh question   [P] Punt to a different topic"
    )?;
    writeln!(
        output,
        "  [A] Add your own question       [S] Synopsis   [Q] Quit"
    )?;
    Ok(())
}

// trace:BUG-136 | ai:claude
/// Present the dead-end menu and act on the choice. A genuine dead end (the
/// strategy returned no successor) is not a forced exit: the user can generate
/// a fresh question, punt to a different topic, author their own, read a
/// synopsis, or quit. Loops until a choice yields a next question
/// ([`DeadEndOutcome::Continue`]) or the user quits ([`DeadEndOutcome::Quit`]);
/// EOF on the menu prompt is treated as quit. `[G]` re-runs the configured
/// strategy from `current` — with `--strategy llm` that generates and persists a
/// fresh follow-on; a deterministic/exhausted bank simply reports it has nothing
/// and the menu stays open (the offline-degrade path).
#[allow(clippy::too_many_arguments)]
fn dead_end_menu(
    bank: &dyn QuestionBank,
    strategy: &dyn NextQuestionStrategy,
    user_authored_persister: &dyn UserAuthoredQuestionPersister,
    observer: &ObserverEngine,
    log_path: &Path,
    branch: &str,
    current: &Question,
    context: &StrategyContext,
    recent_path: &[AnsweredQuestion],
    input: &mut impl BufRead,
    free_text_input: &mut FreeTextInput,
    output: &mut impl Write,
) -> Result<DeadEndOutcome> {
    loop {
        render_dead_end_menu(output)?;
        let choice = match free_text_input.read_line(input, output, "> ")? {
            Some(line) => line.trim().to_ascii_lowercase(),
            None => return Ok(DeadEndOutcome::Quit),
        };
        match choice.chars().next() {
            Some('g') => {
                let generated = {
                    let _spinner = crate::spinner::Spinner::start("thinking");
                    strategy.next_question(current, context, bank)?
                };
                match generated {
                    Some(next) => return Ok(DeadEndOutcome::Continue(next)),
                    None => writeln!(
                        output,
                        "Couldn't generate a new question here (this strategy is exhausted — try `--strategy llm`)."
                    )?,
                }
            }
            Some('p') => match different_topic_punt_question(current, recent_path, bank)? {
                Some(next) => return Ok(DeadEndOutcome::Continue(next)),
                None => writeln!(output, "No different-topic question to punt to.")?,
            },
            Some('a') => {
                // Author + link a begets follow-on from the current node; it
                // becomes a successor in later sessions. Stay in the menu so the
                // user can [G]enerate into it (or pick another exit).
                quick_add_from_current(
                    bank,
                    strategy,
                    user_authored_persister,
                    current,
                    input,
                    output,
                )?;
            }
            Some('s') => {
                // trace:STORY-156 | ai:claude — the dead-end menu surfaces the
                // synopsis (with its conclude OFFER line when well-rounded) but
                // stays in the menu; the graceful conclude path lives at the
                // frontier handler, so the return is intentionally ignored here.
                render_session_synopsis(observer, log_path, Some(branch), output)?;
            }
            Some('q') => return Ok(DeadEndOutcome::Quit),
            _ => writeln!(output, "Pick one of G, P, A, S, or Q.")?,
        }
    }
}

enum ReviewOutcome {
    Frontier,
    Revised {
        index: usize,
        question: Question,
        answer: Answer,
    },
    End,
}

// trace:STORY-128 | ai:claude
/// The session-level context the review pane's observer controls need: the
/// Observer engine (for the per-exchange `?` reading) plus where to find the
/// live session log (for the whole-session `S` synopsis). Bundled so the review
/// helper keeps a tidy argument list.
struct ReviewContext<'a> {
    observer: &'a ObserverEngine,
    log_path: &'a Path,
    branch: &'a str,
}

fn browse_answered_path(
    bank: &dyn QuestionBank,
    recent_path: &[AnsweredQuestion],
    // trace:STORY-127 | ai:claude — the `?` observer and (STORY-128) the `S`
    // synopsis both live in this session-level context.
    review: &ReviewContext<'_>,
    input: &mut impl BufRead,
    free_text_input: &mut FreeTextInput,
    output: &mut impl Write,
) -> Result<ReviewOutcome> {
    if recent_path.is_empty() {
        writeln!(output, "No previous answers to review.")?;
        return Ok(ReviewOutcome::Frontier);
    }
    let mut cursor = recent_path.len() - 1;
    loop {
        let reviewed = &recent_path[cursor];
        let question = bank.load_question(&reviewed.question_ref)?;
        render_reviewed_answer(cursor, recent_path.len(), reviewed, output)?;
        render_question_for(&question, InputContext::Review, output)?;
        match read_answer_or_end(
            &question.answer_kind,
            InputContext::Review,
            input,
            free_text_input,
            output,
        )? {
            AnswerInput::Back => {
                if cursor == 0 {
                    writeln!(output, "Already at the first answered question.")?;
                } else {
                    cursor -= 1;
                }
            }
            AnswerInput::Forward => {
                if cursor + 1 == recent_path.len() {
                    return Ok(ReviewOutcome::Frontier);
                }
                cursor += 1;
            }
            AnswerInput::Answer(answer) => {
                if answer.normalized == reviewed.normalized_answer {
                    writeln!(output, "Answer unchanged; still reviewing the saved path.")?;
                    continue;
                }
                return Ok(ReviewOutcome::Revised {
                    index: cursor,
                    question,
                    answer,
                });
            }
            // trace:STORY-88 | ai:claude — the review pane does not offer the
            // quick-add control (it is frontier-only), so Add never reaches
            // here; stay on the saved path if it somehow does.
            AnswerInput::Add => continue,
            // trace:STORY-127 | ai:claude — non-destructive observer in review:
            // read the reviewed exchange (this question -> the saved answer ->
            // the step it led to) and stay on the same reviewed answer.
            AnswerInput::Observe => {
                let rebuttal = recent_path
                    .get(cursor + 1)
                    .map(|next| next.question_text.clone())
                    .unwrap_or_else(|| question.title.clone());
                let exchange = Exchange {
                    question: question.title.clone(),
                    answer: reviewed.raw_answer.clone(),
                    rebuttal,
                };
                let reading = {
                    let _spinner = crate::spinner::Spinner::start("observing");
                    review.observer.read(&exchange)
                };
                render_exchange_reading(&reading, output)?;
                continue;
            }
            // trace:STORY-128 | ai:claude — non-destructive GLOBAL synopsis in
            // review: read the whole session log so far and stay on the same
            // reviewed answer.
            AnswerInput::Synopsis => {
                render_session_synopsis(
                    review.observer,
                    review.log_path,
                    Some(review.branch),
                    output,
                )?;
                continue;
            }
            // trace:STORY-159 | ai:claude — the goal command is frontier-only in
            // effect: the review pane re-walks the saved path, so a stray `/goal`
            // here is a no-op rather than re-orienting from inside review. (The
            // goal is set at the frontier, where it can orient the next question.)
            AnswerInput::Goal(_) => continue,
            // trace:STORY-161 | ai:claude — the mode toggle, like `/goal`, takes
            // effect at the FRONTIER (where the live mode drives the next-question
            // prompt), so a stray `/mode` in the review pane is a no-op.
            AnswerInput::Mode(_) => continue,
            // trace:STORY-160 | ai:claude — the closing ritual begins at the
            // FRONTIER (where the live position + goal state are in scope), not
            // from inside the review pane re-walking the saved path. A stray
            // rest / verdict / terminate here is a no-op; the user returns to the
            // frontier (Forward / Back to the live edge) to rest their case.
            AnswerInput::Rest | AnswerInput::Verdict | AnswerInput::Terminate => continue,
            // trace:STORY-163 | ai:claude — `/help` (process) and `/tutor`
            // (articulation coach) are non-destructive out-of-band channels that
            // apply anywhere, including the review pane: render the graceful
            // placeholder and stay on the same reviewed answer.
            AnswerInput::Help(question) => {
                render_help_placeholder(&question, output)?;
                continue;
            }
            AnswerInput::Tutor(text) => {
                render_tutor_placeholder(&text, output)?;
                continue;
            }
            AnswerInput::End => return Ok(ReviewOutcome::End),
        }
    }
}

fn render_reviewed_answer(
    cursor: usize,
    total: usize,
    answer: &AnsweredQuestion,
    output: &mut impl Write,
) -> Result<()> {
    // trace:STORY-69 | ai:codex
    writeln!(output, "\nReviewing answer {}/{}:", cursor + 1, total)?;
    writeln!(output, "{}", answer.question_text)?;
    writeln!(output, "saved answer: {}", answer.raw_answer)?;
    Ok(())
}

fn truncate_session_path(
    config: &CliConfig,
    logger: &mut SessionLogger,
    from_turn: u64,
    recent_path: &mut Vec<AnsweredQuestion>,
    surfaced_contradictions: &mut BTreeSet<(String, String)>,
) -> Result<()> {
    recent_path.truncate(from_turn as usize);
    surfaced_contradictions.clear();
    logger.path_truncated(
        &config.session_id,
        &config.user_id,
        &config.branch_id,
        from_turn,
        "User revised a reviewed answer.",
    )
}

fn settled_definition_for<'a>(
    definitions: &[TermDefinition],
    settled_terms: &'a [SettledTermDefinition],
) -> Option<&'a SettledTermDefinition> {
    if definitions.is_empty() {
        return None;
    }
    let label = term_label(definitions);
    settled_terms
        .iter()
        .rev()
        .find(|settled| settled.term_label == label)
}

#[cfg(test)]
pub(crate) fn resume_session(
    config: &CliConfig,
    bank: &dyn QuestionBank,
    strategy: &dyn NextQuestionStrategy,
    input: impl Read,
    output: &mut impl Write,
) -> Result<()> {
    resume_session_with_term_persister(
        config,
        bank,
        strategy,
        &NoopUserSpecificTermPersister,
        input,
        output,
    )
}

fn resume_session_with_term_persister(
    config: &CliConfig,
    bank: &dyn QuestionBank,
    strategy: &dyn NextQuestionStrategy,
    term_persister: &dyn UserSpecificTermPersister,
    input: impl Read,
    output: &mut impl Write,
) -> Result<()> {
    // trace:STORY-20 | ai:codex
    let replay = SessionReplay::load(&config.log_path, &config.branch_id)?;
    replay.render_recap(output)?;
    replay.render(output)?;

    let recent_path = replay.recent_path();
    let question_reweighter = AidaCliQuestionReweighter::default();
    // trace:STORY-88 | ai:claude — resumed sessions get the same quick-add path.
    let user_authored_persister = AidaCliUserAuthoredQuestionPersister::default();

    // Normal resume: a saved follow-up question exists, present it.
    if let Some(next_question_ref) = replay.next_question_ref.as_ref() {
        let mut resumed_config = config.clone();
        resumed_config.seed = next_question_ref.clone();
        return run_session_from_current(
            &resumed_config,
            bank,
            strategy,
            term_persister,
            &AidaCliContradictsEdges::default(),
            &AidaCliContradictionResolutionPersister::default(),
            &question_reweighter,
            &user_authored_persister,
            input,
            output,
            replay.next_turn,
            false,
            recent_path,
        );
    }

    // trace:BUG-136 | ai:claude
    // No saved follow-up — but a terminal saved path is NOT a dead end. Try to
    // continue from the last answered question: auto-attempt a fresh successor
    // (with `--strategy llm` this generates one; a deterministic bank may still
    // surface a begets edge), and only fall back to the interactive dead-end
    // menu if nothing comes back. The session ends only if the user quits.
    let Some(last) = replay.answers.last() else {
        // Nothing answered and nothing saved: genuinely empty (STORY-81 normally
        // discards these before they can be resumed).
        render_session_end(
            Some("No saved follow-up question. Session complete."),
            &config.session_id,
            output,
        )?;
        return Ok(());
    };
    let last_question = bank.load_question(&last.question_ref)?;
    // The path that LED to the last answered question excludes the question
    // itself — that is the strategy's `recent_path` when computing its successor.
    let prior_path: Vec<AnsweredQuestion> = recent_path[..recent_path.len() - 1].to_vec();
    let context = StrategyContext {
        answer: Answer {
            raw: last.raw_answer.clone(),
            normalized: last.normalized_answer.clone(),
        },
        recent_path: prior_path.clone(),
        // trace:STORY-159 | ai:claude — a resumed session keeps orienting toward
        // its restored goal when auto-continuing a terminal saved path.
        goal: config.goal.clone(),
        // trace:STORY-161 | ai:claude — and keeps its restored mode.
        mode: config.mode,
    };

    let auto = {
        let _spinner = crate::spinner::Spinner::start("thinking");
        strategy.next_question(&last_question, &context, bank)?
    };

    let mut input = BufReader::new(input);
    let next = match auto {
        Some(next) => next,
        None => {
            let mut free_text_input = FreeTextInput::from_stdin()?;
            let observer = ObserverEngine::for_config(config);
            match dead_end_menu(
                bank,
                strategy,
                &user_authored_persister,
                &observer,
                &config.log_path,
                &config.branch_id,
                &last_question,
                &context,
                &prior_path,
                &mut input,
                &mut free_text_input,
                output,
            )? {
                DeadEndOutcome::Continue(next) => next,
                DeadEndOutcome::Quit => {
                    // trace:STORY-80 | ai:claude — surviving session keeps its
                    // id + resume footer.
                    render_session_end(None, &config.session_id, output)?;
                    return Ok(());
                }
            }
        }
    };

    // Persist the continuation so a later bare resume sees it as the saved
    // follow-up (rather than re-deriving it every time).
    {
        let mut logger = SessionLogger::open(&config.log_path)?;
        logger.next_question_selected(
            &config.session_id,
            &config.user_id,
            &config.branch_id,
            last.turn,
            &last_question.id,
            &next.id,
            "Continued a terminal saved path on resume.",
        )?;
    }

    let mut resumed_config = config.clone();
    resumed_config.seed = next.id.clone();
    run_session_from_current(
        &resumed_config,
        bank,
        strategy,
        term_persister,
        &AidaCliContradictsEdges::default(),
        &AidaCliContradictionResolutionPersister::default(),
        &question_reweighter,
        &user_authored_persister,
        &mut input,
        output,
        replay.next_turn,
        false,
        recent_path,
    )
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct SessionSummary {
    pub(crate) session_id: String,
    pub(crate) path: PathBuf,
    pub(crate) started_at: Option<String>,
    pub(crate) last_active_at: Option<String>,
    pub(crate) branch_id: Option<String>,
    pub(crate) last_question_answered: Option<String>,
}

pub(crate) fn session_summaries(dir: &Path) -> Result<Vec<SessionSummary>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut summaries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(summary) = SessionSummary::load(&path)? {
            summaries.push(summary);
        }
    }
    summaries.sort_by(|left, right| {
        right
            .last_active_at
            .cmp(&left.last_active_at)
            .then_with(|| right.session_id.cmp(&left.session_id))
    });
    Ok(summaries)
}

impl SessionSummary {
    fn load(path: &Path) -> Result<Option<Self>> {
        let file = File::open(path)?;
        Self::from_reader(file, path)
    }

    pub(crate) fn from_reader(reader: impl Read, path: &Path) -> Result<Option<Self>> {
        let reader = BufReader::new(reader);
        let mut session_id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("unknown")
            .to_string();
        let mut started_at = None;
        let mut last_active_at = None;
        let mut branch_id = None;
        let mut questions = BTreeMap::new();
        let mut last_question_answered = None;

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&line)
                .map_err(|error| QuizdomError::Parse(error.to_string()))?;
            if let Some(occurred_at) = value.get("occurred_at").and_then(Value::as_str) {
                if started_at.is_none() {
                    started_at = Some(occurred_at.to_string());
                }
                last_active_at = Some(occurred_at.to_string());
            }
            if let Some(id) = value.get("session_id").and_then(Value::as_str) {
                session_id = id.to_string();
            }
            if let Some(branch) = value.get("branch_id").and_then(Value::as_str) {
                branch_id = Some(branch.to_string());
            }
            match value.get("event_type").and_then(Value::as_str) {
                Some("question_presented") => {
                    if let (Some(turn), Some(question_text)) = (
                        value.get("turn").and_then(Value::as_u64),
                        value.get("question_text").and_then(Value::as_str),
                    ) {
                        questions.insert(turn, question_text.to_string());
                    }
                }
                Some("answer_recorded") => {
                    if let Some(turn) = value.get("turn").and_then(Value::as_u64) {
                        let question = questions.get(&turn).cloned().unwrap_or_else(|| {
                            json_string(&value, "question_ref").unwrap_or_default()
                        });
                        last_question_answered = Some(question);
                    }
                }
                _ => {}
            }
        }

        if started_at.is_none() && last_active_at.is_none() {
            return Ok(None);
        }

        Ok(Some(Self {
            session_id,
            path: path.to_path_buf(),
            started_at,
            last_active_at,
            branch_id,
            last_question_answered,
        }))
    }
}

pub(crate) fn fork_session(config: &CliConfig, output: &mut impl Write) -> Result<()> {
    // trace:STORY-19 | ai:codex
    let proposition = config
        .proposition
        .as_deref()
        .ok_or_else(|| QuizdomError::Usage("session fork requires --proposition".to_string()))?;
    let agree_seed = config
        .agree_seed
        .as_deref()
        .ok_or_else(|| QuizdomError::Usage("session fork requires --agree-seed".to_string()))?;
    let disagree_seed = config
        .disagree_seed
        .as_deref()
        .ok_or_else(|| QuizdomError::Usage("session fork requires --disagree-seed".to_string()))?;

    let mut logger = SessionLogger::open(&config.log_path)?;
    logger.branch_forked(
        &config.session_id,
        &config.user_id,
        proposition,
        agree_seed,
        disagree_seed,
    )?;
    writeln!(
        output,
        "Forked proposition into agree -> {agree_seed} and disagree -> {disagree_seed}."
    )?;
    Ok(())
}

pub(crate) struct SessionLogger {
    file: fs::File,
    next_event: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct ReplayedAnswer {
    pub(crate) turn: u64,
    pub(crate) question_ref: String,
    pub(crate) question_text: String,
    pub(crate) raw_answer: String,
    pub(crate) normalized_answer: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct SessionReplay {
    pub(crate) branch_id: String,
    pub(crate) answers: Vec<ReplayedAnswer>,
    pub(crate) next_question_ref: Option<String>,
    pub(crate) next_turn: u64,
}

impl SessionReplay {
    pub(crate) fn load(path: &Path, branch_id: &str) -> Result<Self> {
        let file = File::open(path)?;
        Self::from_reader(file, branch_id)
    }

    pub(crate) fn from_reader(reader: impl Read, branch_id: &str) -> Result<Self> {
        let reader = BufReader::new(reader);
        let mut questions = BTreeMap::new();
        let mut answers = Vec::new();
        let mut next_question_ref = None;

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&line)
                .map_err(|error| QuizdomError::Parse(error.to_string()))?;
            match value.get("event_type").and_then(Value::as_str) {
                Some("branch_forked") => {
                    if let Some(seed) = fork_seed_for_branch(&value, branch_id)? {
                        next_question_ref = Some(seed);
                    }
                }
                Some("question_presented") => {
                    if event_branch(&value) != branch_id {
                        continue;
                    }
                    if let (Some(turn), Some(question_text)) = (
                        value.get("turn").and_then(Value::as_u64),
                        value.get("question_text").and_then(Value::as_str),
                    ) {
                        questions.insert(turn, question_text.to_string());
                    }
                }
                Some("answer_recorded") => {
                    if event_branch(&value) != branch_id {
                        continue;
                    }
                    let turn = json_u64(&value, "turn")?;
                    let question_ref = json_string(&value, "question_ref")?;
                    if next_question_ref.as_deref() == Some(question_ref.as_str()) {
                        next_question_ref = None;
                    }
                    let question_text = questions.get(&turn).cloned().unwrap_or_default();
                    answers.push(ReplayedAnswer {
                        turn,
                        question_ref,
                        question_text,
                        raw_answer: json_string(&value, "raw_answer")?,
                        normalized_answer: json_string(&value, "normalized_answer")?,
                    });
                }
                Some("path_truncated") => {
                    if event_branch(&value) != branch_id {
                        continue;
                    }
                    let from_turn = json_u64(&value, "from_turn")?;
                    questions.retain(|turn, _| *turn < from_turn);
                    answers.retain(|answer| answer.turn < from_turn);
                    next_question_ref = None;
                }
                Some("next_question_selected") => {
                    if event_branch(&value) != branch_id {
                        continue;
                    }
                    next_question_ref = Some(json_string(&value, "selected_next_question_ref")?);
                }
                Some("session_ended") => {}
                _ => {}
            }
        }

        let next_turn = answers.last().map(|answer| answer.turn + 1).unwrap_or(0);

        Ok(Self {
            branch_id: branch_id.to_string(),
            answers,
            next_question_ref,
            next_turn,
        })
    }

    pub(crate) fn render(&self, output: &mut impl Write) -> Result<()> {
        writeln!(
            output,
            "Replaying previous session path for branch '{}':",
            self.branch_id
        )?;
        if self.answers.is_empty() {
            writeln!(output, "(no answered questions yet)")?;
        }
        for answer in &self.answers {
            writeln!(output, "\n[turn {}] {}", answer.turn, answer.question_text)?;
            writeln!(output, "question_ref: {}", answer.question_ref)?;
            writeln!(output, "answer: {}", answer.raw_answer)?;
        }
        Ok(())
    }

    pub(crate) fn render_recap(&self, output: &mut impl Write) -> Result<()> {
        writeln!(output, "RECAP:")?;
        writeln!(output, "branch: {}", self.branch_id)?;
        if let Some(answer) = self.answers.last() {
            writeln!(output, "last question: {}", answer.question_text)?;
            writeln!(output, "your answer: {}", answer.raw_answer)?;
        } else {
            writeln!(output, "last question: (none answered yet)")?;
        }
        Ok(())
    }

    fn recent_path(&self) -> Vec<AnsweredQuestion> {
        self.answers
            .iter()
            .map(|answer| AnsweredQuestion {
                question_ref: answer.question_ref.clone(),
                question_text: answer.question_text.clone(),
                raw_answer: answer.raw_answer.clone(),
                normalized_answer: answer.normalized_answer.clone(),
            })
            .collect()
    }
}

fn fork_seed_for_branch(value: &Value, branch_id: &str) -> Result<Option<String>> {
    let Some(branches) = value.get("branches").and_then(Value::as_array) else {
        return Ok(None);
    };
    for branch in branches {
        if branch.get("branch_id").and_then(Value::as_str) == Some(branch_id) {
            return Ok(Some(json_string(branch, "seed_question_ref")?));
        }
    }
    Ok(None)
}

fn event_branch(value: &Value) -> &str {
    value
        .get("branch_id")
        .and_then(Value::as_str)
        .unwrap_or("main")
}

fn json_string(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| QuizdomError::Parse(format!("session log event missing {key}")))
}

fn json_u64(value: &Value, key: &str) -> Result<u64> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| QuizdomError::Parse(format!("session log event missing {key}")))
}

impl SessionLogger {
    fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let next_event = next_event_number(path)?;
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file, next_event })
    }

    #[allow(clippy::too_many_arguments)]
    fn session_started(
        &mut self,
        session_id: &str,
        user_id: &str,
        branch_id: &str,
        seed_question_ref: &str,
        strategy: StrategyKind,
        llm_backend: LlmBackendKind,
        // trace:STORY-159 | ai:claude — the goal set at start (`--goal`), recorded
        // so resume can restore it and the arc/synopsis can orient to it.
        goal: Option<&str>,
        // trace:STORY-161 | ai:claude — the mode set at start (`--mode`), recorded
        // so resume restores it and the verdict path frames the debate correctly.
        mode: SessionMode,
    ) -> Result<()> {
        let event_id = self.event_id();
        let llm_backend_value = (strategy == StrategyKind::Llm).then(|| llm_backend.as_str());
        let llm_model = (strategy == StrategyKind::Llm)
            .then(|| llm_model_for_log(llm_backend))
            .flatten();
        self.write(json!({
            "event_id": event_id,
            "event_type": "session_started",
            "occurred_at": Utc::now().to_rfc3339(),
            "session_id": session_id,
            "user_id": user_id,
            "branch_id": branch_id,
            "seed_question_ref": seed_question_ref,
            "strategy": strategy.as_str(),
            "llm_backend": llm_backend_value,
            "llm_model": llm_model,
            "goal": goal,
            "mode": mode.as_str(),
        }))
    }

    // trace:STORY-159 | ai:claude
    /// Record a goal set (or changed) IN-SESSION — via the `/goal` command or an
    /// accepted Observer proposal. The most recent `goal_set` event is what the
    /// arc/synopsis and resume restore read, so this is how an in-session goal
    /// overrides a `--goal` flag (or a free-flowing start).
    fn goal_set(
        &mut self,
        session_id: &str,
        user_id: &str,
        branch_id: &str,
        turn: u64,
        goal: &str,
        source: &str,
    ) -> Result<()> {
        let event_id = self.event_id();
        self.write(json!({
            "event_id": event_id,
            "event_type": "goal_set",
            "occurred_at": Utc::now().to_rfc3339(),
            "session_id": session_id,
            "user_id": user_id,
            "branch_id": branch_id,
            "turn": turn,
            "goal": goal,
            "source": source,
        }))
    }

    // trace:STORY-161 | ai:claude
    /// Record a session MODE toggled IN-SESSION via the `/mode` command. The most
    /// recent `mode_set` event is what the arc/verdict and resume restore read, so
    /// this is how an in-session toggle overrides the `--mode` flag (or the
    /// default). Belief-neutral: the mode picks the questioning style, never a
    /// belief.
    fn mode_set(
        &mut self,
        session_id: &str,
        user_id: &str,
        branch_id: &str,
        turn: u64,
        mode: SessionMode,
    ) -> Result<()> {
        let event_id = self.event_id();
        self.write(json!({
            "event_id": event_id,
            "event_type": "mode_set",
            "occurred_at": Utc::now().to_rfc3339(),
            "session_id": session_id,
            "user_id": user_id,
            "branch_id": branch_id,
            "turn": turn,
            "mode": mode.as_str(),
        }))
    }

    // trace:STORY-160 | ai:claude
    /// Record the PHASE TRANSITION into the closing ritual (the session stops
    /// asking questions and switches to closing statements). `caller` records who
    /// rested the case (`"user"`; the challenger never rests first). Logged so a
    /// resumed/inspected session can see where the closing ritual began.
    fn phase_changed(
        &mut self,
        session_id: &str,
        user_id: &str,
        branch_id: &str,
        turn: u64,
        phase: &str,
        caller: &str,
    ) -> Result<()> {
        let event_id = self.event_id();
        self.write(json!({
            "event_id": event_id,
            "event_type": "phase_changed",
            "occurred_at": Utc::now().to_rfc3339(),
            "session_id": session_id,
            "user_id": user_id,
            "branch_id": branch_id,
            "turn": turn,
            "phase": phase,
            "caller": caller,
        }))
    }

    // trace:STORY-160 | ai:claude
    /// Record one CLOSING STATEMENT in the closing ritual: either the user's
    /// settled position (`speaker:"user"`) or the challenger's strongest remaining
    /// objection (`speaker:"challenger"`). The `final_word` flag marks the last
    /// statement made under the terminator-forfeits-last-word fairness rule.
    #[allow(clippy::too_many_arguments)]
    fn closing_statement(
        &mut self,
        session_id: &str,
        user_id: &str,
        branch_id: &str,
        turn: u64,
        speaker: &str,
        statement: &str,
        final_word: bool,
    ) -> Result<()> {
        let event_id = self.event_id();
        self.write(json!({
            "event_id": event_id,
            "event_type": "closing_statement",
            "occurred_at": Utc::now().to_rfc3339(),
            "session_id": session_id,
            "user_id": user_id,
            "branch_id": branch_id,
            "turn": turn,
            "speaker": speaker,
            "statement": statement,
            "final_word": final_word,
        }))
    }

    fn question_presented(
        &mut self,
        session_id: &str,
        user_id: &str,
        branch_id: &str,
        turn: u64,
        question: &Question,
    ) -> Result<()> {
        let event_id = self.event_id();
        self.write(json!({
            "event_id": event_id,
            "event_type": "question_presented",
            "occurred_at": Utc::now().to_rfc3339(),
            "session_id": session_id,
            "user_id": user_id,
            "branch_id": branch_id,
            "turn": turn,
            "question_ref": question.id,
            "question_text": question.title,
            "answer_mode": question.answer_kind.mode(),
        }))
    }

    fn answer_recorded(
        &mut self,
        session_id: &str,
        user_id: &str,
        branch_id: &str,
        turn: u64,
        question: &Question,
        answer: &Answer,
    ) -> Result<()> {
        let event_id = self.event_id();
        self.write(json!({
            "event_id": event_id,
            "event_type": "answer_recorded",
            "occurred_at": Utc::now().to_rfc3339(),
            "session_id": session_id,
            "user_id": user_id,
            "branch_id": branch_id,
            "turn": turn,
            "question_ref": question.id,
            "answer_mode": question.answer_kind.mode(),
            "raw_answer": answer.raw,
            "normalized_answer": answer.normalized,
        }))
    }

    fn contradiction_resolved(
        &mut self,
        session_id: &str,
        user_id: &str,
        branch_id: &str,
        turn: u64,
        contradiction: &Contradiction,
        answer: &Answer,
        resolution: Option<&ContradictionResolution>,
    ) -> Result<()> {
        // trace:STORY-59 | ai:codex
        let event_id = self.event_id();
        self.write(json!({
            "event_id": event_id,
            "event_type": "contradiction_resolved",
            "occurred_at": Utc::now().to_rfc3339(),
            "session_id": session_id,
            "user_id": user_id,
            "branch_id": branch_id,
            "turn": turn,
            "left_belief_ref": contradiction.left_id,
            "left_belief": contradiction.left,
            "right_belief_ref": contradiction.right_id,
            "right_belief": contradiction.right,
            "raw_resolution": answer.raw,
            "normalized_resolution": answer.normalized,
            "kept_side": resolution.map(|resolution| resolution.kept_side.as_str()),
            "graph_ref": resolution.and_then(|resolution| resolution.graph_ref.as_deref()),
        }))
    }

    fn path_truncated(
        &mut self,
        session_id: &str,
        user_id: &str,
        branch_id: &str,
        from_turn: u64,
        reason: &str,
    ) -> Result<()> {
        let event_id = self.event_id();
        self.write(json!({
            "event_id": event_id,
            "event_type": "path_truncated",
            "occurred_at": Utc::now().to_rfc3339(),
            "session_id": session_id,
            "user_id": user_id,
            "branch_id": branch_id,
            "from_turn": from_turn,
            "reason": reason,
        }))
    }

    fn term_interpreted(
        &mut self,
        session_id: &str,
        user_id: &str,
        branch_id: &str,
        turn: u64,
        settled: &SettledTermDefinition,
        definitions: &[TermDefinition],
    ) -> Result<()> {
        let event_id = self.event_id();
        self.write(json!({
            "event_id": event_id,
            "event_type": "term_interpreted",
            "occurred_at": Utc::now().to_rfc3339(),
            "session_id": session_id,
            "user_id": user_id,
            "branch_id": branch_id,
            "turn": turn,
            "term": settled.term_label,
            "term_ref": settled.term.id,
            "term_refs": definitions.iter().map(|definition| definition.id.as_str()).collect::<Vec<_>>(),
            "raw_definition": settled.raw_meaning,
            "adopted_title": settled.term.title,
            "adopted_definition": settled.term.definition,
        }))
    }

    fn next_question_selected(
        &mut self,
        session_id: &str,
        user_id: &str,
        branch_id: &str,
        turn: u64,
        question_ref: &str,
        selected_next_question_ref: &str,
        selection_reason: &str,
    ) -> Result<()> {
        let event_id = self.event_id();
        self.write(json!({
            "event_id": event_id,
            "event_type": "next_question_selected",
            "occurred_at": Utc::now().to_rfc3339(),
            "session_id": session_id,
            "user_id": user_id,
            "branch_id": branch_id,
            "turn": turn,
            "question_ref": question_ref,
            "selected_next_question_ref": selected_next_question_ref,
            "selection_reason": selection_reason,
        }))
    }

    fn session_ended(
        &mut self,
        session_id: &str,
        user_id: &str,
        branch_id: &str,
        turn: u64,
        summary: &str,
    ) -> Result<()> {
        let event_id = self.event_id();
        self.write(json!({
            "event_id": event_id,
            "event_type": "session_ended",
            "occurred_at": Utc::now().to_rfc3339(),
            "session_id": session_id,
            "user_id": user_id,
            "branch_id": branch_id,
            "turn": turn,
            "summary": summary,
        }))
    }

    fn branch_forked(
        &mut self,
        session_id: &str,
        user_id: &str,
        proposition: &str,
        agree_seed: &str,
        disagree_seed: &str,
    ) -> Result<()> {
        let event_id = self.event_id();
        self.write(json!({
            "event_id": event_id,
            "event_type": "branch_forked",
            "occurred_at": Utc::now().to_rfc3339(),
            "session_id": session_id,
            "user_id": user_id,
            "proposition": proposition,
            "branches": [
                {
                    "branch_id": "agree",
                    "stance": "agree",
                    "seed_question_ref": agree_seed,
                },
                {
                    "branch_id": "disagree",
                    "stance": "disagree",
                    "seed_question_ref": disagree_seed,
                }
            ],
        }))
    }

    fn event_id(&mut self) -> String {
        let event_id = format!("evt-{:06}", self.next_event);
        self.next_event += 1;
        event_id
    }

    fn write(&mut self, value: serde_json::Value) -> Result<()> {
        serde_json::to_writer(&mut self.file, &value)
            .map_err(|error| QuizdomError::Parse(error.to_string()))?;
        writeln!(self.file)?;
        self.file.flush()?;
        Ok(())
    }
}

fn next_event_number(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(1);
    }

    let file = File::open(path)?;
    let count = BufReader::new(file)
        .lines()
        .filter(|line| {
            line.as_ref()
                .map(|line| !line.trim().is_empty())
                .unwrap_or(false)
        })
        .count();
    Ok(count as u64 + 1)
}

// trace:STORY-156 | ai:claude
#[cfg(test)]
mod conclude_tests {
    use super::*;

    fn ask(line: &str) -> bool {
        let mut input = std::io::Cursor::new(format!("{line}\n"));
        let mut free_text = FreeTextInput::Plain;
        let mut out = Vec::new();
        prompt_to_conclude(&mut input, &mut free_text, &mut out).expect("prompt")
    }

    #[test]
    fn affirmative_answers_conclude() {
        // Only an explicit affirmative concludes — the offer is honoured.
        for yes in ["c", "conclude", "y", "yes", "Conclude", "YES"] {
            assert!(ask(yes), "{yes:?} should conclude");
        }
    }

    #[test]
    fn anything_else_keeps_exploring() {
        // Agency: the default is to keep exploring. A blank line, "keep", "no",
        // or noise must NOT auto-conclude against the user's wishes.
        for keep in ["", "k", "keep", "n", "no", "edge cases", "probe"] {
            assert!(!ask(keep), "{keep:?} should keep exploring");
        }
    }

    #[test]
    fn eof_offline_does_not_conclude() {
        // Non-TTY / piped / EOF: degrade gracefully — never conclude on the
        // user's behalf when there is no answer to read.
        let mut input = std::io::Cursor::new(Vec::new());
        let mut free_text = FreeTextInput::Plain;
        let mut out = Vec::new();
        assert!(!prompt_to_conclude(&mut input, &mut free_text, &mut out).expect("prompt"));
    }

    // ---- STORY-163: /help + /tutor graceful placeholders -------------------

    #[test]
    fn help_placeholder_is_belief_neutral_and_about_the_tool() {
        // trace:STORY-163 | ai:claude — the /help channel is belief-neutral by
        // construction: the note talks only about the TOOL/process, points at the
        // palette's per-command `?` help, and never references any belief content.
        let mut out = Vec::new();
        render_help_placeholder("how do I rest my case?", &mut out).expect("render");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("/help"));
        assert!(text.to_lowercase().contains("belief-neutral"));
        assert!(text.contains("how do I rest my case?"));
        // It points the user at the already-live per-command help.
        assert!(text.contains("'?'") || text.contains("palette"));
    }

    #[test]
    fn help_placeholder_without_a_question_still_guides_the_user() {
        let mut out = Vec::new();
        render_help_placeholder("", &mut out).expect("render");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("/help"));
        assert!(text.to_lowercase().contains("controls") || text.to_lowercase().contains("flow"));
    }

    #[test]
    fn tutor_placeholder_promises_to_sharpen_the_users_point_without_supplying_a_belief() {
        // trace:STORY-163 | ai:claude — the /tutor note reflects + sharpens the
        // user's OWN point and names missing nuance, and explicitly never tells the
        // user what to believe — the belief-neutral guarantee, surfaced even before
        // the LLM engine (STORY-165) lands.
        let mut out = Vec::new();
        render_tutor_placeholder("free will is uncaused choice", &mut out).expect("render");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("/tutor"));
        assert!(text.to_lowercase().contains("nuance"));
        assert!(text
            .to_lowercase()
            .contains("without ever telling you what to believe"));
        // It echoes the user's OWN point back (to be sharpened), never supplying a
        // belief or taking a side.
        assert!(text.contains("free will is uncaused choice"));
        assert!(!text.to_lowercase().contains("you should believe"));
    }
}
