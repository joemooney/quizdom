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
// trace:STORY-168 | ai:claude — the engine now talks to the FrontEnd seam; the
// raw input readers (read_answer_or_end / FreeTextInput) live behind LineFrontEnd.
use crate::frontend::FrontEnd;
use crate::input::{
    render_breadcrumb, render_question, render_question_for, AnswerInput, InputContext,
};
use crate::model::{Answer, AnswerKind, Question, TermDefinition};
// trace:STORY-127 | ai:claude
use crate::observer::{read_exchange, structural_reading, Exchange, ExchangeReading};
// trace:STORY-164 | ai:claude
use crate::observer::{answer_help, HelpAnswer};
// trace:STORY-165 | ai:claude
use crate::observer::{read_tutor, TutorContext, TutorReading};
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
    arc_from_session_log, render_synopsis, structural_synopsis, synopsize, ScoreGauge, SessionArc,
    SessionSynopsis, SCORE_GATE_TURNS,
};
use chrono::Utc;
use llm::{AnthropicClient, ClaudeCliClient};
use serde_json::json;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
// trace:STORY-169 | ai:claude — `IsTerminal` drives the front-end selection.
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
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
    // trace:STORY-169 | ai:claude
    /// Force the HEADLESS line front-end even on an interactive TTY (`--no-tui`).
    /// EPIC-167 / ADR-166 make the ratatui TUI the default for interactive
    /// `start`/`resume`/`fork`; this escape hatch keeps the old line UI for
    /// scripting-adjacent interactive use, demos, or a terminal the TUI misreads.
    /// Non-TTY streams already auto-select headless regardless of this flag.
    pub(crate) no_tui: bool,
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
        // trace:STORY-169 | ai:claude — default off: interactive sessions get the TUI.
        let mut no_tui = false;
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
                // trace:STORY-169 | ai:claude — `--no-tui` forces the headless line
                // front-end even on a TTY (the TUI is otherwise the interactive
                // default). A bare flag, no value.
                "--no-tui" => no_tui = true,
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
            // trace:STORY-169 | ai:claude
            no_tui,
        })
    }
}

impl CliConfig {
    // trace:STORY-169 | ai:claude
    /// Whether this command runs an INTERACTIVE session (so the ratatui TUI is a
    /// candidate front-end). `start`/`resume`/`fork` are interactive; `list` (and
    /// the standalone commands routed elsewhere) are not. Used by the front-end
    /// selection so only the interactive paths can pick up the TUI.
    pub(crate) fn is_interactive(&self) -> bool {
        matches!(
            self.command,
            SessionCommand::Start | SessionCommand::Resume | SessionCommand::Fork
        )
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
    output: &mut dyn Write,
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
        "  --no-tui                            Force the headless line UI (skip the ratatui TUI)",
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

pub(crate) fn list_sessions(config: &CliConfig, output: &mut dyn Write) -> Result<()> {
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
    // trace:STORY-173 | ai:claude — a test-only backend that returns a CANNED
    // goal proposal (or `None`), so the user-requested + interrogator goal-offer
    // flows can be exercised end-to-end without a live LLM. Belief-neutral: the
    // canned proposal is still a QUESTION to settle, never a belief.
    #[cfg(test)]
    Mock(Option<crate::observer::GoalProposal>),
    // trace:STORY-175 | ai:claude — a test-only backend that returns a CANNED
    // `/judge` ruling, so the SUSTAINED/OVERRULED ruling + open-thread tracking can
    // be exercised end-to-end without a live LLM. For every other method it behaves
    // like a present (non-offline) LLM backend degrading structurally — the
    // objection tests only drive the judge path. Belief-neutral: the canned ruling
    // judges STRUCTURE, never which belief is true.
    #[cfg(test)]
    MockJudge(crate::observer::JudgeRuling),
}

// trace:BUG-181 | ai:claude — a test-build regression guard against a live LLM
// leak. `run_session_from_current` always builds its observer via
// `ObserverEngine::for_config`, and `test_config` defaults the backend to
// `ClaudeCli` (the production default). Without this guard, any test that drives
// a path which calls `synopsize`/`read`/… on a live backend (e.g. the score-gauge
// gate test crossing `SCORE_GATE_TURNS`) would SPAWN the real `claude` CLI —
// ~60s and flaky, and an unwanted LLM charge during `cargo test`. So in TEST
// builds `for_config` refuses to construct a network-backed engine and returns
// `Offline` instead (structural readings, identical degraded output), unless a
// test explicitly opts in via `allow_live_backend`. The opt-in exists only so a
// future test that genuinely wants the live path can take it deliberately; the
// ignored API-billed smoke tests construct their clients directly and never route
// through `for_config`, so they are unaffected. No production behavior changes:
// outside `cfg(test)` this is a plain backend match.
#[cfg(test)]
thread_local! {
    static ALLOW_LIVE_BACKEND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
impl ObserverEngine {
    /// Opt the current test thread into a real network-backed `for_config`
    /// engine. Off by default so no test can accidentally reach a live LLM.
    /// Returns a guard that restores the previous setting on drop.
    fn allow_live_backend() -> impl Drop {
        struct Restore(bool);
        impl Drop for Restore {
            fn drop(&mut self) {
                ALLOW_LIVE_BACKEND.with(|c| c.set(self.0));
            }
        }
        let prev = ALLOW_LIVE_BACKEND.with(|c| c.replace(true));
        Restore(prev)
    }

    fn live_backend_allowed() -> bool {
        ALLOW_LIVE_BACKEND.with(std::cell::Cell::get)
    }
}

impl ObserverEngine {
    /// Build the observer for a session from its configured LLM backend.
    ///
    /// The default backend is claude-cli (always constructible); the Anthropic
    /// backend degrades to `Offline` when its API key is absent, so an offline
    /// machine still gets the structural note rather than a failure.
    fn for_config(config: &CliConfig) -> Self {
        // trace:BUG-181 | ai:claude — fail-safe to `Offline` under test so the
        // session loop can never spawn a live LLM (see the guard note above).
        #[cfg(test)]
        if !Self::live_backend_allowed() {
            return Self::Offline;
        }
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
            #[cfg(test)]
            Self::Mock(_) => structural_reading(exchange),
            #[cfg(test)]
            Self::MockJudge(_) => structural_reading(exchange),
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
            #[cfg(test)]
            Self::Mock(_) => structural_synopsis(arc),
            #[cfg(test)]
            Self::MockJudge(_) => structural_synopsis(arc),
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
            // trace:STORY-173 | ai:claude — the canned proposal, gated on having
            // SOME positions (mirrors the real path, which never proposes from an
            // empty conversation: `propose_goal` returns `None` with no positions).
            #[cfg(test)]
            Self::Mock(proposal) => {
                if positions.is_empty() {
                    None
                } else {
                    proposal.clone()
                }
            }
            #[cfg(test)]
            Self::MockJudge(_) => None,
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
            #[cfg(test)]
            Self::Mock(_) => crate::observer::structural_objection(position, goal),
            #[cfg(test)]
            Self::MockJudge(_) => crate::observer::structural_objection(position, goal),
        }
    }

    // trace:STORY-164 | ai:claude
    /// Answer a free-form `/help` process question from TOOL-CONTEXT (the design),
    /// belief-neutral. Uses the LLM when present and falls back to the STATIC help
    /// index offline, so `/help` always answers, online or off. The PROCESS-HELP
    /// counterpart to [`read`]. Belief-neutral by construction: the tool-context
    /// carries no belief content and the system prompt forbids engaging any belief.
    fn help(&self, question: &str) -> HelpAnswer {
        let tool_context = help_tool_context();
        match self {
            Self::ClaudeCli(client) => answer_help(client, question, &tool_context),
            Self::Anthropic(client) => answer_help(client, question, &tool_context),
            Self::Offline => crate::observer::static_help_index(question, &tool_context),
            #[cfg(test)]
            Self::Mock(_) => crate::observer::static_help_index(question, &tool_context),
            #[cfg(test)]
            Self::MockJudge(_) => crate::observer::static_help_index(question, &tool_context),
        }
    }

    // trace:STORY-165 | ai:claude
    /// Coach the user's articulation with `/tutor`: reflect their OWN point back
    /// more precisely, teach the relevant distinction, and name the missing nuance.
    /// Uses the LLM when present and falls back to the STRUCTURAL coaching note
    /// offline, so `/tutor` always responds, online or off. The ARTICULATION
    /// counterpart to [`read`]. Belief-neutral by construction: the system prompt
    /// and the structural fallback both refuse to supply a belief or take a side.
    fn tutor(&self, context: &TutorContext) -> TutorReading {
        match self {
            Self::ClaudeCli(client) => read_tutor(client, context),
            Self::Anthropic(client) => read_tutor(client, context),
            Self::Offline => crate::observer::structural_tutor(context),
            #[cfg(test)]
            Self::Mock(_) => crate::observer::structural_tutor(context),
            #[cfg(test)]
            Self::MockJudge(_) => crate::observer::structural_tutor(context),
        }
    }

    // trace:STORY-175 | ai:claude
    /// Rule on a `/judge`-ed objection: the belief-neutral SUSTAINED/OVERRULED
    /// ruling + resolving condition. Uses the LLM when present and falls back to the
    /// structural ruling offline (though `/judge` is gated upstream to report "needs
    /// an LLM backend" before reaching here when offline). The OBJECTION-RULING
    /// counterpart to [`read`]. Belief-neutral: judges STRUCTURE, never which belief
    /// is true.
    fn judge(
        &self,
        objection: &str,
        goal: Option<&str>,
        context: &str,
    ) -> crate::observer::JudgeRuling {
        match self {
            Self::ClaudeCli(client) => {
                crate::observer::read_judge_ruling(client, objection, goal, context)
            }
            Self::Anthropic(client) => {
                crate::observer::read_judge_ruling(client, objection, goal, context)
            }
            Self::Offline => crate::observer::structural_judge_ruling(objection, goal),
            #[cfg(test)]
            Self::Mock(_) => crate::observer::structural_judge_ruling(objection, goal),
            // trace:STORY-175 | ai:claude — return the CANNED ruling so the
            // SUSTAINED/OVERRULED paths + open-thread tracking are testable.
            #[cfg(test)]
            Self::MockJudge(ruling) => ruling.clone(),
        }
    }

    // trace:STORY-175 | ai:claude
    /// Whether the interrogator should RAISE its own `/objection` this turn (the
    /// bounded self-objection). Uses the LLM when present; offline / no-backend it
    /// never objects (returns `None`). Belief-neutral: the objection it raises names
    /// a STRUCTURAL tension, never a belief.
    #[cfg(test)]
    fn interrogator_objection(&self, positions: &[String]) -> Option<String> {
        match self {
            Self::ClaudeCli(client) => {
                crate::observer::propose_interrogator_objection(client, positions)
            }
            Self::Anthropic(client) => {
                crate::observer::propose_interrogator_objection(client, positions)
            }
            Self::Offline => None,
            // trace:STORY-175 | ai:claude — a Mock reuses the goal-proposal canned
            // value to decide whether to object: a `Some` proposal yields an
            // objection (rare, gated on ≥2 positions like the real path); `None`
            // stays quiet.
            Self::Mock(proposal) => {
                if positions.len() < 2 {
                    return None;
                }
                proposal
                    .as_ref()
                    .map(|p| format!("material unaddressed tension re: {}", p.goal))
            }
            // The judge-mock never self-objects (the judge tests drive /judge only).
            Self::MockJudge(_) => None,
        }
    }

    #[cfg(not(test))]
    fn interrogator_objection(&self, positions: &[String]) -> Option<String> {
        match self {
            Self::ClaudeCli(client) => {
                crate::observer::propose_interrogator_objection(client, positions)
            }
            Self::Anthropic(client) => {
                crate::observer::propose_interrogator_objection(client, positions)
            }
            Self::Offline => None,
        }
    }

    // trace:STORY-173 | ai:claude
    /// True when this engine has NO LLM backend reachable, so a goal proposal is
    /// impossible. Drives the offline degrade on the on-demand `/goal` /
    /// `/request-goal` paths: instead of proposing, the session reports "no goal"
    /// with a "needs an LLM backend" note rather than silently doing nothing.
    fn is_offline(&self) -> bool {
        match self {
            Self::Offline => true,
            Self::ClaudeCli(_) | Self::Anthropic(_) => false,
            #[cfg(test)]
            Self::Mock(_) => false,
            #[cfg(test)]
            Self::MockJudge(_) => false,
        }
    }
}

// trace:STORY-164 | ai:claude
/// Build the TOOL-CONTEXT the `/help` channel answers from: one line per command
/// in the palette registry (the single source of truth for the controls), pairing
/// each command with its one-line description. This is the DESIGN of the tool —
/// the controls and what they do — and it carries no belief content, so `/help`
/// answers (online or via the offline static index) stay strictly belief-neutral.
/// Sourcing it from the same registry the palette renders keeps `/help` in sync
/// with the live command set automatically.
fn help_tool_context() -> String {
    let mut context = String::from(
        "quizdom is a Socratic belief-exploration tool. At each question you can answer, \
         or use these controls (a bare '/' opens a palette of them):\n",
    );
    for command in crate::palette::command_registry() {
        context.push_str(&format!("{} — {}\n", command.command, command.description));
    }
    context
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
    output: &mut dyn Write,
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

// trace:STORY-174 | ai:claude
/// Compute a fresh [`ScoreGauge`] reading at a GATE: read the session arc from
/// the log (scoped to the branch + the live goal), run the synopsis LLM pass, and
/// derive the gauge. The synopsis already scores roundedness WITH RESPECT TO the
/// goal when one is set (STORY-159), so a goal in the arc yields a distance-to-goal
/// reading; no goal yields general roundedness. Offline / not-logged-in the
/// synopsis degrades and the gauge reads "needs LLM" rather than a fabricated %.
/// Belief-neutral: the score reads STRUCTURE / progress, never belief-correctness.
fn compute_score_gauge(
    observer: &ObserverEngine,
    log_path: &Path,
    branch: Option<&str>,
    goal: Option<&str>,
    // trace:STORY-175 | ai:claude — the most recent SUSTAINED objection's tracked
    // open thread, folded into the gauge so it WIDENS the distance-to-goal until
    // addressed; `None` when no objection is tracked. Belief-neutral: a structural
    // gap, never a belief.
    open_thread: Option<&str>,
) -> ScoreGauge {
    let mut arc = match File::open(log_path) {
        Ok(file) => arc_from_session_log(file, branch).unwrap_or_default(),
        Err(_) => SessionArc::default(),
    };
    // The live goal (set this run via `/goal`, perhaps not yet flushed/visible in
    // the arc the same way) wins, so the gauge scopes to exactly what the
    // breadcrumb shows. Belief-neutral: the goal is the question being resolved.
    if let Some(goal) = goal.map(str::trim).filter(|g| !g.is_empty()) {
        arc.goal = Some(goal.to_string());
    }
    let synopsis = {
        let _spinner = crate::spinner::Spinner::start("scoring");
        observer.synopsize(&arc)
    };
    let gauge = ScoreGauge::from_synopsis(&synopsis);
    // trace:STORY-175 | ai:claude — fold the tracked sustained-objection open thread
    // into the gauge (widens the distance-to-goal); a no-op when there is none.
    match open_thread.map(str::trim).filter(|t| !t.is_empty()) {
        Some(thread) => gauge.with_open_thread(thread),
        None => gauge,
    }
}

// trace:STORY-174 | ai:claude
/// Render the persistent score gauge to the front-end output. Emits TWO things:
///
/// 1. A machine-readable `[score: <body>]` line the TUI status bar mirrors into
///    its gauge segment (the same out-of-band channel the breadcrumb uses).
/// 2. A human-readable META footer line for the HEADLESS path, so a non-TTY /
///    `--no-tui` run shows the gauge in its footer (the TUI hides the raw bracket
///    line via the status bar; the footer reads naturally either way).
///
/// `fresh` marks a live (gate) computation vs a cached value shown between gates.
/// Belief-neutral throughout: the gauge reads STRUCTURE / distance-to-goal.
fn render_score_gauge(gauge: &ScoreGauge, fresh: bool, output: &mut dyn Write) -> Result<()> {
    let segment = gauge.status_segment(fresh);
    // The bracketed line the TUI parses (mirrors the `[topic: …]` breadcrumb
    // channel). It is plain structural chrome, not a voice.
    writeln!(output, "[{segment}]")?;
    // The headless-facing footer: the same reading in the META voice.
    writeln!(
        output,
        "{}",
        crate::style::paint(crate::style::meta(), &format!("  {segment}"))
    )?;
    Ok(())
}

// trace:STORY-174 | ai:claude
/// Emit the gauge-OFF marker so the TUI status bar clears its score segment when
/// `/score` toggles the gauge off, plus a headless confirmation line.
fn render_score_gauge_off(output: &mut dyn Write) -> Result<()> {
    writeln!(output, "[score: off]")?;
    writeln!(
        output,
        "{}",
        crate::style::paint(crate::style::meta(), "  Score gauge off.")
    )?;
    Ok(())
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
// trace:STORY-168 | ai:claude — front-end seam.
fn prompt_to_conclude(fe: &mut dyn FrontEnd) -> Result<bool> {
    let prompt = "Conclude with a summary? [c]onclude / [k]eep exploring (default keep): ";
    let choice = match fe.read_line(prompt)? {
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
    output: &mut dyn Write,
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
    output: &mut dyn Write,
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
/// Flatten an arc's recorded turns into the belief-neutral POSITION strings the
/// goal-proposal prompt reads. Each turn becomes `On "<question>": <position>`
/// (or just the bare position when no question is attached); empty positions are
/// dropped. Shared by every goal-proposal path so they read the conversation the
/// same way.
fn positions_from_arc(arc: &SessionArc) -> Vec<String> {
    arc.turns
        .iter()
        .filter(|turn| !turn.position.is_empty())
        .map(|turn| {
            if turn.question.is_empty() {
                turn.position.clone()
            } else {
                format!("On \"{}\": {}", turn.question, turn.position)
            }
        })
        .collect()
}

// trace:STORY-173 | ai:claude
/// Build the session arc for an on-demand goal request: re-read the whole session
/// log so the proposal sees every recorded position so far (the same source the
/// synopsis reads). A missing / unreadable log yields an empty arc, so the caller
/// degrades to "nothing recorded yet" rather than failing.
fn arc_for_goal_request(log_path: &Path, branch: &str) -> SessionArc {
    match File::open(log_path) {
        Ok(file) => arc_from_session_log(file, Some(branch)).unwrap_or_default(),
        Err(_) => SessionArc::default(),
    }
}

// trace:STORY-173 | ai:claude
/// Render a `GoalProposal` and offer it to the user with three choices:
/// **accept** (sets the goal exactly as `/goal <text>` would, logged with the
/// given `source`), **edit** (the user rephrases; the edited text is set), or
/// **decline** (nothing changes — agency: the default is to keep exploring). EOF
/// / a non-TTY prompt is treated as decline, so a piped or offline run never sets
/// a goal on the user's behalf. Returns `true` when a goal was set. Belief-neutral
/// throughout: the proposal (and any edit) is the QUESTION being resolved.
#[allow(clippy::too_many_arguments)]
fn offer_goal_proposal(
    goal: &mut Option<String>,
    proposal: &crate::observer::GoalProposal,
    source: &str,
    config: &CliConfig,
    logger: &mut SessionLogger,
    turn: u64,
    fe: &mut dyn FrontEnd,
) -> Result<bool> {
    writeln!(
        fe.out(),
        "\n{}",
        crate::style::paint(
            crate::style::meta(),
            &format!(
                "META (observer) — we seem to be exploring: {}",
                proposal.goal
            )
        )
    )?;
    if !proposal.rationale.trim().is_empty() {
        writeln!(
            fe.out(),
            "{}",
            crate::style::paint(
                crate::style::meta(),
                &format!("  Why: {}", proposal.rationale.trim())
            )
        )?;
    }
    let prompt =
        "Make resolving it the session goal? [a]ccept / [e]dit / [d]ecline (default decline): ";
    let choice = match fe.read_line(prompt)? {
        Some(line) => line.trim().to_ascii_lowercase(),
        // EOF / non-TTY: never set a goal on the user's behalf.
        None => return Ok(false),
    };
    let to_set = match choice.as_str() {
        "a" | "accept" | "y" | "yes" => proposal.goal.clone(),
        "e" | "edit" => {
            // The user rephrases the proposed QUESTION in their own words. An
            // empty edit (or EOF) declines rather than setting a blank goal.
            match fe.read_line("Edit the goal (the question to settle): ")? {
                Some(edited) if !edited.trim().is_empty() => edited.trim().to_string(),
                _ => return Ok(false),
            }
        }
        // Anything else — including `d`/`decline`/a blank line — keeps exploring.
        _ => return Ok(false),
    };
    set_goal_in_session(goal, &to_set, source, config, logger, turn, fe.out())?;
    Ok(true)
}

// trace:STORY-159 | ai:claude
/// When no goal is set yet, ask the Observer whether a thesis has crystallized
/// and, if so, OFFER it as the session goal (accept / edit / decline). The user
/// decides (agency: the default is to keep exploring free-flowing). Degrades
/// gracefully: offline / no crystallized thesis / EOF prompt → no goal is set,
/// the session stays free-flowing. Belief-neutral: the proposed goal is a QUESTION
/// to settle, never a belief to adopt. Used on the `/synopsis` way-3 path.
// trace:STORY-168 | ai:claude — front-end seam.
fn maybe_propose_goal(
    goal: &mut Option<String>,
    observer: &ObserverEngine,
    arc: &SessionArc,
    config: &CliConfig,
    logger: &mut SessionLogger,
    turn: u64,
    fe: &mut dyn FrontEnd,
) -> Result<()> {
    // Only propose when free-flowing — a session that already has a goal does not
    // get nagged with another.
    if goal.is_some() {
        return Ok(());
    }
    let positions = positions_from_arc(arc);
    let proposal = {
        let _spinner = crate::spinner::Spinner::start("reading for a thesis");
        observer.propose_goal(&positions)
    };
    let Some(proposal) = proposal else {
        return Ok(());
    };
    offer_goal_proposal(goal, &proposal, "observer", config, logger, turn, fe)?;
    Ok(())
}

// trace:STORY-173 | ai:claude
/// The ON-DEMAND goal request: the user typed bare `/goal` with no goal set (and
/// confirmed the `[y/N]` prompt) or `/request-goal` (which skips that confirm).
/// Re-reads the session arc, asks the Observer to propose a goal, and offers it
/// (accept / edit / decline). Degrades when no LLM backend is reachable: instead
/// of proposing, it reports "no goal" with a "needs an LLM backend" note, so the
/// user understands WHY no proposal came rather than seeing silence. Likewise a
/// thin / un-crystallized conversation reports that no thesis has emerged yet.
/// Belief-neutral: any goal set is the QUESTION being resolved, never a belief.
fn request_goal_on_demand(
    goal: &mut Option<String>,
    observer: &ObserverEngine,
    config: &CliConfig,
    logger: &mut SessionLogger,
    turn: u64,
    fe: &mut dyn FrontEnd,
) -> Result<()> {
    // A goal is already set — show it (the on-demand request never overrides a
    // live goal without the user re-stating one).
    if let Some(current) = goal.as_deref() {
        writeln!(fe.out(), "Current goal: {current}")?;
        return Ok(());
    }
    // Offline degrade: no LLM to read for a thesis. Report "no goal" with the
    // backend note rather than silently doing nothing.
    if observer.is_offline() {
        writeln!(
            fe.out(),
            "No goal set — proposing one needs an LLM backend (none reachable). State one directly with `/goal <the question you're resolving>`."
        )?;
        return Ok(());
    }
    let arc = arc_for_goal_request(&config.log_path, &config.branch_id);
    let positions = positions_from_arc(&arc);
    let proposal = {
        let _spinner = crate::spinner::Spinner::start("reading for a thesis");
        observer.propose_goal(&positions)
    };
    let Some(proposal) = proposal else {
        writeln!(
            fe.out(),
            "No goal set — no single thesis has crystallized yet. Keep exploring, or state one with `/goal <the question you're resolving>`."
        )?;
        return Ok(());
    };
    offer_goal_proposal(goal, &proposal, "user", config, logger, turn, fe)?;
    Ok(())
}

// trace:STORY-173 | ai:claude
/// The INTERROGATOR's bounded goal offer: when no goal is set and a thesis has
/// CRYSTALLIZED (the Observer's `propose_goal` returns one — the same signal the
/// synopsis path reads), the questioner offers EXACTLY ONCE to make resolving it
/// the goal. `offer_made` tracks the one-shot guard: it is set the first time an
/// offer is actually surfaced and the offer is NEVER repeated, so the user is
/// never nagged. No offer happens early (a thin conversation yields no crystallized
/// thesis), honoring free-flow. Belief-neutral: the offer names the QUESTION being
/// circled, never a belief to adopt.
#[allow(clippy::too_many_arguments)]
fn maybe_offer_goal_on_crystallize(
    goal: &mut Option<String>,
    offer_made: &mut bool,
    observer: &ObserverEngine,
    recent_path: &[AnsweredQuestion],
    config: &CliConfig,
    logger: &mut SessionLogger,
    turn: u64,
    fe: &mut dyn FrontEnd,
) -> Result<()> {
    // One-shot guard: never offer again once an offer has been surfaced, and never
    // offer over a goal that is already set.
    if *offer_made || goal.is_some() {
        return Ok(());
    }
    // Honor free-flow: only consider an offer once there is real substance to read
    // (a couple of recorded positions). An early, thin conversation is left alone.
    let positions: Vec<String> = recent_path
        .iter()
        .filter(|answered| !answered.normalized_answer.is_empty())
        .map(|answered| format!("On \"{}\": {}", answered.question_text, answered.raw_answer))
        .collect();
    if positions.len() < 2 {
        return Ok(());
    }
    let proposal = {
        let _spinner = crate::spinner::Spinner::start("reading for a thesis");
        observer.propose_goal(&positions)
    };
    let Some(proposal) = proposal else {
        // No thesis has crystallized yet — stay free-flowing and DO NOT burn the
        // one-shot. The offer can still surface on a later, more-formed turn.
        return Ok(());
    };
    // A thesis crystallized: this is the single offer. Mark it spent BEFORE
    // surfacing it so a declined (or accepted) offer is never repeated.
    *offer_made = true;
    offer_goal_proposal(goal, &proposal, "observer", config, logger, turn, fe)?;
    Ok(())
}

// trace:STORY-175 | ai:claude
/// The INTERROGATOR's BOUNDED self-objection: when no objection is open and a
/// genuine MATERIAL, still-unaddressed structural tension exists, the questioner
/// raises its own `/objection` AT MOST ONCE (the same one-shot-ish posture as the
/// goal-offer). `objection_made` is the one-shot guard — set the first time the
/// interrogator actually objects, and NEVER re-raised, so it never spams. No
/// objection happens on a thin conversation (fewer than two recorded positions) or
/// while one is already open. Offline it never objects. Belief-neutral: the
/// objection names a STRUCTURAL tension, never a belief.
#[allow(clippy::too_many_arguments)]
fn maybe_interrogator_objection(
    objection_state: &mut Option<ObjectionState>,
    objection_made: &mut bool,
    observer: &ObserverEngine,
    recent_path: &[AnsweredQuestion],
    config: &CliConfig,
    logger: &mut SessionLogger,
    turn: u64,
    output: &mut dyn Write,
) -> Result<()> {
    // One-at-a-time + one-shot guards: never raise over an open objection, and
    // never raise a second time (the interrogator objects rarely, never per-turn).
    if objection_state.is_some() || *objection_made {
        return Ok(());
    }
    // Honor free-flow: only consider objecting once there is real substance.
    let positions: Vec<String> = recent_path
        .iter()
        .filter(|answered| !answered.normalized_answer.is_empty())
        .map(|answered| format!("On \"{}\": {}", answered.question_text, answered.raw_answer))
        .collect();
    if positions.len() < 2 {
        return Ok(());
    }
    let Some(text) = observer.interrogator_objection(&positions) else {
        // No genuine material tension worth objecting over — stay quiet and DO NOT
        // spend the one-shot. A later, more-formed turn can still object.
        return Ok(());
    };
    // A material tension surfaced: this is the single interrogator objection. Mark
    // the one-shot spent BEFORE raising so it is never re-raised.
    *objection_made = true;
    writeln!(
        output,
        "The interrogator raises an objection on a material, unaddressed point:"
    )?;
    raise_objection(
        objection_state,
        &text,
        ObjectionParty::Interrogator,
        config,
        logger,
        turn,
        output,
    )?;
    Ok(())
}

// trace:STORY-175 | ai:claude
/// Emit the open-objection STATUS MOTIF: a machine-readable `[objection: …]` line
/// the TUI status bar mirrors into a GAVEL segment (the same out-of-band channel
/// the breadcrumb / score gauge use), plus a human-readable META line for the
/// HEADLESS path so a non-TTY / `--no-tui` run shows the pin in its footer. The
/// gavel glyph reads as "court is in session" on the contested point.
fn render_objection_motif(state: &ObjectionState, output: &mut dyn Write) -> Result<()> {
    // The bracketed line the TUI parses (mirrors the `[score: …]` channel).
    writeln!(
        output,
        "[objection: {} ({})]",
        state.text,
        state.objector.as_str()
    )?;
    // The headless-facing META footer with the gavel motif.
    writeln!(
        output,
        "{}",
        crate::style::paint(
            crate::style::meta(),
            &format!(
                "  {} OBJECTION (raised by {}): {} — pinned. /resolved (objector) or /judge (other party) to clear.",
                crate::style::OBJECTION_GAVEL,
                state.objector.as_str(),
                state.text
            )
        )
    )?;
    Ok(())
}

// trace:STORY-175 | ai:claude
/// Emit the objection-CLEAR motif so the TUI status bar drops its gavel segment
/// when the objection is `/resolved` or `/judge`-d, plus a headless confirmation.
fn render_objection_clear_motif(output: &mut dyn Write) -> Result<()> {
    writeln!(output, "[objection: clear]")?;
    Ok(())
}

// trace:STORY-175 | ai:claude
/// Handle `/objection <text>` from EITHER party: PIN the exchange on the contested
/// point. The ONE-AT-A-TIME guard refuses a second objection while one is open
/// ("resolve the open objection first"); a bare `/objection` (empty text) SHOWS the
/// current open objection (or notes none is open). On success the exchange enters
/// the OBJECTION state, the questioner narrows to the point, and the gavel motif is
/// shown. Belief-neutral: the objection names a STRUCTURAL tension, never a belief.
#[allow(clippy::too_many_arguments)]
fn raise_objection(
    objection_state: &mut Option<ObjectionState>,
    text: &str,
    objector: ObjectionParty,
    config: &CliConfig,
    logger: &mut SessionLogger,
    turn: u64,
    output: &mut dyn Write,
) -> Result<()> {
    let text = text.trim();
    // One-at-a-time guard / bare-`/objection` shows the open one.
    if let Some(open) = objection_state.as_ref() {
        if text.is_empty() {
            writeln!(
                output,
                "Open objection (raised by {}): {}",
                open.objector.as_str(),
                open.text
            )?;
        } else {
            writeln!(
                output,
                "An objection is already open — resolve the open objection first (/resolved by the objector, or /judge by the other party) before raising another."
            )?;
        }
        return Ok(());
    }
    // No open objection. A bare `/objection` has nothing to pin.
    if text.is_empty() {
        writeln!(
            output,
            "No objection open. Raise one with `/objection <the contested point>` to pin the exchange on it."
        )?;
        return Ok(());
    }
    let state = ObjectionState {
        text: text.to_string(),
        objector,
    };
    logger.objection_raised(
        &config.session_id,
        &config.user_id,
        &config.branch_id,
        turn,
        objector.as_str(),
        text,
    )?;
    render_objection_motif(&state, output)?;
    *objection_state = Some(state);
    Ok(())
}

// trace:STORY-175 | ai:claude
/// Handle `/resolved`: ONLY the OBJECTOR may call it (withdraw / accept the
/// resolution). A wrong-caller (the OTHER party) is rejected with a helpful note
/// pointing them at `/judge`. With no objection open it says so. On success the
/// objection clears, the gavel motif drops, and the resolution is logged. Returns
/// `true` when the objection was cleared (so the caller drops any tracked thread).
fn resolve_objection(
    objection_state: &mut Option<ObjectionState>,
    caller: ObjectionParty,
    config: &CliConfig,
    logger: &mut SessionLogger,
    turn: u64,
    output: &mut dyn Write,
) -> Result<bool> {
    let Some(open) = objection_state.as_ref() else {
        writeln!(
            output,
            "No objection is open to resolve. Raise one with `/objection <text>` first."
        )?;
        return Ok(false);
    };
    // ASYMMETRIC caller guard: only the objector may /resolved.
    if open.objector != caller {
        writeln!(
            output,
            "Only the party who RAISED the objection ({}) may call /resolved. As the other party, use /judge to have the Observer rule on it.",
            open.objector.as_str()
        )?;
        return Ok(false);
    }
    let text = open.text.clone();
    logger.objection_cleared(
        &config.session_id,
        &config.user_id,
        &config.branch_id,
        turn,
        "resolved",
        &text,
    )?;
    writeln!(
        output,
        "Objection resolved by the objector — \"{text}\" withdrawn/accepted. Returning to normal flow."
    )?;
    render_objection_clear_motif(output)?;
    *objection_state = None;
    Ok(true)
}

// trace:STORY-175 | ai:claude
/// Handle `/judge`: ONLY the OTHER (non-objecting) party may call it, escalating
/// the open objection to the OBSERVER for a belief-neutral SUSTAINED/OVERRULED
/// ruling + resolving condition. A wrong-caller (the objector) is rejected with a
/// helpful note pointing them at `/resolved`. OFFLINE degrades to a "needs an LLM
/// backend" note WITHOUT clearing the objection (the ruling needs the LLM). On a
/// successful ruling the objection CLEARS; a SUSTAINED ruling returns the tracked
/// OPEN THREAD (the resolving condition) so the caller folds it into the gauge.
/// Belief-neutral: the ruling judges STRUCTURE, never which belief is true.
#[allow(clippy::too_many_arguments)]
fn judge_objection(
    objection_state: &mut Option<ObjectionState>,
    caller: ObjectionParty,
    observer: &ObserverEngine,
    context: &str,
    goal: Option<&str>,
    config: &CliConfig,
    logger: &mut SessionLogger,
    turn: u64,
    output: &mut dyn Write,
) -> Result<JudgeOutcome> {
    let Some(open) = objection_state.as_ref() else {
        writeln!(
            output,
            "No objection is open to judge. Raise one with `/objection <text>` first."
        )?;
        return Ok(JudgeOutcome::default());
    };
    // ASYMMETRIC caller guard: only the NON-objecting party may /judge.
    if open.objector == caller {
        writeln!(
            output,
            "Only the OTHER party (not the objector) may call /judge. As the party who raised it, use /resolved to withdraw/accept it."
        )?;
        return Ok(JudgeOutcome::default());
    }
    // Offline degrade: the belief-neutral ruling needs the LLM. Report it and leave
    // the objection OPEN (it can still be /resolved by the objector). `/objection`
    // and `/resolved` are pure state transitions and keep working offline.
    if observer.is_offline() {
        writeln!(
            output,
            "Ruling on an objection needs an LLM backend (none reachable). The objection stays open — the objector can still /resolved it."
        )?;
        return Ok(JudgeOutcome::default());
    }
    let text = open.text.clone();
    let ruling = {
        let _spinner = crate::spinner::Spinner::start("ruling");
        observer.judge(&text, goal, context)
    };
    render_judge_ruling(&ruling, output)?;
    // trace:STORY-175 | ai:claude — DECIDED: a SUSTAINED objection becomes a TRACKED
    // OPEN THREAD (the resolving condition) that lowers roundedness / widens the
    // distance-to-goal gauge until addressed; the dialogue PROCEEDS. An OVERRULED
    // objection just clears (nothing tracked).
    let open_thread = match ruling.verdict {
        crate::observer::JudgeVerdict::Sustained => Some(ruling.resolving_condition.clone()),
        crate::observer::JudgeVerdict::Overruled => None,
    };
    logger.objection_cleared(
        &config.session_id,
        &config.user_id,
        &config.branch_id,
        turn,
        ruling.verdict.as_str(),
        &ruling.resolving_condition,
    )?;
    render_objection_clear_motif(output)?;
    *objection_state = None;
    Ok(JudgeOutcome { open_thread })
}

// trace:STORY-175 | ai:claude
/// Render the Observer's belief-neutral `/judge` ruling in the META voice:
/// SUSTAINED / OVERRULED, the structural rationale, and the resolving condition.
/// A sustained ruling notes it becomes a tracked open thread that widens the gauge.
fn render_judge_ruling(
    ruling: &crate::observer::JudgeRuling,
    output: &mut dyn Write,
) -> Result<()> {
    let verdict = match ruling.verdict {
        crate::observer::JudgeVerdict::Sustained => "SUSTAINED",
        crate::observer::JudgeVerdict::Overruled => "OVERRULED",
    };
    let mut body = format!(
        "{} RULING: {} — {}\n  Resolving condition: {}",
        crate::style::OBJECTION_GAVEL,
        verdict,
        ruling.rationale,
        ruling.resolving_condition
    );
    if matches!(ruling.verdict, crate::observer::JudgeVerdict::Sustained) {
        body.push_str(
            "\n  (Tracked as an open thread — it widens the distance-to-goal until addressed; the dialogue proceeds.)",
        );
    }
    if ruling.degraded {
        body.push_str("\n  (offline ruling — needs an LLM backend for a full ruling)");
    }
    writeln!(
        output,
        "{}",
        crate::style::paint(crate::style::meta(), &body)
    )?;
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

// trace:STORY-175 | ai:claude
/// Who raised the OPEN OBJECTION (the court-case `/objection`). Drives the
/// ASYMMETRIC exits: `/resolved` is the OBJECTOR's call (withdraw/accept), and
/// `/judge` is the OTHER party's call (escalate to the Observer). A wrong-caller
/// is rejected with a helpful note. Belief-neutral: the party is a procedural
/// role, never a belief side.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ObjectionParty {
    /// The human user raised the objection.
    User,
    /// The interrogator raised the objection (rarely — the bounded self-objection).
    Interrogator,
}

impl ObjectionParty {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Interrogator => "interrogator",
        }
    }
}

// trace:STORY-175 | ai:claude
/// The live OPEN OBJECTION the exchange is PINNED on. Only one is active at a time
/// (a second `/objection` is refused with "resolve the open objection first"). The
/// `objector` drives the asymmetric exits. While `Some`, the next-question prompt
/// NARROWS to `text` (via [`StrategyContext::objection`]) and normal advancement
/// pauses. Belief-neutral: `text` names a STRUCTURAL tension, never a belief.
#[derive(Debug, Clone)]
struct ObjectionState {
    /// The contested point the exchange is pinned on.
    text: String,
    /// Who raised it — the objector (`/resolved`) vs the other party (`/judge`).
    objector: ObjectionParty,
}

// trace:STORY-175 | ai:claude
/// The outcome of handling a `/judge`-ed objection: a SUSTAINED objection becomes a
/// tracked OPEN THREAD (the resolving condition) that widens the distance-to-goal
/// gauge until addressed; an OVERRULED objection just clears. `open_thread` is
/// `Some` only when sustained. Belief-neutral: the thread is a STRUCTURAL gap.
#[derive(Debug, Clone, Default)]
struct JudgeOutcome {
    /// The tracked open thread to fold into the gauge when the objection was
    /// SUSTAINED; `None` when overruled (nothing tracked).
    open_thread: Option<String>,
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
// trace:STORY-168 | ai:claude — front-end seam (the I/O triple collapsed into one
// `fe`, leaving 7 args, so the prior too-many-arguments allow is no longer needed).
fn run_closing_phase(
    config: &CliConfig,
    observer: &ObserverEngine,
    goal: Option<&str>,
    rester: ClosingParty,
    logger: &mut SessionLogger,
    turn: u64,
    fe: &mut dyn FrontEnd,
) -> Result<ClosingOutcome> {
    logger.phase_changed(
        &config.session_id,
        &config.user_id,
        &config.branch_id,
        turn,
        "closing",
        rester.as_str(),
    )?;
    render_closing_banner(fe.out())?;

    // The most recent settled position the user stated, fed to the challenger so
    // its objection presses on THIS case. Empty until the user makes a statement.
    let mut last_position = String::new();

    loop {
        // The user makes (the next) closing statement: their final / settled
        // position. They can instead request the verdict or call terminate.
        render_closing_user_prompt(fe.out())?;
        let line = fe.read_line("> ")?;
        let raw = match line {
            Some(raw) => raw,
            // EOF / non-TTY: do not hang. Render the verdict on what we have.
            None => {
                return finish_with_verdict(config, observer, goal, fe.out());
            }
        };
        if crate::input::is_verdict_command(&raw) {
            // A direct request for the FINAL VERDICT — no terminator, no forfeited
            // last word; render the belief-neutral assessment and end.
            return finish_with_verdict(config, observer, goal, fe.out());
        }
        if crate::input::is_terminate_command(&raw) {
            // The USER terminates: the fairness rule gives the CHALLENGER the final
            // word (its strongest remaining objection) before the verdict. The user
            // does NOT get to add another statement.
            let final_speaker = final_word_speaker(ClosingParty::User);
            debug_assert_eq!(final_speaker, ClosingParty::Challenger);
            render_terminate_note(ClosingParty::User, fe.out())?;
            let objection = {
                let _spinner = crate::spinner::Spinner::start("closing objection");
                observer.closing_objection(&last_position, goal)
            };
            render_closing_objection(&objection, true, fe.out())?;
            logger.closing_statement(
                &config.session_id,
                &config.user_id,
                &config.branch_id,
                turn,
                ClosingParty::Challenger.as_str(),
                &objection.objection,
                true,
            )?;
            return finish_with_verdict(config, observer, goal, fe.out());
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
        render_recorded_user_statement(statement, fe.out())?;
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
        render_closing_objection(&objection, false, fe.out())?;
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
    output: &mut dyn Write,
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
    output: &mut dyn Write,
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
fn render_closing_banner(output: &mut dyn Write) -> Result<()> {
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
fn render_closing_user_prompt(output: &mut dyn Write) -> Result<()> {
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
fn render_recorded_user_statement(statement: &str, output: &mut dyn Write) -> Result<()> {
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
    output: &mut dyn Write,
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
fn render_terminate_note(terminator: ClosingParty, output: &mut dyn Write) -> Result<()> {
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

// trace:STORY-175 | ai:claude
/// Build the recent-exchange CONTEXT string the `/judge` ruling reads, so the
/// Observer can weigh whether the objection's contested point is MATERIAL and
/// already ADDRESSED. Purely structural — it echoes the user's own recorded
/// question/answer pairs, inventing nothing. Belief-neutral: it carries positions
/// taken, never beliefs graded.
fn judge_context_for_frontier(current: &Question, recent_path: &[AnsweredQuestion]) -> String {
    let mut context = String::new();
    for answered in recent_path.iter().rev().take(5).rev() {
        context.push_str(&format!(
            "Q: {} A: {}\n",
            answered.question_text, answered.raw_answer
        ));
    }
    context.push_str(&format!("Current question: {}", current.title));
    context
}

// trace:STORY-127 | ai:claude
/// Render an [`ExchangeReading`] as a clearly-labeled META voice, visually
/// distinct from the question (style::meta). Belief-neutral and clarify-only:
/// it restates the rebuttal, names the tension, diagnoses the mismatch, and
/// lists the dimensions a precise answer must address — it never supplies an
/// answer. Pure over the buffer + reading, so it is unit-testable without a
/// live LLM. The caller re-presents the SAME question afterwards (non-
/// destructive), so this only writes; it never consumes input or mutates state.
fn render_exchange_reading(reading: &ExchangeReading, output: &mut dyn Write) -> Result<()> {
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

// trace:STORY-164 | ai:claude
/// Render a [`HelpAnswer`] from the `/help` channel as a clearly-labeled META
/// voice, visually distinct from the question (style::meta). Belief-neutral and
/// process-focused: the answer talks about HOW THE TOOL WORKS (controls, flow),
/// sourced from TOOL-CONTEXT — it never supplies a belief or takes a side. Pure
/// over the buffer + answer, so it is unit-testable without a live LLM. The caller
/// re-presents the SAME question afterwards (non-destructive), so this only
/// writes; it never consumes input or mutates state. The header flags the offline
/// degraded mode (the static help index) so the user knows when no model answered.
fn render_help_answer(answer: &HelpAnswer, output: &mut dyn Write) -> Result<()> {
    let header = if answer.degraded {
        "META (/help, offline) — process help (belief-neutral; about the tool, not your belief):"
    } else {
        "META (/help) — process help (belief-neutral; about the tool, not your belief):"
    };
    writeln!(
        output,
        "\n{}",
        crate::style::paint(crate::style::meta(), header)
    )?;
    for line in answer.answer.trim().lines() {
        writeln!(
            output,
            "{}",
            crate::style::paint(crate::style::meta(), &format!("  {line}"))
        )?;
    }
    Ok(())
}

// trace:STORY-165 | ai:claude
/// Assemble the [`TutorContext`] for the `/tutor` coach at the frontier: the
/// question on screen plus the user's OWN point. The point is the text typed after
/// `/tutor` when given; on a bare `/tutor` it falls back to the user's most recent
/// answer on the path (the half-formed view they are reaching to sharpen). It reads
/// only the user's OWN words — it never seeds a belief.
fn tutor_context_for_frontier(
    text: &str,
    current: &Question,
    recent_path: &[AnsweredQuestion],
) -> TutorContext {
    let typed = text.trim();
    let point = if !typed.is_empty() {
        typed.to_string()
    } else {
        recent_path
            .last()
            .map(|prior| prior.raw_answer.clone())
            .unwrap_or_default()
    };
    TutorContext {
        question: current.title.clone(),
        point,
    }
}

// trace:STORY-165 | ai:claude
/// Render a [`TutorReading`] from the `/tutor` channel as a clearly-labeled META
/// voice, visually distinct from the question (style::meta). Content-aware but
/// STILL belief-neutral: it reflects the user's OWN point back more precisely
/// (framed as a check), teaches the relevant distinction, and names the nuance they
/// have not yet addressed — it never supplies a belief or takes a side. Pure over
/// the buffer + reading, so it is unit-testable without a live LLM. The caller
/// re-presents the SAME question afterwards (non-destructive). The header flags the
/// offline degraded mode (the structural note) so the user knows when no model
/// coached.
fn render_tutor_reading(reading: &TutorReading, output: &mut dyn Write) -> Result<()> {
    let header = if reading.degraded {
        "META (/tutor, offline) — articulation & nuance coach (sharpens YOUR point; never supplies it):"
    } else {
        "META (/tutor) — articulation & nuance coach (sharpens YOUR point; never supplies it):"
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
    line("Your point, sharper", &reading.reflection, output)?;
    line("The distinction", &reading.distinction, output)?;
    if !reading.missing_nuance.is_empty() {
        writeln!(
            output,
            "{}",
            crate::style::paint(crate::style::meta(), "  Nuance you have not yet addressed:")
        )?;
        for nuance in &reading.missing_nuance {
            writeln!(
                output,
                "{}",
                crate::style::paint(crate::style::meta(), &format!("    - {nuance}"))
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn run_session(
    config: &CliConfig,
    bank: &dyn QuestionBank,
    strategy: &dyn NextQuestionStrategy,
    input: impl Read,
    output: &mut dyn Write,
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
    output: &mut dyn Write,
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
    output: &mut dyn Write,
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
    output: &mut dyn Write,
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
    output: &mut dyn Write,
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
    output: &mut dyn Write,
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

// trace:STORY-169 | ai:claude
/// Select + build the session front-end (ADR-166 / EPIC-167).
///
/// Returns a boxed [`FrontEnd`] over the supplied `input`/`output` so the engine
/// loop talks to ONE binding regardless of which impl backs it. The choice is
/// [`crate::tui::select_front_end`]'s policy: an interactive command on a real
/// TTY (and not `--no-tui`) gets the ratatui [`crate::tui::TuiFrontEnd`];
/// otherwise the [`crate::frontend::LineFrontEnd`] reproduces today's line
/// behavior over `input`/`output` (the path every test, pipe, and `--no-tui` run
/// takes). The TUI ignores `input`/`output` (it drives the real terminal through
/// crossterm) and reads its nested quick-add from an empty line source.
fn build_session_front_end<'a, R: Read + 'a>(
    config: &CliConfig,
    input: R,
    output: &'a mut dyn Write,
) -> Result<Box<dyn crate::frontend::FrontEnd + 'a>> {
    let choice = crate::tui::select_front_end(
        config.is_interactive(),
        config.no_tui,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    );
    match choice {
        crate::tui::FrontEndChoice::Tui => {
            // The TUI owns the real terminal; the nested quick-add core reads from
            // an empty line source (its in-TUI authoring UI is a STORY-170 concern).
            let empty = std::io::BufReader::new(std::io::empty());
            Ok(Box::new(crate::tui::TuiFrontEnd::new(empty)?))
        }
        crate::tui::FrontEndChoice::Headless => {
            Ok(Box::new(crate::frontend::LineFrontEnd::new(input, output)?))
        }
    }
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
    output: &mut dyn Write,
    mut turn: u64,
    write_start_event: bool,
    mut recent_path: Vec<AnsweredQuestion>,
) -> Result<()> {
    // trace:STORY-168 | ai:claude
    // Build the front-end at the engine boundary and route ALL session I/O through
    // it: the engine below renders via `fe.out()` and requests input/control via
    // `fe.read_answer` / `fe.read_line`. The engine is front-end-agnostic.
    //
    // trace:STORY-169 | ai:claude — SELECT the front-end here (ADR-166 / EPIC-167):
    // an interactive TTY (and not `--no-tui`) gets the ratatui TUI; everything else
    // — a non-TTY stream, `--no-tui`, the ~336 piped/byte tests, scripted runs —
    // gets the HEADLESS LINE front-end, which reproduces today's byte-for-byte
    // behavior over `input`/`output`. The box lets one binding hold either impl
    // without duplicating the loop below.
    let mut fe_box = build_session_front_end(config, input, output)?;
    let fe: &mut dyn crate::frontend::FrontEnd = fe_box.as_mut();
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
    // trace:STORY-173 | ai:claude
    // The interrogator's bounded goal-offer guard: the questioner offers to make a
    // crystallized thesis the goal AT MOST ONCE per session (never re-offered).
    // Set the first time an offer is actually surfaced. Seeded `true` when the
    // session already starts with a goal — there is nothing to offer.
    let mut goal_offer_made = goal.is_some();
    // trace:STORY-161 | ai:claude
    // The live session MODE. Seeded from `--mode` / the resumed start
    // (config.mode) and updated in-session by the `/mode` toggle. It drives the
    // next-question prompt (via StrategyContext) and is logged so the verdict path
    // and resume read the same mode. Belief-neutral: debate argues craft, never
    // which belief is true.
    let mut mode: SessionMode = config.mode;
    // trace:STORY-174 | ai:claude
    // The persistent SCORE GAUGE state. `score_gauge_on` is the `/score` toggle —
    // DEFAULT OFF (even with a goal set), flipped only by `/score`. `last_gauge`
    // caches the most recent computed reading so it can be shown with a "cached"
    // freshness marker BETWEEN gates (the EPIC-154 cost guard — scoring needs an
    // LLM pass, so it recomputes only at gates, never every turn).
    // `turns_since_score` counts answered turns since the last recompute; at
    // `SCORE_GATE_TURNS` it triggers a fresh recompute at the next frontier.
    let mut score_gauge_on = false;
    let mut last_gauge: Option<ScoreGauge> = None;
    let mut turns_since_score: u64 = 0;
    // trace:STORY-175 | ai:claude
    // The OPEN OBJECTION the exchange is PINNED on (the court-case `/objection`).
    // `None` = normal flow; `Some` narrows the questioner to the contested point and
    // pauses normal advancement until `/resolved` (objector) or `/judge` (other
    // party). One at a time. `objection_open_threads` collects the resolving
    // condition of each SUSTAINED `/judge` ruling: a tracked open thread that widens
    // the distance-to-goal gauge until addressed (DECIDED — STORY-175). The
    // interrogator's bounded self-objection is one-shot, guarded by
    // `interrogator_objection_made` (same posture as the goal-offer): it objects
    // RARELY, never per-turn.
    let mut objection_state: Option<ObjectionState> = None;
    let mut objection_open_threads: Vec<String> = Vec::new();
    let mut interrogator_objection_made = false;
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
                fe.out(),
            )?;
            // trace:STORY-174 | ai:claude — when the persistent gauge is ON, show
            // it under the breadcrumb each frontier turn. COST GUARD: recompute
            // only at GATES (every `SCORE_GATE_TURNS` answered turns); between
            // gates show the LAST computed value with a "cached" marker. The
            // toggle itself (and the start of the loop after a recompute) resets
            // `turns_since_score` to 0, so the first turn after toggling shows the
            // fresh value.
            if score_gauge_on {
                let fresh = if turns_since_score >= SCORE_GATE_TURNS || last_gauge.is_none() {
                    last_gauge = Some(compute_score_gauge(
                        &observer,
                        &config.log_path,
                        Some(&config.branch_id),
                        goal.as_deref(),
                        objection_open_threads.last().map(String::as_str),
                    ));
                    turns_since_score = 0;
                    true
                } else {
                    false
                };
                if let Some(gauge) = &last_gauge {
                    render_score_gauge(gauge, fresh, fe.out())?;
                }
            }
            let probed_terms = load_probed_terms(bank, &current);
            if let Some(settled) = settled_definition_for(&probed_terms, &settled_terms) {
                render_settled_term_definition(settled, fe.out())?;
            } else {
                render_term_definitions(&probed_terms, fe.out())?;
            }
            render_question_for(&current, InputContext::Frontier, fe.out())?;
            let answer = match fe.read_answer(&current.answer_kind, InputContext::Frontier)? {
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
                        fe,
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
                    quick_add_from_current(bank, strategy, user_authored_persister, &current, fe)?;
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
                    render_exchange_reading(&reading, fe.out())?;
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
                        fe.out(),
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
                                fe,
                            )?;
                        }
                        if synopsis.offers_conclude() && prompt_to_conclude(fe)? {
                            crate::synopsis::render_conclusion(&synopsis, &arc, fe.out())?;
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
                AnswerInput::Score => {
                    // trace:STORY-174 | ai:claude
                    // Toggle the persistent score gauge. `/score` is the SOLE
                    // toggle and the gauge defaults OFF. Turning it ON computes the
                    // score IMMEDIATELY (a gate) and shows it; turning it OFF emits
                    // the gauge-off marker (the TUI clears its segment) and stops
                    // showing it. Non-destructive: the SAME question is then
                    // re-presented (the loop redraws the breadcrumb + gauge).
                    // Belief-neutral: the gauge reads structure / distance-to-goal.
                    score_gauge_on = !score_gauge_on;
                    if score_gauge_on {
                        let gauge = compute_score_gauge(
                            &observer,
                            &config.log_path,
                            Some(&config.branch_id),
                            goal.as_deref(),
                            objection_open_threads.last().map(String::as_str),
                        );
                        render_score_gauge(&gauge, true, fe.out())?;
                        last_gauge = Some(gauge);
                        // The toggle is itself a gate, so the next frontier turn
                        // shows this fresh value without recomputing.
                        turns_since_score = 0;
                    } else {
                        last_gauge = None;
                        render_score_gauge_off(fe.out())?;
                    }
                    continue;
                }
                AnswerInput::Goal(text) => {
                    // trace:STORY-159 | ai:claude
                    // In-session goal (way 2 of 3): the user states the
                    // thesis. A non-empty text SETS the goal — logged as a
                    // `goal_set` event (so resume restores it and the arc /
                    // synopsis orient to it) — then the SAME question is
                    // re-presented, now oriented toward the goal. Non-destructive:
                    // nothing else changes. Belief-neutral: the goal is the
                    // question being settled, never a belief.
                    // trace:STORY-173 | ai:claude
                    // A bare `/goal` (empty text) now branches on whether a goal
                    // is set: WITH a goal it still just SHOWS the current one
                    // (unchanged), but with NO goal it first PROMPTS "No goal set —
                    // request one? [y/N]" and, on yes, proposes one on demand
                    // (accept / edit / decline). It never clears a goal.
                    if text.trim().is_empty() && goal.is_none() {
                        let prompt = "No goal set — request one? [y/N]: ";
                        let wants = match fe.read_line(prompt)? {
                            Some(line) => {
                                matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
                            }
                            // EOF / non-TTY: do not propose on the user's behalf.
                            None => false,
                        };
                        if wants {
                            request_goal_on_demand(
                                &mut goal,
                                &observer,
                                config,
                                &mut logger,
                                answered_turn,
                                fe,
                            )?;
                        }
                        continue;
                    }
                    set_goal_in_session(
                        &mut goal,
                        &text,
                        "user",
                        config,
                        &mut logger,
                        answered_turn,
                        fe.out(),
                    )?;
                    continue;
                }
                AnswerInput::RequestGoal => {
                    // trace:STORY-173 | ai:claude
                    // The on-demand `/request-goal` alias: propose a goal DIRECTLY
                    // (skipping the bare-`/goal` `[y/N]` confirm), then offer it
                    // (accept / edit / decline). With a goal already set it just
                    // shows the current one. Offline degrades to a "needs an LLM
                    // backend" note. Non-destructive: nothing else changes.
                    request_goal_on_demand(
                        &mut goal,
                        &observer,
                        config,
                        &mut logger,
                        answered_turn,
                        fe,
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
                        fe.out(),
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
                        fe,
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
                    let outcome =
                        finish_with_verdict(config, &observer, goal.as_deref(), fe.out())?;
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
                    render_closing_banner(fe.out())?;
                    render_terminate_note(ClosingParty::User, fe.out())?;
                    let objection = {
                        let _spinner = crate::spinner::Spinner::start("closing objection");
                        observer.closing_objection("", goal.as_deref())
                    };
                    render_closing_objection(&objection, true, fe.out())?;
                    logger.closing_statement(
                        &config.session_id,
                        &config.user_id,
                        &config.branch_id,
                        answered_turn,
                        ClosingParty::Challenger.as_str(),
                        &objection.objection,
                        true,
                    )?;
                    let outcome =
                        finish_with_verdict(config, &observer, goal.as_deref(), fe.out())?;
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
                    // trace:STORY-164 | ai:claude — non-destructive process-help
                    // channel: answer the free-form question from TOOL-CONTEXT (the
                    // design), belief-neutral, then re-present the SAME question
                    // (like Observe). Offline degrades to a static help index.
                    let answer = {
                        let _spinner = crate::spinner::Spinner::start("helping");
                        observer.help(&question)
                    };
                    render_help_answer(&answer, fe.out())?;
                    continue;
                }
                AnswerInput::Tutor(text) => {
                    // trace:STORY-165 | ai:claude — non-destructive articulation &
                    // nuance coach: reflect the user's OWN point back more precisely,
                    // teach the distinction, and name the missing nuance — never
                    // supplying a belief or taking a side. Re-present the SAME
                    // question (like Observe). Offline degrades to a structural note.
                    let context = tutor_context_for_frontier(&text, &current, &recent_path);
                    let reading = {
                        let _spinner = crate::spinner::Spinner::start("tutoring");
                        observer.tutor(&context)
                    };
                    render_tutor_reading(&reading, fe.out())?;
                    continue;
                }
                AnswerInput::Objection(text) => {
                    // trace:STORY-175 | ai:claude
                    // EITHER party raised a court-style `/objection`: PIN the
                    // exchange on the contested point (the user is the objector
                    // here). One-at-a-time guard refuses a second; a bare
                    // `/objection` shows the open one. On success the questioner
                    // narrows to the point (via StrategyContext) and the gavel motif
                    // shows. Non-destructive otherwise: the SAME question is
                    // re-presented. Belief-neutral: the objection is a structural
                    // tension, never a belief.
                    raise_objection(
                        &mut objection_state,
                        &text,
                        ObjectionParty::User,
                        config,
                        &mut logger,
                        answered_turn,
                        fe.out(),
                    )?;
                    continue;
                }
                AnswerInput::Resolved => {
                    // trace:STORY-175 | ai:claude
                    // `/resolved`: ONLY the OBJECTOR may call it. A wrong-caller is
                    // rejected with a helpful note. On success the objection clears
                    // and normal flow resumes. Pure state transition — works offline.
                    resolve_objection(
                        &mut objection_state,
                        ObjectionParty::User,
                        config,
                        &mut logger,
                        answered_turn,
                        fe.out(),
                    )?;
                    continue;
                }
                AnswerInput::Judge => {
                    // trace:STORY-175 | ai:claude
                    // `/judge`: ONLY the OTHER (non-objecting) party may call it ->
                    // the Observer renders a belief-neutral SUSTAINED/OVERRULED
                    // ruling + resolving condition, then clears the objection. A
                    // wrong-caller (the objector) is rejected. Offline degrades to a
                    // "needs an LLM backend" note (the objection stays open). A
                    // SUSTAINED ruling tracks the resolving condition as an open
                    // thread that widens the gauge until addressed; the dialogue
                    // proceeds.
                    let context = judge_context_for_frontier(&current, &recent_path);
                    let outcome = judge_objection(
                        &mut objection_state,
                        ObjectionParty::User,
                        &observer,
                        &context,
                        goal.as_deref(),
                        config,
                        &mut logger,
                        answered_turn,
                        fe.out(),
                    )?;
                    if let Some(thread) = outcome.open_thread {
                        objection_open_threads.push(thread);
                    }
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
                render_settled_term_definition(settled, fe.out())?;
            } else if let Some(settled) =
                prompt_for_term_meaning(&probed_terms, strategy, term_persister, fe)?
            {
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
        // trace:STORY-174 | ai:claude — count this answered turn toward the score
        // gauge's recompute GATE (the cost guard): only at `SCORE_GATE_TURNS` does
        // the next frontier turn re-run the LLM-backed score. Counted once per
        // answered turn regardless of whether the gauge is currently on, so
        // turning the gauge on mid-session re-gates from a clean start (the toggle
        // resets the counter anyway).
        turns_since_score = turns_since_score.saturating_add(1);
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
                        // trace:STORY-175 | ai:claude — narrow to an open objection.
                        objection: objection_state.as_ref().map(|o| o.text.clone()),
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
                        fe,
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
                contradiction_resolution_persister,
                fe,
            )? {
                break;
            }
        }
        if matches!(current.answer_kind, AnswerKind::FreeText) {
            let flagged_terms = strategy.loaded_terms(&current, &answer).unwrap_or_default();
            let definitions = definitions_for_loaded_terms(&probed_terms, &flagged_terms);
            if let Some(settled) = settled_definition_for(&definitions, &settled_terms) {
                render_settled_term_definition(settled, fe.out())?;
            } else {
                render_term_definitions(&definitions, fe.out())?;
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
            // trace:STORY-175 | ai:claude — while an objection is pinned the next
            // question NARROWS to the contested point (priority over the goal).
            objection: objection_state.as_ref().map(|o| o.text.clone()),
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
                    fe,
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

        // trace:STORY-173 | ai:claude
        // INTERROGATOR-offered goal (offer once, on crystallize): with a fresh
        // answer recorded and the conversation grown, let the questioner offer a
        // goal AT MOST ONCE — when a thesis has crystallized and none is set. The
        // one-shot guard (`goal_offer_made`) means the offer never repeats, so the
        // user is never nagged; an early, thin conversation yields no crystallized
        // thesis, honoring free-flow. Declining keeps exploring. Belief-neutral:
        // the offer names the QUESTION being circled, never a belief.
        maybe_offer_goal_on_crystallize(
            &mut goal,
            &mut goal_offer_made,
            &observer,
            &recent_path,
            config,
            &mut logger,
            turn,
            fe,
        )?;

        // trace:STORY-175 | ai:claude
        // INTERROGATOR self-objection (raise rarely, on a material tension): with a
        // fresh answer recorded and the conversation grown, let the questioner raise
        // its OWN `/objection` AT MOST ONCE — when a genuine material, unaddressed
        // structural tension exists and none is already open. The one-shot guard
        // (`interrogator_objection_made`) means it never spams (same bounded posture
        // as the goal-offer). Offline it never objects. Belief-neutral: it names a
        // structural tension, never a belief.
        maybe_interrogator_objection(
            &mut objection_state,
            &mut interrogator_objection_made,
            &observer,
            &recent_path,
            config,
            &mut logger,
            turn,
            fe.out(),
        )?;
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
            writeln!(fe.out(), "Session ended.")?;
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
            render_session_end(preface, &config.session_id, fe.out())?;
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

// trace:STORY-168 | ai:claude — front-end seam.
fn ask_contradiction_follow_up(
    config: &CliConfig,
    logger: &mut SessionLogger,
    turn: u64,
    contradiction: &Contradiction,
    resolution_persister: &dyn ContradictionResolutionPersister,
    fe: &mut dyn FrontEnd,
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
    render_question(&question, fe.out())?;
    match fe.read_answer(&question.answer_kind, InputContext::Frontier)? {
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
            render_session_end(None, &config.session_id, fe.out())?;
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
        // trace:STORY-174 | ai:claude — a stray `/score` on a transient
        // contradiction follow-up is a no-op (no gauge state in scope here).
        | AnswerInput::Score
        | AnswerInput::Goal(_)
        // trace:STORY-173 | ai:claude — a stray `/request-goal` on a transient
        // contradiction follow-up is a no-op here (no loop goal state in scope).
        | AnswerInput::RequestGoal
        // trace:STORY-161 | ai:claude — a stray `/mode` toggle on a transient
        // contradiction follow-up is a no-op here (no loop mode state in scope).
        | AnswerInput::Mode(_)
        | AnswerInput::Rest
        | AnswerInput::Verdict
        | AnswerInput::Terminate
        // trace:STORY-163 | ai:claude — a stray `/help` / `/tutor` on a transient
        // contradiction follow-up is a no-op here (no LLM channel state in scope).
        | AnswerInput::Help(_)
        | AnswerInput::Tutor(_)
        // trace:STORY-175 | ai:claude — a stray `/objection` / `/resolved` / `/judge`
        // on a transient contradiction follow-up is a no-op here (no objection state
        // in scope; the pin lives on the main frontier loop).
        | AnswerInput::Objection(_)
        | AnswerInput::Resolved
        | AnswerInput::Judge => Ok(false),
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
// trace:STORY-168 | ai:claude — front-end seam: the quick-add banner renders via
// `fe.out()`, then the STORY-87 authoring core runs against the front-end's raw
// line channels (`fe.author_io`) so that shared, standalone-also core is unchanged.
fn quick_add_from_current(
    bank: &dyn QuestionBank,
    strategy: &dyn NextQuestionStrategy,
    user_authored_persister: &dyn UserAuthoredQuestionPersister,
    current: &Question,
    fe: &mut dyn FrontEnd,
) -> Result<()> {
    writeln!(
        fe.out(),
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
    let (input, output) = fe.author_io();
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
fn render_dead_end_menu(output: &mut dyn Write) -> Result<()> {
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
// trace:STORY-168 | ai:claude — front-end seam.
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
    fe: &mut dyn FrontEnd,
) -> Result<DeadEndOutcome> {
    loop {
        render_dead_end_menu(fe.out())?;
        let choice = match fe.read_line("> ")? {
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
                        fe.out(),
                        "Couldn't generate a new question here (this strategy is exhausted — try `--strategy llm`)."
                    )?,
                }
            }
            Some('p') => match different_topic_punt_question(current, recent_path, bank)? {
                Some(next) => return Ok(DeadEndOutcome::Continue(next)),
                None => writeln!(fe.out(), "No different-topic question to punt to.")?,
            },
            Some('a') => {
                // Author + link a begets follow-on from the current node; it
                // becomes a successor in later sessions. Stay in the menu so the
                // user can [G]enerate into it (or pick another exit).
                quick_add_from_current(bank, strategy, user_authored_persister, current, fe)?;
            }
            Some('s') => {
                // trace:STORY-156 | ai:claude — the dead-end menu surfaces the
                // synopsis (with its conclude OFFER line when well-rounded) but
                // stays in the menu; the graceful conclude path lives at the
                // frontier handler, so the return is intentionally ignored here.
                render_session_synopsis(observer, log_path, Some(branch), fe.out())?;
            }
            Some('q') => return Ok(DeadEndOutcome::Quit),
            _ => writeln!(fe.out(), "Pick one of G, P, A, S, or Q.")?,
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
/// Observer engine (for the per-exchange `o` reading — STORY-176 moved observe
/// off `?`, which is now the cheat-sheet) plus where to find the live session log
/// (for the whole-session `S` synopsis). Bundled so the review helper keeps a
/// tidy argument list.
struct ReviewContext<'a> {
    observer: &'a ObserverEngine,
    log_path: &'a Path,
    branch: &'a str,
}

// trace:STORY-168 | ai:claude — front-end seam.
fn browse_answered_path(
    bank: &dyn QuestionBank,
    recent_path: &[AnsweredQuestion],
    // trace:STORY-127 | ai:claude — the `?` observer and (STORY-128) the `S`
    // synopsis both live in this session-level context.
    review: &ReviewContext<'_>,
    fe: &mut dyn FrontEnd,
) -> Result<ReviewOutcome> {
    if recent_path.is_empty() {
        writeln!(fe.out(), "No previous answers to review.")?;
        return Ok(ReviewOutcome::Frontier);
    }
    let mut cursor = recent_path.len() - 1;
    loop {
        let reviewed = &recent_path[cursor];
        let question = bank.load_question(&reviewed.question_ref)?;
        render_reviewed_answer(cursor, recent_path.len(), reviewed, fe.out())?;
        render_question_for(&question, InputContext::Review, fe.out())?;
        match fe.read_answer(&question.answer_kind, InputContext::Review)? {
            AnswerInput::Back => {
                if cursor == 0 {
                    writeln!(fe.out(), "Already at the first answered question.")?;
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
                    writeln!(
                        fe.out(),
                        "Answer unchanged; still reviewing the saved path."
                    )?;
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
                render_exchange_reading(&reading, fe.out())?;
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
                    fe.out(),
                )?;
                continue;
            }
            // trace:STORY-174 | ai:claude — the `/score` gauge toggle takes effect
            // at the FRONTIER (where the live gauge state + goal are in scope), so
            // a stray `/score` in the review pane is a no-op; the user returns to
            // the frontier to toggle the gauge.
            AnswerInput::Score => continue,
            // trace:STORY-159 | ai:claude — the goal command is frontier-only in
            // effect: the review pane re-walks the saved path, so a stray `/goal`
            // here is a no-op rather than re-orienting from inside review. (The
            // goal is set at the frontier, where it can orient the next question.)
            AnswerInput::Goal(_) => continue,
            // trace:STORY-173 | ai:claude — `/request-goal`, like `/goal`, takes
            // effect at the FRONTIER (where the live goal state is in scope), so a
            // stray one in the review pane is a no-op.
            AnswerInput::RequestGoal => continue,
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
            // trace:STORY-164 | ai:claude — `/help` (process) is a non-destructive
            // out-of-band channel that applies anywhere, including the review pane:
            // answer the free-form question from TOOL-CONTEXT (belief-neutral; the
            // static index offline) and stay on the same reviewed answer. `/tutor`
            // (STORY-165) likewise coaches the reviewed answer's articulation here.
            AnswerInput::Help(question) => {
                let answer = {
                    let _spinner = crate::spinner::Spinner::start("helping");
                    review.observer.help(&question)
                };
                render_help_answer(&answer, fe.out())?;
                continue;
            }
            // trace:STORY-165 | ai:claude — `/tutor` in review coaches the user's
            // OWN point: the text typed after /tutor, or (bare) the SAVED answer on
            // this reviewed step. Belief-neutral (structural note offline); stay on
            // the same reviewed answer.
            AnswerInput::Tutor(text) => {
                let typed = text.trim();
                let point = if typed.is_empty() {
                    reviewed.raw_answer.clone()
                } else {
                    typed.to_string()
                };
                let context = TutorContext {
                    question: question.title.clone(),
                    point,
                };
                let reading = {
                    let _spinner = crate::spinner::Spinner::start("tutoring");
                    review.observer.tutor(&context)
                };
                render_tutor_reading(&reading, fe.out())?;
                continue;
            }
            // trace:STORY-175 | ai:claude — the court-case `/objection` controls
            // take effect at the FRONTIER (where the live objection state pins the
            // exchange), not from inside the review pane re-walking the saved path.
            // A stray `/objection` / `/resolved` / `/judge` here is a no-op; the user
            // returns to the frontier to raise / clear an objection.
            AnswerInput::Objection(_) | AnswerInput::Resolved | AnswerInput::Judge => continue,
            AnswerInput::End => return Ok(ReviewOutcome::End),
        }
    }
}

fn render_reviewed_answer(
    cursor: usize,
    total: usize,
    answer: &AnsweredQuestion,
    output: &mut dyn Write,
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
    output: &mut dyn Write,
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
    output: &mut dyn Write,
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
        // trace:STORY-175 | ai:claude — a resumed auto-continue has no live pin.
        objection: None,
    };

    let auto = {
        let _spinner = crate::spinner::Spinner::start("thinking");
        strategy.next_question(&last_question, &context, bank)?
    };

    let mut input = BufReader::new(input);
    let next = match auto {
        Some(next) => next,
        None => {
            let observer = ObserverEngine::for_config(config);
            // trace:STORY-168 | ai:claude — the resume dead-end menu runs against a
            // front-end built over the SAME input stream; it is dropped before
            // `run_session_from_current` builds its own over the remaining bytes
            // (reproducing the prior split: the menu and the resumed loop shared
            // one reader).
            // trace:STORY-169 | ai:claude — the menu is interactive, so it goes
            // through the SAME front-end selection as the loop: a TTY gets the TUI,
            // a piped/`--no-tui` run gets the headless line front-end over `input`.
            let outcome = {
                let mut fe = build_session_front_end(config, &mut input, &mut *output)?;
                dead_end_menu(
                    bank,
                    strategy,
                    &user_authored_persister,
                    &observer,
                    &config.log_path,
                    &config.branch_id,
                    &last_question,
                    &context,
                    &prior_path,
                    fe.as_mut(),
                )?
            };
            match outcome {
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

pub(crate) fn fork_session(config: &CliConfig, output: &mut dyn Write) -> Result<()> {
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

    pub(crate) fn render(&self, output: &mut dyn Write) -> Result<()> {
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

    pub(crate) fn render_recap(&self, output: &mut dyn Write) -> Result<()> {
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

    // trace:STORY-175 | ai:claude
    /// Record an OBJECTION raised (the court-case `/objection`): who raised it and
    /// the contested point the exchange is now pinned on. Logged so resume / inspect
    /// see where the exchange was pinned and by whom.
    fn objection_raised(
        &mut self,
        session_id: &str,
        user_id: &str,
        branch_id: &str,
        turn: u64,
        objector: &str,
        text: &str,
    ) -> Result<()> {
        let event_id = self.event_id();
        self.write(json!({
            "event_id": event_id,
            "event_type": "objection_raised",
            "occurred_at": Utc::now().to_rfc3339(),
            "session_id": session_id,
            "user_id": user_id,
            "branch_id": branch_id,
            "turn": turn,
            "objector": objector,
            "text": text,
        }))
    }

    // trace:STORY-175 | ai:claude
    /// Record an objection CLEARED: either `/resolved` (the objector withdrew /
    /// accepted) or a `/judge` ruling (`sustained` / `overruled`). `resolution`
    /// carries the disposition + (for a sustained ruling) the tracked open thread.
    fn objection_cleared(
        &mut self,
        session_id: &str,
        user_id: &str,
        branch_id: &str,
        turn: u64,
        disposition: &str,
        resolution: &str,
    ) -> Result<()> {
        let event_id = self.event_id();
        self.write(json!({
            "event_id": event_id,
            "event_type": "objection_cleared",
            "occurred_at": Utc::now().to_rfc3339(),
            "session_id": session_id,
            "user_id": user_id,
            "branch_id": branch_id,
            "turn": turn,
            "disposition": disposition,
            "resolution": resolution,
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

    // trace:STORY-168 | ai:claude — drive the helper through the headless line
    // front-end seam (the engine no longer hands it a raw I/O triple).
    fn ask(line: &str) -> bool {
        let input = std::io::Cursor::new(format!("{line}\n"));
        let mut fe = crate::frontend::LineFrontEnd::new(input, Vec::new()).expect("front end");
        prompt_to_conclude(&mut fe).expect("prompt")
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
        let input = std::io::Cursor::new(Vec::new());
        let mut fe = crate::frontend::LineFrontEnd::new(input, Vec::new()).expect("front end");
        assert!(!prompt_to_conclude(&mut fe).expect("prompt"));
    }

    // ---- STORY-164: /help engine (tool-context, belief-neutral) ------------

    #[test]
    fn help_tool_context_is_built_from_the_palette_registry() {
        // trace:STORY-164 | ai:claude — the TOOL-CONTEXT /help answers from is the
        // palette command registry (the single source of truth for the controls),
        // so /help stays in sync with the live command set. It carries the controls
        // and their descriptions — the design — and no belief content.
        let context = help_tool_context();
        for command in crate::palette::command_registry() {
            assert!(
                context.contains(command.command),
                "tool-context missing {}",
                command.command
            );
        }
        // Belief-neutral: it is about the tool, never a belief.
        assert!(context.contains("Socratic belief-exploration tool"));
    }

    #[test]
    fn render_help_answer_labels_the_meta_voice_and_flags_offline() {
        // trace:STORY-164 | ai:claude — the /help answer renders in the META voice
        // (distinct from the question) and flags the offline/static-index mode so the
        // user knows when no model answered. Belief-neutral header by construction.
        let mut out = Vec::new();
        render_help_answer(
            &HelpAnswer {
                answer: "Use /rest to begin the closing ritual.".to_string(),
                degraded: false,
            },
            &mut out,
        )
        .expect("render");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("META (/help)"));
        assert!(text.to_lowercase().contains("belief-neutral"));
        assert!(text.contains("Use /rest to begin the closing ritual."));

        let mut out = Vec::new();
        render_help_answer(
            &HelpAnswer {
                answer: "Offline help.".to_string(),
                degraded: true,
            },
            &mut out,
        )
        .expect("render");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("META (/help, offline)"));
    }

    #[test]
    fn offline_observer_help_answers_from_a_static_index() {
        // trace:STORY-164 | ai:claude — end-to-end through the ObserverEngine: an
        // offline engine answers /help from the STATIC help index (built from the
        // tool-context), degrading gracefully with the controls — never a belief.
        let answer = ObserverEngine::Offline.help("how do I rest my case?");
        assert!(answer.degraded);
        assert!(answer.answer.contains("/rest"));
        assert!(answer.answer.contains("about the tool, not your belief"));
    }

    // ---- STORY-165: /tutor articulation & nuance coach ---------------------

    #[test]
    fn render_tutor_reading_labels_the_meta_voice_and_flags_offline() {
        // trace:STORY-165 | ai:claude — the /tutor reading renders in the META voice
        // (distinct from the question): reflection + distinction + missing nuance,
        // belief-neutral header. The offline header flags the structural-note mode.
        let mut out = Vec::new();
        render_tutor_reading(
            &TutorReading {
                reflection: "You seem to be getting at X — is that it?".to_string(),
                distinction: "The line between uncaused and unconstrained.".to_string(),
                missing_nuance: vec!["What 'forced' includes".to_string()],
                degraded: false,
            },
            &mut out,
        )
        .expect("render");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("META (/tutor)"));
        assert!(text.to_lowercase().contains("never supplies it"));
        assert!(text.contains("is that it?"));
        assert!(text.contains("Nuance you have not yet addressed:"));
        assert!(text.contains("What 'forced' includes"));

        let mut out = Vec::new();
        render_tutor_reading(
            &TutorReading {
                reflection: "r".to_string(),
                distinction: String::new(),
                missing_nuance: vec![],
                degraded: true,
            },
            &mut out,
        )
        .expect("render");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("META (/tutor, offline)"));
    }

    #[test]
    fn tutor_context_for_frontier_uses_typed_point_then_falls_back_to_last_answer() {
        // trace:STORY-165 | ai:claude — `/tutor <text>` sharpens the typed point; a
        // bare `/tutor` falls back to the user's most recent answer (the half-formed
        // view they are reaching to sharpen). It reads only the user's OWN words.
        let current = Question {
            id: "Q-2".to_string(),
            title: "Is free will real?".to_string(),
            tags: vec![],
            answer_kind: AnswerKind::FreeText,
            weight: 0,
        };
        let path = vec![AnsweredQuestion {
            question_ref: "Q-1".to_string(),
            question_text: "What is a choice?".to_string(),
            raw_answer: "a choice is an unforced selection".to_string(),
            normalized_answer: "a choice is an unforced selection".to_string(),
        }];

        let typed = tutor_context_for_frontier("free will is uncaused", &current, &path);
        assert_eq!(typed.question, "Is free will real?");
        assert_eq!(typed.point, "free will is uncaused");

        let bare = tutor_context_for_frontier("   ", &current, &path);
        assert_eq!(bare.point, "a choice is an unforced selection");

        let empty = tutor_context_for_frontier("", &current, &[]);
        assert_eq!(empty.point, "");
    }

    #[test]
    fn offline_observer_tutor_coaches_from_a_structural_note() {
        // trace:STORY-165 | ai:claude — end-to-end through the ObserverEngine: an
        // offline engine coaches /tutor from a STRUCTURAL note (reflect + distinction
        // + nuance), belief-neutral — it never supplies a belief or takes a side.
        let context = TutorContext {
            question: "Is free will real?".to_string(),
            point: "free will is unforced choice".to_string(),
        };
        let reading = ObserverEngine::Offline.tutor(&context);
        assert!(reading.degraded);
        assert!(reading.reflection.to_lowercase().contains("is that"));
        assert!(!reading.missing_nuance.is_empty());
        // Belief-neutral: the distinction is the USER's to draw, not the tutor's.
        assert!(reading.distinction.to_lowercase().contains("yours to draw"));
    }

    // trace:BUG-181 | ai:claude — the regression guard against a live-LLM leak in
    // tests. `for_config` is the single construction point the session loop uses,
    // and `test_config` defaults the backend to `ClaudeCli` (the production
    // default). Under test the guard must yield a NON-network `Offline` engine so
    // the score-gauge gate path (and any sibling synopsis/objection path) can never
    // spawn the real `claude` CLI (~60s + a charge). The explicit `allow_live_backend`
    // opt-in remains available for a future test that deliberately wants the live
    // path; this asserts BOTH the default block and the opt-in escape hatch.
    #[test]
    fn for_config_blocks_a_live_backend_under_test_unless_opted_in() {
        let mut config = CliConfig::parse([
            "session".to_string(),
            "start".to_string(),
            "--seed".to_string(),
            "Q-1".to_string(),
        ])
        .expect("parse");
        // The production default backend (set explicitly so the test is robust to
        // the env / default changing).
        config.llm_backend = LlmBackendKind::ClaudeCli;

        // Default: even a ClaudeCli backend resolves to the offline engine, so no
        // test can shell out to `claude` through the session loop.
        assert!(
            matches!(ObserverEngine::for_config(&config), ObserverEngine::Offline),
            "for_config must fail safe to Offline under test"
        );

        // Opt-in escape hatch: a test that genuinely wants the live path gets the
        // network-backed engine, and the setting is restored when the guard drops.
        {
            let _live = ObserverEngine::allow_live_backend();
            assert!(
                matches!(
                    ObserverEngine::for_config(&config),
                    ObserverEngine::ClaudeCli(_)
                ),
                "allow_live_backend must restore the real ClaudeCli engine"
            );
        }

        // Back to the safe default after the opt-in guard drops.
        assert!(matches!(
            ObserverEngine::for_config(&config),
            ObserverEngine::Offline
        ));
    }
}

// trace:STORY-173 | ai:claude
// Request-a-goal-when-none-is-set: the user-requested proposal (bare `/goal`
// confirm + `/request-goal` direct) and the bounded interrogator offer. Drives
// the helpers through the headless line front-end seam with a CANNED-proposal
// Observer (`ObserverEngine::Mock`), so both request paths and the bounded-offer
// guard are exercised without a live LLM. Belief-neutral throughout: every goal
// set is the QUESTION being resolved, never a belief.
#[cfg(test)]
mod goal_request_tests {
    use super::*;
    use crate::observer::GoalProposal;
    use crate::strategy::AnsweredQuestion;

    fn unique_log(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "quizdom-story-173-{tag}-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn test_config(path: &std::path::Path) -> CliConfig {
        CliConfig {
            command: SessionCommand::Start,
            seed: "Q-1".to_string(),
            user_id: "test-user".to_string(),
            session_id: "sess-test".to_string(),
            session_id_provided: true,
            log_path: path.to_path_buf(),
            log_path_provided: true,
            branch_id: "main".to_string(),
            proposition: None,
            agree_seed: None,
            disagree_seed: None,
            strategy: StrategyKind::Deterministic,
            strategy_provided: false,
            llm_backend: LlmBackendKind::ClaudeCli,
            goal: None,
            mode: SessionMode::Socratic,
            mode_provided: false,
            no_tui: false,
        }
    }

    fn proposal() -> GoalProposal {
        GoalProposal {
            goal: "can libertarian free will be held consistently?".to_string(),
            rationale: "the user keeps circling whether uncaused choice survives causation"
                .to_string(),
        }
    }

    fn answered(question: &str, answer: &str) -> AnsweredQuestion {
        AnsweredQuestion {
            question_ref: "Q-x".to_string(),
            question_text: question.to_string(),
            raw_answer: answer.to_string(),
            normalized_answer: answer.to_string(),
        }
    }

    /// Seed a session log with a couple of recorded positions so the on-demand
    /// arc has substance to read (the `arc_from_session_log` reader builds turns
    /// from `question_presented` + `answer_recorded` pairs). Belief-neutral: these
    /// are positions taken, not beliefs graded.
    fn seed_positions(path: &std::path::Path) {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open log");
        for (turn, (question, answer)) in [
            ("Is free will real?", "yes"),
            ("Can a caused choice be free?", "no"),
        ]
        .iter()
        .enumerate()
        {
            writeln!(
                file,
                r#"{{"event_type":"question_presented","branch_id":"main","turn":{turn},"question_ref":"Q-{turn}","question_text":"{question}","occurred_at":"2026-01-01T00:00:00Z"}}"#
            )
            .unwrap();
            writeln!(
                file,
                r#"{{"event_type":"answer_recorded","branch_id":"main","turn":{turn},"question_ref":"Q-{turn}","raw_answer":"{answer}","normalized_answer":"{answer}","occurred_at":"2026-01-01T00:00:01Z"}}"#
            )
            .unwrap();
        }
    }

    /// Drive `request_goal_on_demand` with a canned proposal and a scripted input
    /// line. The log is seeded with positions so the on-demand arc is non-empty.
    /// Returns `(resulting goal, rendered output)`.
    fn run_request(engine: ObserverEngine, input: &str, tag: &str) -> (Option<String>, String) {
        let path = unique_log(tag);
        seed_positions(&path);
        let config = test_config(&path);
        let mut logger = SessionLogger::open(&path).expect("logger");
        let mut goal: Option<String> = None;
        let fe_input = std::io::Cursor::new(input.as_bytes().to_vec());
        let mut fe = crate::frontend::LineFrontEnd::new(fe_input, Vec::new()).expect("front end");
        request_goal_on_demand(&mut goal, &engine, &config, &mut logger, 0, &mut fe)
            .expect("request");
        let out = String::from_utf8(fe.into_output()).unwrap();
        let _ = std::fs::remove_file(&path);
        (goal, out)
    }

    // ---- (1)+(2): the user-requested proposal — accept / edit / decline -------

    #[test]
    fn request_goal_direct_accept_sets_the_goal() {
        // `/request-goal` (and the confirmed bare `/goal`) propose directly; an
        // `accept` sets the goal exactly as `/goal <text>` would.
        let (goal, out) = run_request(ObserverEngine::Mock(Some(proposal())), "a\n", "accept");
        assert_eq!(
            goal.as_deref(),
            Some("can libertarian free will be held consistently?")
        );
        // The proposal was surfaced belief-neutrally as the QUESTION being settled.
        assert!(out.contains("can libertarian free will be held consistently?"));
        assert!(out.contains("Goal set:"));
    }

    #[test]
    fn request_goal_decline_leaves_no_goal() {
        // Declining keeps the session free-flowing — no goal is set on the user's
        // behalf (agency preserved).
        let (goal, _out) = run_request(ObserverEngine::Mock(Some(proposal())), "d\n", "decline");
        assert!(goal.is_none());
    }

    #[test]
    fn request_goal_edit_sets_the_edited_question() {
        // Editing lets the user rephrase the proposed QUESTION; the edited text is
        // what gets set, not the original proposal.
        let (goal, _out) = run_request(
            ObserverEngine::Mock(Some(proposal())),
            "e\nis determinism compatible with deliberation?\n",
            "edit",
        );
        assert_eq!(
            goal.as_deref(),
            Some("is determinism compatible with deliberation?")
        );
    }

    #[test]
    fn request_goal_blank_or_eof_declines() {
        // A blank choice / EOF declines rather than setting a goal — never set on
        // the user's behalf.
        let (goal, _out) = run_request(ObserverEngine::Mock(Some(proposal())), "\n", "blank");
        assert!(goal.is_none());
        let (goal, _out) = run_request(ObserverEngine::Mock(Some(proposal())), "", "eof");
        assert!(goal.is_none());
    }

    // ---- (4): offline degrades to a "needs an LLM backend" note ---------------

    #[test]
    fn request_goal_offline_degrades_to_a_note_not_a_proposal() {
        // No LLM backend reachable: report "no goal" with the backend note instead
        // of silently doing nothing. No goal is set.
        let (goal, out) = run_request(ObserverEngine::Offline, "a\n", "offline");
        assert!(goal.is_none());
        assert!(out.contains("needs an LLM backend"));
    }

    #[test]
    fn request_goal_reports_when_no_thesis_has_crystallized() {
        // An LLM is present but no single thesis has formed (`None` proposal): say
        // so rather than fabricating a goal. Belief-neutral: never invents a thesis.
        let (goal, out) = run_request(ObserverEngine::Mock(None), "a\n", "uncrystallized");
        assert!(goal.is_none());
        assert!(out.contains("no single thesis has crystallized"));
    }

    // ---- (3): the bounded interrogator offer — offer once, never twice --------

    /// Drive `maybe_offer_goal_on_crystallize` once with a scripted input and the
    /// given recorded positions. Returns `(goal, offer_made, output)`.
    fn run_offer(
        engine: &ObserverEngine,
        path: &std::path::Path,
        goal: &mut Option<String>,
        offer_made: &mut bool,
        recent_path: &[AnsweredQuestion],
        input: &str,
    ) -> String {
        let config = test_config(path);
        let mut logger = SessionLogger::open(path).expect("logger");
        let fe_input = std::io::Cursor::new(input.as_bytes().to_vec());
        let mut fe = crate::frontend::LineFrontEnd::new(fe_input, Vec::new()).expect("front end");
        maybe_offer_goal_on_crystallize(
            goal,
            offer_made,
            engine,
            recent_path,
            &config,
            &mut logger,
            1,
            &mut fe,
        )
        .expect("offer");
        String::from_utf8(fe.into_output()).unwrap()
    }

    #[test]
    fn interrogator_offers_a_goal_once_and_not_twice() {
        // With a crystallized thesis and no goal, the interrogator offers ONCE.
        // After that single offer the one-shot guard is spent — a second call never
        // re-offers (never nags), even though a thesis is still available.
        let path = unique_log("offer-once");
        let engine = ObserverEngine::Mock(Some(proposal()));
        let recent_path = vec![
            answered("Is free will real?", "yes"),
            answered("Can a caused choice be free?", "no"),
        ];
        let mut goal: Option<String> = None;
        let mut offer_made = false;

        // First call: a goal is offered (user declines), and the guard is spent.
        let first = run_offer(
            &engine,
            &path,
            &mut goal,
            &mut offer_made,
            &recent_path,
            "d\n",
        );
        assert!(offer_made, "the first offer must spend the one-shot guard");
        assert!(goal.is_none(), "declining leaves the session free-flowing");
        assert!(
            first.contains("can libertarian free will be held consistently?"),
            "the first call must surface the offer: {first}"
        );

        // Second call: NEVER re-offered — no proposal is surfaced again.
        let second = run_offer(
            &engine,
            &path,
            &mut goal,
            &mut offer_made,
            &recent_path,
            "a\n",
        );
        assert!(
            !second.contains("we seem to be exploring"),
            "the offer must never repeat: {second}"
        );
        assert!(goal.is_none(), "a spent offer never sets a goal");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn interrogator_offer_accepts_and_sets_the_goal() {
        // Accepting the single offer sets the goal (logged source observer), which
        // then orients questioning + roundedness per STORY-159.
        let path = unique_log("offer-accept");
        let engine = ObserverEngine::Mock(Some(proposal()));
        let recent_path = vec![
            answered("Is free will real?", "yes"),
            answered("Can a caused choice be free?", "no"),
        ];
        let mut goal: Option<String> = None;
        let mut offer_made = false;
        let out = run_offer(
            &engine,
            &path,
            &mut goal,
            &mut offer_made,
            &recent_path,
            "a\n",
        );
        assert!(offer_made);
        assert_eq!(
            goal.as_deref(),
            Some("can libertarian free will be held consistently?")
        );
        assert!(out.contains("Goal set:"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn interrogator_does_not_offer_early_on_a_thin_conversation() {
        // Honor free-flow: a thin conversation (fewer than two recorded positions)
        // yields no offer, and the one-shot guard is NOT spent — the offer can still
        // surface on a later, more-formed turn.
        let path = unique_log("offer-thin");
        let engine = ObserverEngine::Mock(Some(proposal()));
        let recent_path = vec![answered("Is free will real?", "yes")];
        let mut goal: Option<String> = None;
        let mut offer_made = false;
        let out = run_offer(
            &engine,
            &path,
            &mut goal,
            &mut offer_made,
            &recent_path,
            "a\n",
        );
        assert!(!offer_made, "a thin conversation must not spend the guard");
        assert!(goal.is_none());
        assert!(out.is_empty(), "no offer should be surfaced: {out}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn interrogator_does_not_offer_when_a_goal_is_already_set() {
        // A session that already has a goal is never offered another (no nag).
        let path = unique_log("offer-has-goal");
        let engine = ObserverEngine::Mock(Some(proposal()));
        let recent_path = vec![
            answered("Is free will real?", "yes"),
            answered("Can a caused choice be free?", "no"),
        ];
        let mut goal: Option<String> = Some("is determinism true?".to_string());
        let mut offer_made = false;
        let out = run_offer(
            &engine,
            &path,
            &mut goal,
            &mut offer_made,
            &recent_path,
            "a\n",
        );
        assert_eq!(goal.as_deref(), Some("is determinism true?"));
        assert!(out.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn interrogator_does_not_spend_the_guard_until_a_thesis_crystallizes() {
        // Enough substance but NO crystallized thesis (`None` proposal): no offer is
        // surfaced and the guard is preserved, so a later turn can still offer.
        let path = unique_log("offer-none");
        let engine = ObserverEngine::Mock(None);
        let recent_path = vec![
            answered("Is free will real?", "yes"),
            answered("Can a caused choice be free?", "no"),
        ];
        let mut goal: Option<String> = None;
        let mut offer_made = false;
        let out = run_offer(
            &engine,
            &path,
            &mut goal,
            &mut offer_made,
            &recent_path,
            "a\n",
        );
        assert!(
            !offer_made,
            "an un-crystallized turn must not spend the guard"
        );
        assert!(goal.is_none());
        assert!(out.is_empty());
        let _ = std::fs::remove_file(&path);
    }
}

// trace:STORY-175 | ai:claude
// The court-case `/objection` mechanic: raise+pin, the ASYMMETRIC exits
// (`/resolved` objector-only, `/judge` other-party-only), the wrong-caller
// rejections, the SUSTAINED (=> tracked open thread widens the gauge) + OVERRULED
// rulings, the one-at-a-time guard, the bounded interrogator self-objection, and
// the offline `/judge` degrade. Drives the objection helpers through the headless
// line front-end seam with a Mock / MockJudge observer, so the full mechanic is
// exercised without a live LLM. Belief-neutral throughout: an objection names a
// STRUCTURAL tension and a ruling judges STANDING, never which belief is true.
#[cfg(test)]
mod objection_tests {
    use super::*;
    use crate::observer::{JudgeRuling, JudgeVerdict};
    use crate::strategy::AnsweredQuestion;

    fn unique_log(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "quizdom-story-175-{tag}-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn test_config(path: &std::path::Path) -> CliConfig {
        CliConfig {
            command: SessionCommand::Start,
            seed: "Q-1".to_string(),
            user_id: "test-user".to_string(),
            session_id: "sess-test".to_string(),
            session_id_provided: true,
            log_path: path.to_path_buf(),
            log_path_provided: true,
            branch_id: "main".to_string(),
            proposition: None,
            agree_seed: None,
            disagree_seed: None,
            strategy: StrategyKind::Deterministic,
            strategy_provided: false,
            llm_backend: LlmBackendKind::ClaudeCli,
            goal: None,
            mode: SessionMode::Socratic,
            mode_provided: false,
            no_tui: false,
        }
    }

    fn sustained_ruling() -> JudgeRuling {
        JudgeRuling {
            verdict: JudgeVerdict::Sustained,
            rationale: "the point is material and was never addressed".to_string(),
            resolving_condition: "define whether a caused choice counts as free".to_string(),
            degraded: false,
        }
    }

    fn overruled_ruling() -> JudgeRuling {
        JudgeRuling {
            verdict: JudgeVerdict::Overruled,
            rationale: "already addressed two turns ago".to_string(),
            resolving_condition: "none — it was covered".to_string(),
            degraded: false,
        }
    }

    fn answered(question: &str, answer: &str) -> AnsweredQuestion {
        AnsweredQuestion {
            question_ref: "Q-x".to_string(),
            question_text: question.to_string(),
            raw_answer: answer.to_string(),
            normalized_answer: answer.to_string(),
        }
    }

    // ---- raise + pin -------------------------------------------------------

    #[test]
    fn raising_an_objection_pins_the_exchange_and_emits_the_gavel_motif() {
        let path = unique_log("raise");
        let config = test_config(&path);
        let mut logger = SessionLogger::open(&path).expect("logger");
        let mut state: Option<ObjectionState> = None;
        let mut out = Vec::new();
        raise_objection(
            &mut state,
            "you never defined what 'free' means",
            ObjectionParty::User,
            &config,
            &mut logger,
            0,
            &mut out,
        )
        .expect("raise");
        let state = state.expect("objection must be pinned");
        assert_eq!(state.text, "you never defined what 'free' means");
        assert_eq!(state.objector, ObjectionParty::User);
        let out = String::from_utf8(out).unwrap();
        // The machine-readable motif the TUI mirrors + the headless gavel footer.
        assert!(out.contains("[objection: you never defined what 'free' means (user)]"));
        assert!(out.contains(crate::style::OBJECTION_GAVEL));
        let _ = std::fs::remove_file(&path);
    }

    // ---- one-at-a-time guard ----------------------------------------------

    #[test]
    fn a_second_objection_is_refused_while_one_is_open() {
        let path = unique_log("one-at-a-time");
        let config = test_config(&path);
        let mut logger = SessionLogger::open(&path).expect("logger");
        let mut state = Some(ObjectionState {
            text: "first contested point".to_string(),
            objector: ObjectionParty::User,
        });
        let mut out = Vec::new();
        raise_objection(
            &mut state,
            "a different point",
            ObjectionParty::User,
            &config,
            &mut logger,
            1,
            &mut out,
        )
        .expect("raise");
        // The open objection is unchanged; the second is refused with the note.
        assert_eq!(state.as_ref().unwrap().text, "first contested point");
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("resolve the open objection first"));
        let _ = std::fs::remove_file(&path);
    }

    // ---- /resolved by the objector ----------------------------------------

    #[test]
    fn resolved_clears_the_objection_for_the_objector() {
        let path = unique_log("resolved-ok");
        let config = test_config(&path);
        let mut logger = SessionLogger::open(&path).expect("logger");
        let mut state = Some(ObjectionState {
            text: "the contested point".to_string(),
            objector: ObjectionParty::User,
        });
        let mut out = Vec::new();
        let cleared = resolve_objection(
            &mut state,
            ObjectionParty::User, // the objector
            &config,
            &mut logger,
            2,
            &mut out,
        )
        .expect("resolve");
        assert!(cleared);
        assert!(state.is_none(), "objection must be cleared");
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("Objection resolved by the objector"));
        assert!(out.contains("[objection: clear]"));
        let _ = std::fs::remove_file(&path);
    }

    // ---- wrong-caller rejection for /resolved ------------------------------

    #[test]
    fn resolved_rejects_the_wrong_caller() {
        let path = unique_log("resolved-wrong");
        let config = test_config(&path);
        let mut logger = SessionLogger::open(&path).expect("logger");
        let mut state = Some(ObjectionState {
            text: "the contested point".to_string(),
            objector: ObjectionParty::Interrogator, // interrogator raised it
        });
        let mut out = Vec::new();
        let cleared = resolve_objection(
            &mut state,
            ObjectionParty::User, // the WRONG caller (not the objector)
            &config,
            &mut logger,
            3,
            &mut out,
        )
        .expect("resolve");
        assert!(!cleared);
        assert!(
            state.is_some(),
            "a wrong-caller must not clear the objection"
        );
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("Only the party who RAISED the objection"));
        assert!(out.contains("/judge")); // points them at the right control
        let _ = std::fs::remove_file(&path);
    }

    // ---- /judge SUSTAINED => tracked open thread lowers the gauge ----------

    #[test]
    fn judge_sustained_clears_and_tracks_an_open_thread_that_lowers_the_gauge() {
        let path = unique_log("judge-sustained");
        let config = test_config(&path);
        let mut logger = SessionLogger::open(&path).expect("logger");
        let observer = ObserverEngine::MockJudge(sustained_ruling());
        let mut state = Some(ObjectionState {
            text: "you never reconciled free will with causation".to_string(),
            objector: ObjectionParty::User, // user objected, so the OTHER party judges
        });
        let mut out = Vec::new();
        let outcome = judge_objection(
            &mut state,
            ObjectionParty::Interrogator, // the non-objecting party
            &observer,
            "Q: Is free will real? A: yes",
            Some("can free will survive causation?"),
            &config,
            &mut logger,
            4,
            &mut out,
        )
        .expect("judge");
        assert!(state.is_none(), "a ruling clears the objection");
        // SUSTAINED => the resolving condition becomes the tracked open thread.
        assert_eq!(
            outcome.open_thread.as_deref(),
            Some("define whether a caused choice counts as free")
        );
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("SUSTAINED"));
        assert!(out.contains("Tracked as an open thread"));
        assert!(out.contains("[objection: clear]"));

        // The tracked thread WIDENS the gauge: fold it into a scored gauge and the
        // composite drops + the open thread becomes the named gap.
        let base = crate::synopsis::ScoreGauge {
            composite: Some(80),
            limiting_gap: "completeness".to_string(),
            goal: Some("can free will survive causation?".to_string()),
            degraded: false,
        };
        let widened = base
            .clone()
            .with_open_thread(outcome.open_thread.as_deref().unwrap());
        assert!(widened.composite.unwrap() < base.composite.unwrap());
        assert!(widened
            .status_segment(true)
            .contains("define whether a caused choice counts as free"));
        let _ = std::fs::remove_file(&path);
    }

    // ---- /judge OVERRULED => clears, nothing tracked -----------------------

    #[test]
    fn judge_overruled_clears_and_tracks_nothing() {
        let path = unique_log("judge-overruled");
        let config = test_config(&path);
        let mut logger = SessionLogger::open(&path).expect("logger");
        let observer = ObserverEngine::MockJudge(overruled_ruling());
        let mut state = Some(ObjectionState {
            text: "an immaterial nitpick".to_string(),
            objector: ObjectionParty::User,
        });
        let mut out = Vec::new();
        let outcome = judge_objection(
            &mut state,
            ObjectionParty::Interrogator,
            &observer,
            "",
            None,
            &config,
            &mut logger,
            5,
            &mut out,
        )
        .expect("judge");
        assert!(state.is_none(), "a ruling clears the objection");
        assert!(
            outcome.open_thread.is_none(),
            "an overruled objection tracks nothing"
        );
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("OVERRULED"));
        assert!(!out.contains("Tracked as an open thread"));
        let _ = std::fs::remove_file(&path);
    }

    // ---- wrong-caller rejection for /judge ---------------------------------

    #[test]
    fn judge_rejects_the_objector_as_the_wrong_caller() {
        let path = unique_log("judge-wrong");
        let config = test_config(&path);
        let mut logger = SessionLogger::open(&path).expect("logger");
        let observer = ObserverEngine::MockJudge(sustained_ruling());
        let mut state = Some(ObjectionState {
            text: "the contested point".to_string(),
            objector: ObjectionParty::User,
        });
        let mut out = Vec::new();
        let outcome = judge_objection(
            &mut state,
            ObjectionParty::User, // the OBJECTOR may NOT judge their own objection
            &observer,
            "",
            None,
            &config,
            &mut logger,
            6,
            &mut out,
        )
        .expect("judge");
        assert!(outcome.open_thread.is_none());
        assert!(
            state.is_some(),
            "a wrong-caller must not clear the objection"
        );
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("Only the OTHER party"));
        assert!(out.contains("/resolved")); // points them at the right control
        let _ = std::fs::remove_file(&path);
    }

    // ---- offline /judge degrades to a note, keeps the objection open -------

    #[test]
    fn judge_offline_degrades_and_keeps_the_objection_open() {
        let path = unique_log("judge-offline");
        let config = test_config(&path);
        let mut logger = SessionLogger::open(&path).expect("logger");
        let observer = ObserverEngine::Offline; // no LLM to rule
        let mut state = Some(ObjectionState {
            text: "the contested point".to_string(),
            objector: ObjectionParty::User,
        });
        let mut out = Vec::new();
        let outcome = judge_objection(
            &mut state,
            ObjectionParty::Interrogator, // the right caller, but offline
            &observer,
            "",
            None,
            &config,
            &mut logger,
            7,
            &mut out,
        )
        .expect("judge");
        assert!(outcome.open_thread.is_none());
        // The objection stays OPEN — the objector can still /resolved it.
        assert!(
            state.is_some(),
            "offline /judge must not clear the objection"
        );
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("needs an LLM backend"));
        let _ = std::fs::remove_file(&path);
    }

    // ---- /objection and /resolved are pure state transitions (work offline) --

    #[test]
    fn objection_and_resolved_are_pure_state_transitions_offline() {
        // Belief-neutral plumbing: raising and resolving need no LLM — only /judge
        // does. So with NO observer involved at all, raise+resolve still work.
        let path = unique_log("offline-transitions");
        let config = test_config(&path);
        let mut logger = SessionLogger::open(&path).expect("logger");
        let mut state: Option<ObjectionState> = None;
        let mut out = Vec::new();
        raise_objection(
            &mut state,
            "a contested point",
            ObjectionParty::User,
            &config,
            &mut logger,
            0,
            &mut out,
        )
        .expect("raise");
        assert!(state.is_some());
        let cleared = resolve_objection(
            &mut state,
            ObjectionParty::User,
            &config,
            &mut logger,
            1,
            &mut out,
        )
        .expect("resolve");
        assert!(cleared);
        assert!(state.is_none());
        let _ = std::fs::remove_file(&path);
    }

    // ---- bounded interrogator self-objection -------------------------------

    #[test]
    fn interrogator_objects_once_rarely_and_never_twice() {
        let path = unique_log("interrogator-once");
        let config = test_config(&path);
        let mut logger = SessionLogger::open(&path).expect("logger");
        // The Mock self-objects when it has a canned proposal + ≥2 positions.
        let observer = ObserverEngine::Mock(Some(crate::observer::GoalProposal {
            goal: "whether free will survives causation".to_string(),
            rationale: "circling".to_string(),
        }));
        let recent_path = vec![
            answered("Is free will real?", "yes"),
            answered("Can a caused choice be free?", "no"),
        ];
        let mut state: Option<ObjectionState> = None;
        let mut made = false;

        // First turn: the interrogator raises its own objection, spending the guard.
        let mut out = Vec::new();
        maybe_interrogator_objection(
            &mut state,
            &mut made,
            &observer,
            &recent_path,
            &config,
            &mut logger,
            1,
            &mut out,
        )
        .expect("offer");
        assert!(made, "the first material tension spends the one-shot guard");
        let raised = state.take().expect("the interrogator must have objected");
        assert_eq!(raised.objector, ObjectionParty::Interrogator);
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("interrogator raises an objection"));

        // Second turn: NEVER re-objects (the guard is spent), even with substance.
        let mut state2: Option<ObjectionState> = None;
        let mut out2 = Vec::new();
        maybe_interrogator_objection(
            &mut state2,
            &mut made,
            &observer,
            &recent_path,
            &config,
            &mut logger,
            2,
            &mut out2,
        )
        .expect("offer");
        assert!(
            state2.is_none(),
            "a spent guard must never raise a second interrogator objection"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn interrogator_stays_quiet_on_a_thin_conversation_and_never_offline() {
        let path = unique_log("interrogator-thin");
        let config = test_config(&path);
        let mut logger = SessionLogger::open(&path).expect("logger");
        let observer = ObserverEngine::Mock(Some(crate::observer::GoalProposal {
            goal: "g".to_string(),
            rationale: "r".to_string(),
        }));
        // Thin: a single position must not spend the guard or object.
        let thin = vec![answered("Is free will real?", "yes")];
        let mut state: Option<ObjectionState> = None;
        let mut made = false;
        let mut out = Vec::new();
        maybe_interrogator_objection(
            &mut state,
            &mut made,
            &observer,
            &thin,
            &config,
            &mut logger,
            1,
            &mut out,
        )
        .expect("offer");
        assert!(!made, "a thin conversation must not spend the guard");
        assert!(state.is_none());

        // Offline: never objects, even with substance.
        let recent_path = vec![
            answered("Is free will real?", "yes"),
            answered("Can a caused choice be free?", "no"),
        ];
        let mut made2 = false;
        let mut out2 = Vec::new();
        maybe_interrogator_objection(
            &mut state,
            &mut made2,
            &ObserverEngine::Offline,
            &recent_path,
            &config,
            &mut logger,
            2,
            &mut out2,
        )
        .expect("offer");
        assert!(!made2, "offline must never object");
        assert!(state.is_none());
        let _ = std::fs::remove_file(&path);
    }
}
