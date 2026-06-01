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
    read_answer_or_end, render_question, render_question_for, AnswerInput, FreeTextInput,
    InputContext,
};
use crate::model::{Answer, AnswerKind, Question, TermDefinition};
use crate::persist::{
    AidaCliGeneratedQuestionPersister, AidaCliQuestionReweighter, AidaCliUserSpecificTermPersister,
    QuestionReweighter, UserSpecificTermPersister,
};
#[cfg(test)]
use crate::persist::{NoopQuestionReweighter, NoopUserSpecificTermPersister};
use crate::strategy::{
    different_topic_punt_question, AnsweredQuestion, QualitySignal, StrategyContext,
};
use crate::strategy::{
    DeterministicNextQuestionStrategy, LlmNextQuestionStrategy, NextQuestionStrategy,
    WeightedNextQuestionStrategy,
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
        })
    }
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
    if !config.session_id_provided && !config.log_path_provided {
        let summary =
            latest_session_summary(&session_log_dir(&config.user_id))?.ok_or_else(|| {
                QuizdomError::Usage(format!("no sessions found for user {}", config.user_id))
            })?;
        config.session_id = summary.session_id;
        config.log_path = summary.path;
    }
    // trace:BUG-71 | ai:codex
    if !config.strategy_provided {
        if let Some(metadata) = SessionStrategyMetadata::load(&config.log_path, &config.branch_id)?
        {
            config.strategy = metadata.strategy;
            if let Some(llm_backend) = metadata.llm_backend {
                config.llm_backend = llm_backend;
            }
        }
    }
    Ok(config)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct SessionStrategyMetadata {
    strategy: StrategyKind,
    llm_backend: Option<LlmBackendKind>,
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
            if value.get("event_type").and_then(Value::as_str) != Some("session_started") {
                continue;
            }
            let Some(strategy_value) = value.get("strategy").and_then(Value::as_str) else {
                return Ok(None);
            };
            let strategy = parse_strategy(strategy_value)?;
            let llm_backend = value
                .get("llm_backend")
                .and_then(Value::as_str)
                .map(parse_llm_backend)
                .transpose()?;
            return Ok(Some(Self {
                strategy,
                llm_backend,
            }));
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
    run_session_from_current(
        config,
        bank,
        strategy,
        term_persister,
        &contradiction_edges,
        &contradiction_resolution_persister,
        &question_reweighter,
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
        input,
        output,
        0,
        true,
        Vec::new(),
    )
}

fn run_session_from_current(
    config: &CliConfig,
    bank: &dyn QuestionBank,
    strategy: &dyn NextQuestionStrategy,
    term_persister: &dyn UserSpecificTermPersister,
    contradiction_edges: &dyn ContradictsEdges,
    contradiction_resolution_persister: &dyn ContradictionResolutionPersister,
    question_reweighter: &dyn QuestionReweighter,
    input: impl Read,
    output: &mut impl Write,
    mut turn: u64,
    write_start_event: bool,
    mut recent_path: Vec<AnsweredQuestion>,
) -> Result<()> {
    let mut input = BufReader::new(input);
    let mut free_text_input = FreeTextInput::from_stdin()?;
    let mut logger = SessionLogger::open(&config.log_path)?;
    let mut current = bank.load_question(&config.seed)?;
    let mut settled_terms = Vec::new();
    let mut surfaced_contradictions = BTreeSet::new();
    let mut pending_revision: Option<(usize, Question, Answer)> = None;

    if write_start_event {
        logger.session_started(
            &config.session_id,
            &config.user_id,
            &config.branch_id,
            &current.id,
            config.strategy,
            config.llm_backend,
        )?;
    }

    loop {
        let (answered_turn, answer) =
            if let Some((index, revised_question, revised_answer)) = pending_revision.take() {
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
                                writeln!(output, "Session ended.")?;
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
                    AnswerInput::Forward => continue,
                    AnswerInput::End => {
                        writeln!(output, "Session ended.")?;
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
                    writeln!(output, "No different-topic questions. Session complete.")?;
                    logger.session_ended(
                        &config.session_id,
                        &config.user_id,
                        &config.branch_id,
                        answered_turn,
                        "No different-topic punt target.",
                    )?;
                    break;
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
        };

        match strategy.next_question(&current, &context, bank)? {
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
                writeln!(output, "No follow-up questions. Session complete.")?;
                logger.session_ended(
                    &config.session_id,
                    &config.user_id,
                    &config.branch_id,
                    answered_turn,
                    "No outgoing begets successor.",
                )?;
                break;
            }
        }
    }

    Ok(())
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
            writeln!(output, "Session ended.")?;
            logger.session_ended(
                &config.session_id,
                &config.user_id,
                &config.branch_id,
                turn,
                "User ended session.",
            )?;
            Ok(true)
        }
        AnswerInput::Back | AnswerInput::Forward => Ok(false),
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

fn browse_answered_path(
    bank: &dyn QuestionBank,
    recent_path: &[AnsweredQuestion],
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

    let Some(next_question_ref) = replay.next_question_ref.as_ref() else {
        writeln!(output, "No saved follow-up question. Session complete.")?;
        return Ok(());
    };

    let mut resumed_config = config.clone();
    resumed_config.seed = next_question_ref.clone();
    let recent_path = replay.recent_path();
    let question_reweighter = AidaCliQuestionReweighter::default();
    run_session_from_current(
        &resumed_config,
        bank,
        strategy,
        term_persister,
        &AidaCliContradictsEdges::default(),
        &AidaCliContradictionResolutionPersister::default(),
        &question_reweighter,
        input,
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

pub(crate) fn latest_session_summary(dir: &Path) -> Result<Option<SessionSummary>> {
    Ok(session_summaries(dir)?.into_iter().next())
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

    fn session_started(
        &mut self,
        session_id: &str,
        user_id: &str,
        branch_id: &str,
        seed_question_ref: &str,
        strategy: StrategyKind,
        llm_backend: LlmBackendKind,
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
