use chrono::Utc;
use llm::{AnthropicClient, LLMClient, Message};
use serde_json::json;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DEFAULT_SEED: &str = "Q-23";
const DEFAULT_USER: &str = "local-user";
const SOCRATIC_SYSTEM_PROMPT: &str = "You are quizdom's Socratic belief-exploration engine. There are no correct answers. Explore and challenge the user's beliefs, probe semantic nuance, and prefer formal or shared definitions before bespoke meanings. Decide whether to select an existing follow-up question or generate one new concise follow-up question.";

#[derive(Debug)]
pub enum QuizdomError {
    Io(io::Error),
    Aida(String),
    Parse(String),
    Usage(String),
}

impl fmt::Display for QuizdomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Aida(message) | Self::Parse(message) | Self::Usage(message) => {
                write!(f, "{message}")
            }
        }
    }
}

impl std::error::Error for QuizdomError {}

impl From<io::Error> for QuizdomError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub type Result<T> = std::result::Result<T, QuizdomError>;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Question {
    pub id: String,
    pub title: String,
    pub tags: Vec<String>,
    pub answer_kind: AnswerKind,
    pub weight: u32,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum AnswerKind {
    YesNo,
    Choice(Vec<String>),
    FreeText,
}

impl AnswerKind {
    pub fn mode(&self) -> String {
        match self {
            Self::YesNo => "yes-no".to_string(),
            Self::Choice(options) => format!("choice[{}]", options.join(",")),
            Self::FreeText => "free-text".to_string(),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Answer {
    pub raw: String,
    pub normalized: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct QuestionRef {
    pub id: String,
}

pub trait QuestionBank {
    fn load_question(&self, id: &str) -> Result<Question>;
    fn begets(&self, id: &str) -> Result<Vec<QuestionRef>>;
}

pub trait GeneratedQuestionPersister {
    fn persist_generated_question(
        &self,
        origin: &Question,
        question: &Question,
    ) -> Result<Question>;
}

pub trait NextQuestionStrategy {
    fn next_question(
        &self,
        current: &Question,
        context: &StrategyContext,
        bank: &dyn QuestionBank,
    ) -> Result<Option<Question>>;
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct StrategyContext {
    pub answer: Answer,
    pub recent_path: Vec<AnsweredQuestion>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AnsweredQuestion {
    pub question_ref: String,
    pub question_text: String,
    pub raw_answer: String,
    pub normalized_answer: String,
}

pub struct DeterministicNextQuestionStrategy;

pub struct NoopGeneratedQuestionPersister;

impl GeneratedQuestionPersister for NoopGeneratedQuestionPersister {
    fn persist_generated_question(
        &self,
        _origin: &Question,
        question: &Question,
    ) -> Result<Question> {
        Ok(question.clone())
    }
}

impl NextQuestionStrategy for DeterministicNextQuestionStrategy {
    fn next_question(
        &self,
        current: &Question,
        _context: &StrategyContext,
        bank: &dyn QuestionBank,
    ) -> Result<Option<Question>> {
        let successors = sorted_successors(current, bank)?;
        Ok(successors.into_iter().next())
    }
}

fn successor_questions(current: &Question, bank: &dyn QuestionBank) -> Result<Vec<Question>> {
    bank.begets(&current.id)?
        .into_iter()
        .map(|question_ref| bank.load_question(&question_ref.id))
        .collect()
}

fn sorted_successors(current: &Question, bank: &dyn QuestionBank) -> Result<Vec<Question>> {
    let mut successors = successor_questions(current, bank)?;
    successors.sort_by(|left, right| {
        right
            .weight
            .cmp(&left.weight)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(successors)
}

fn strategy_prompt(
    current: &Question,
    context: &StrategyContext,
    candidates: &[Question],
) -> String {
    let mut prompt = format!(
        "Current question ({id}): {title}\nAnswer mode: {mode}\nUser raw answer: {raw}\nUser normalized answer: {normalized}\n\nRecent path:\n",
        id = current.id,
        title = current.title,
        mode = current.answer_kind.mode(),
        raw = context.answer.raw,
        normalized = context.answer.normalized,
    );
    for item in &context.recent_path {
        prompt.push_str(&format!(
            "- {}: {} => {}\n",
            item.question_ref, item.question_text, item.raw_answer
        ));
    }
    prompt.push_str("\nCandidate bank questions:\n");
    if candidates.is_empty() {
        prompt.push_str("(none)\n");
    }
    for candidate in candidates {
        prompt.push_str(&format!(
            "- {} [weight:{} {}]: {}\n",
            candidate.id,
            candidate.weight,
            candidate.answer_kind.mode(),
            candidate.title
        ));
    }
    prompt.push_str(
        "\nReturn only JSON. To select a bank question: {\"action\":\"select\",\"id\":\"Q-...\"}. To generate a question: {\"action\":\"generate\",\"question\":\"...\",\"answer_mode\":\"yes-no|free-text\"}.",
    );
    prompt
}

fn apply_llm_decision(text: &str, candidates: &[Question]) -> Result<Option<Question>> {
    let value: Value = serde_json::from_str(text.trim())
        .map_err(|error| QuizdomError::Parse(format!("invalid LLM strategy JSON: {error}")))?;
    match value.get("action").and_then(Value::as_str) {
        Some("select") => {
            let id = value
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| QuizdomError::Parse("LLM select decision missing id".to_string()))?;
            Ok(candidates
                .iter()
                .find(|candidate| candidate.id == id)
                .cloned())
        }
        Some("generate") => {
            let title = value
                .get("question")
                .and_then(Value::as_str)
                .filter(|question| !question.trim().is_empty())
                .ok_or_else(|| {
                    QuizdomError::Parse("LLM generate decision missing question".to_string())
                })?;
            if let Some(existing) = find_near_identical_question(title, candidates) {
                return Ok(Some(existing.clone()));
            }
            let answer_kind = match value
                .get("answer_mode")
                .and_then(Value::as_str)
                .unwrap_or("free-text")
            {
                "yes-no" => AnswerKind::YesNo,
                "free-text" => AnswerKind::FreeText,
                other if other.starts_with("choice[") => {
                    answer_kind_from_tags(&[format!("answer:{other}")])
                        .unwrap_or(AnswerKind::FreeText)
                }
                _ => AnswerKind::FreeText,
            };
            Ok(Some(Question {
                id: "generated:llm".to_string(),
                title: title.trim().to_string(),
                tags: vec![
                    "generated".to_string(),
                    format!("answer:{}", answer_kind.mode()),
                ],
                answer_kind,
                weight: 0,
            }))
        }
        _ => Err(QuizdomError::Parse(
            "LLM strategy decision must use action select or generate".to_string(),
        )),
    }
}

fn find_near_identical_question<'a>(
    title: &str,
    candidates: &'a [Question],
) -> Option<&'a Question> {
    let normalized_title = normalize_question_title(title);
    candidates
        .iter()
        .find(|candidate| normalize_question_title(&candidate.title) == normalized_title)
}

fn normalize_question_title(title: &str) -> String {
    title
        .trim()
        .trim_end_matches('?')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

pub struct LlmNextQuestionStrategy<C, P = NoopGeneratedQuestionPersister> {
    client: C,
    deterministic: DeterministicNextQuestionStrategy,
    generated_question_persister: P,
}

impl<C> LlmNextQuestionStrategy<C> {
    pub fn new(client: C) -> Self {
        Self {
            client,
            deterministic: DeterministicNextQuestionStrategy,
            generated_question_persister: NoopGeneratedQuestionPersister,
        }
    }
}

impl<C, P> LlmNextQuestionStrategy<C, P> {
    pub fn with_generated_question_persister(client: C, generated_question_persister: P) -> Self {
        Self {
            client,
            deterministic: DeterministicNextQuestionStrategy,
            generated_question_persister,
        }
    }
}

impl<C, P> NextQuestionStrategy for LlmNextQuestionStrategy<C, P>
where
    C: LLMClient,
    P: GeneratedQuestionPersister,
{
    fn next_question(
        &self,
        current: &Question,
        context: &StrategyContext,
        bank: &dyn QuestionBank,
    ) -> Result<Option<Question>> {
        // trace:STORY-37 | ai:codex
        match self.llm_next_question(current, context, bank) {
            Ok(next) => Ok(next),
            Err(_) => self.deterministic.next_question(current, context, bank),
        }
    }
}

impl<C, P> LlmNextQuestionStrategy<C, P>
where
    C: LLMClient,
    P: GeneratedQuestionPersister,
{
    fn llm_next_question(
        &self,
        current: &Question,
        context: &StrategyContext,
        bank: &dyn QuestionBank,
    ) -> Result<Option<Question>> {
        let candidates = successor_questions(current, bank).unwrap_or_default();
        let prompt = strategy_prompt(current, context, &candidates);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(QuizdomError::Io)?;
        let (text, _tool_calls) = runtime
            .block_on(
                self.client
                    .call(SOCRATIC_SYSTEM_PROMPT, &[Message::user(prompt)], &[]),
            )
            .map_err(|error| QuizdomError::Aida(error.to_string()))?;
        let next = apply_llm_decision(&text, &candidates)?;
        match next {
            Some(question) if question.id == "generated:llm" => self
                .generated_question_persister
                .persist_generated_question(current, &question)
                .map(Some),
            other => Ok(other),
        }
    }
}

pub struct AidaCliQuestionBank {
    command: String,
}

impl Default for AidaCliQuestionBank {
    fn default() -> Self {
        Self {
            command: "aida".to_string(),
        }
    }
}

impl QuestionBank for AidaCliQuestionBank {
    fn load_question(&self, id: &str) -> Result<Question> {
        let output = Command::new(&self.command).args(["show", id]).output()?;
        if !output.status.success() {
            return Err(QuizdomError::Aida(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        parse_question_show(&String::from_utf8_lossy(&output.stdout))
    }

    fn begets(&self, id: &str) -> Result<Vec<QuestionRef>> {
        let output = Command::new(&self.command)
            .args(["rel", "list", id, "--type", "begets"])
            .output()?;
        if !output.status.success() {
            return Err(QuizdomError::Aida(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        Ok(parse_begets_rel_list(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }
}

trait CommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<Output>;
}

struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<Output> {
        Command::new(program)
            .args(args)
            .output()
            .map_err(Into::into)
    }
}

struct AidaCliGeneratedQuestionPersister<R = SystemCommandRunner> {
    command: String,
    runner: R,
}

impl Default for AidaCliGeneratedQuestionPersister<SystemCommandRunner> {
    fn default() -> Self {
        Self {
            command: "aida".to_string(),
            runner: SystemCommandRunner,
        }
    }
}

impl<R> AidaCliGeneratedQuestionPersister<R>
where
    R: CommandRunner,
{
    #[cfg(test)]
    fn new(command: impl Into<String>, runner: R) -> Self {
        Self {
            command: command.into(),
            runner,
        }
    }
}

impl<R> GeneratedQuestionPersister for AidaCliGeneratedQuestionPersister<R>
where
    R: CommandRunner,
{
    fn persist_generated_question(
        &self,
        origin: &Question,
        question: &Question,
    ) -> Result<Question> {
        // trace:STORY-38 | ai:codex
        let topic = question_topic(origin);
        let tags = generated_question_tags(&topic, &question.answer_kind);
        let description = generated_question_description(question, origin);
        let add_args = vec![
            "add".to_string(),
            "--prefix".to_string(),
            "Q".to_string(),
            "--type".to_string(),
            "functional".to_string(),
            "--status".to_string(),
            "approved".to_string(),
            "--priority".to_string(),
            "medium".to_string(),
            "--title".to_string(),
            question.title.clone(),
            "--description".to_string(),
            description,
            "--tags".to_string(),
            tags.join(","),
        ];
        let add_output = self.runner.run(&self.command, &add_args)?;
        if !add_output.status.success() {
            return Err(QuizdomError::Aida(
                String::from_utf8_lossy(&add_output.stderr).to_string(),
            ));
        }
        let id = parse_added_question_id(&String::from_utf8_lossy(&add_output.stdout))?;
        let rel_args = vec![
            "rel".to_string(),
            "add".to_string(),
            "--from".to_string(),
            origin.id.clone(),
            "--to".to_string(),
            id.clone(),
            "--type".to_string(),
            "begets".to_string(),
        ];
        let rel_output = self.runner.run(&self.command, &rel_args)?;
        if !rel_output.status.success() {
            return Err(QuizdomError::Aida(
                String::from_utf8_lossy(&rel_output.stderr).to_string(),
            ));
        }

        let mut persisted = question.clone();
        persisted.id = id;
        persisted.tags = tags;
        persisted.weight = 50;
        Ok(persisted)
    }
}

fn question_topic(question: &Question) -> String {
    question
        .tags
        .iter()
        .find_map(|tag| tag.strip_prefix("topic:"))
        .filter(|topic| !topic.trim().is_empty())
        .unwrap_or("generated")
        .to_string()
}

fn generated_question_tags(topic: &str, answer_kind: &AnswerKind) -> Vec<String> {
    vec![
        format!("topic:{topic}"),
        format!("answer:{}", answer_kind.mode()),
        "weight:50".to_string(),
        "seed".to_string(),
    ]
}

fn generated_question_description(question: &Question, origin: &Question) -> String {
    format!(
        "LLM-generated quizdom question.\n\nanswer: {}\norigin: {}\n\nGenerated from origin question: {}",
        question.answer_kind.mode(),
        origin.id,
        origin.title
    )
}

fn parse_added_question_id(output: &str) -> Result<String> {
    output
        .split(|character: char| character.is_whitespace() || character == ':')
        .find(|token| token.starts_with("Q-"))
        .map(str::to_string)
        .ok_or_else(|| QuizdomError::Parse("aida add output did not include Q id".to_string()))
}

#[derive(Debug, Clone)]
struct CliConfig {
    command: SessionCommand,
    seed: String,
    user_id: String,
    session_id: String,
    log_path: PathBuf,
    branch_id: String,
    proposition: Option<String>,
    agree_seed: Option<String>,
    disagree_seed: Option<String>,
    strategy: StrategyKind,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum StrategyKind {
    Deterministic,
    Llm,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SessionCommand {
    Start,
    Resume,
    Fork,
}

impl CliConfig {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut command = SessionCommand::Start;
        let mut seed = DEFAULT_SEED.to_string();
        let mut user_id = DEFAULT_USER.to_string();
        let mut session_id = format!("sess-{}", Utc::now().timestamp());
        let mut log_path = None;
        let mut branch_id = "main".to_string();
        let mut proposition = None;
        let mut agree_seed = None;
        let mut disagree_seed = None;
        let mut strategy = env_strategy();
        let mut args = args.into_iter().peekable();

        if matches!(args.peek().map(String::as_str), Some("session")) {
            args.next();
        }
        if matches!(args.peek().map(String::as_str), Some("start")) {
            args.next();
        } else if matches!(args.peek().map(String::as_str), Some("resume")) {
            command = SessionCommand::Resume;
            args.next();
        } else if matches!(args.peek().map(String::as_str), Some("fork")) {
            command = SessionCommand::Fork;
            args.next();
        }

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--seed" => seed = next_arg(&mut args, "--seed")?,
                "--user" => user_id = next_arg(&mut args, "--user")?,
                "--session" => session_id = next_arg(&mut args, "--session")?,
                "--log" => log_path = Some(PathBuf::from(next_arg(&mut args, "--log")?)),
                "--branch" => branch_id = next_arg(&mut args, "--branch")?,
                "--proposition" => proposition = Some(next_arg(&mut args, "--proposition")?),
                "--agree-seed" => agree_seed = Some(next_arg(&mut args, "--agree-seed")?),
                "--disagree-seed" => disagree_seed = Some(next_arg(&mut args, "--disagree-seed")?),
                "--strategy" => strategy = parse_strategy(&next_arg(&mut args, "--strategy")?)?,
                "--help" | "-h" => return Err(QuizdomError::Usage(usage())),
                other => {
                    return Err(QuizdomError::Usage(format!(
                        "unknown argument: {other}\n{}",
                        usage()
                    )))
                }
            }
        }

        let log_path = log_path.unwrap_or_else(|| {
            PathBuf::from("data")
                .join("users")
                .join(&user_id)
                .join("sessions")
                .join(format!("{session_id}.jsonl"))
        });

        Ok(Self {
            command,
            seed,
            user_id,
            session_id,
            log_path,
            branch_id,
            proposition,
            agree_seed,
            disagree_seed,
            strategy,
        })
    }
}

fn env_strategy() -> StrategyKind {
    std::env::var("QUIZDOM_STRATEGY")
        .ok()
        .and_then(|value| parse_strategy(&value).ok())
        .unwrap_or(StrategyKind::Deterministic)
}

fn parse_strategy(value: &str) -> Result<StrategyKind> {
    match value {
        "deterministic" => Ok(StrategyKind::Deterministic),
        "llm" => Ok(StrategyKind::Llm),
        other => Err(QuizdomError::Usage(format!(
            "unknown strategy: {other}; expected deterministic or llm"
        ))),
    }
}

fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| QuizdomError::Usage(format!("{name} requires a value")))
}

fn usage() -> String {
    "usage: quizdom [session] [start|resume|fork] [--seed Q-23] [--branch main] [--strategy deterministic|llm] [--user local-user] [--session sess-id] [--log path] [--proposition text --agree-seed Q --disagree-seed Q]"
        .to_string()
}

pub fn run_cli(
    args: impl IntoIterator<Item = String>,
    input: impl Read,
    mut output: impl Write,
) -> Result<()> {
    let config = CliConfig::parse(args)?;
    let bank = AidaCliQuestionBank::default();
    let deterministic = DeterministicNextQuestionStrategy;
    match config.command {
        SessionCommand::Start => match build_strategy(&config) {
            Some(strategy) => run_session(&config, &bank, strategy.as_ref(), input, &mut output),
            None => run_session(&config, &bank, &deterministic, input, &mut output),
        },
        SessionCommand::Resume => match build_strategy(&config) {
            Some(strategy) => resume_session(&config, &bank, strategy.as_ref(), input, &mut output),
            None => resume_session(&config, &bank, &deterministic, input, &mut output),
        },
        SessionCommand::Fork => fork_session(&config, &mut output),
    }
}

fn build_strategy(config: &CliConfig) -> Option<Box<dyn NextQuestionStrategy>> {
    match config.strategy {
        StrategyKind::Deterministic => None,
        StrategyKind::Llm => AnthropicClient::from_env().ok().map(|client| {
            Box::new(LlmNextQuestionStrategy::with_generated_question_persister(
                client,
                AidaCliGeneratedQuestionPersister::default(),
            )) as Box<dyn NextQuestionStrategy>
        }),
    }
}

fn run_session(
    config: &CliConfig,
    bank: &dyn QuestionBank,
    strategy: &dyn NextQuestionStrategy,
    input: impl Read,
    output: &mut impl Write,
) -> Result<()> {
    // trace:STORY-17 | ai:codex
    run_session_from_current(config, bank, strategy, input, output, 0, true, Vec::new())
}

fn run_session_from_current(
    config: &CliConfig,
    bank: &dyn QuestionBank,
    strategy: &dyn NextQuestionStrategy,
    input: impl Read,
    output: &mut impl Write,
    mut turn: u64,
    write_start_event: bool,
    mut recent_path: Vec<AnsweredQuestion>,
) -> Result<()> {
    let mut input = BufReader::new(input);
    let mut logger = SessionLogger::open(&config.log_path)?;
    let mut current = bank.load_question(&config.seed)?;

    if write_start_event {
        logger.session_started(
            &config.session_id,
            &config.user_id,
            &config.branch_id,
            &current.id,
        )?;
    }

    loop {
        logger.question_presented(
            &config.session_id,
            &config.user_id,
            &config.branch_id,
            turn,
            &current,
        )?;
        render_question(&current, output)?;
        let answer = match read_answer_or_end(&current.answer_kind, &mut input, output)? {
            AnswerInput::Answer(answer) => answer,
            AnswerInput::End => {
                writeln!(output, "Session ended.")?;
                logger.session_ended(
                    &config.session_id,
                    &config.user_id,
                    &config.branch_id,
                    turn,
                    "User ended session.",
                )?;
                break;
            }
        };
        logger.answer_recorded(
            &config.session_id,
            &config.user_id,
            &config.branch_id,
            turn,
            &current,
            &answer,
        )?;
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
                    turn,
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
                    turn,
                    "No outgoing begets successor.",
                )?;
                break;
            }
        }
    }

    Ok(())
}

fn resume_session(
    config: &CliConfig,
    bank: &dyn QuestionBank,
    strategy: &dyn NextQuestionStrategy,
    input: impl Read,
    output: &mut impl Write,
) -> Result<()> {
    // trace:STORY-20 | ai:codex
    let replay = SessionReplay::load(&config.log_path, &config.branch_id)?;
    replay.render(output)?;

    let Some(next_question_ref) = replay.next_question_ref.as_ref() else {
        writeln!(output, "No saved follow-up question. Session complete.")?;
        return Ok(());
    };

    let mut resumed_config = config.clone();
    resumed_config.seed = next_question_ref.clone();
    let recent_path = replay.recent_path();
    run_session_from_current(
        &resumed_config,
        bank,
        strategy,
        input,
        output,
        replay.next_turn,
        false,
        recent_path,
    )
}

fn fork_session(config: &CliConfig, output: &mut impl Write) -> Result<()> {
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

fn render_question(question: &Question, output: &mut impl Write) -> Result<()> {
    writeln!(output, "\n{}", question.title)?;
    match &question.answer_kind {
        AnswerKind::YesNo => writeln!(output, "Answer yes or no, or /end to end this session.")?,
        AnswerKind::Choice(options) => {
            for (index, option) in options.iter().enumerate() {
                writeln!(output, "{}. {}", index + 1, option)?;
            }
            writeln!(output, "Enter a choice, or /end to end this session.")?;
        }
        AnswerKind::FreeText => writeln!(
            output,
            "Answer in your own words, or /end to end this session."
        )?,
    }
    write!(output, "> ")?;
    output.flush()?;
    Ok(())
}

enum AnswerInput {
    Answer(Answer),
    End,
}

fn read_answer_or_end(
    kind: &AnswerKind,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<AnswerInput> {
    loop {
        let mut raw = String::new();
        if input.read_line(&mut raw)? == 0 {
            return Err(QuizdomError::Parse("no answer provided".to_string()));
        }
        let raw = raw.trim().to_string();
        if raw == "/end" {
            return Ok(AnswerInput::End);
        }
        if let Some(normalized) = normalize_answer(kind, &raw) {
            return Ok(AnswerInput::Answer(Answer { raw, normalized }));
        }
        write!(output, "Please enter a valid answer or /end: ")?;
        output.flush()?;
    }
}

fn normalize_answer(kind: &AnswerKind, raw: &str) -> Option<String> {
    match kind {
        AnswerKind::YesNo => match raw.trim().to_ascii_lowercase().as_str() {
            "yes" | "y" => Some("yes".to_string()),
            "no" | "n" => Some("no".to_string()),
            _ => None,
        },
        AnswerKind::Choice(options) => {
            let trimmed = raw.trim();
            if let Ok(index) = trimmed.parse::<usize>() {
                return options.get(index.checked_sub(1)?).cloned();
            }
            options
                .iter()
                .find(|option| option.eq_ignore_ascii_case(trimmed))
                .cloned()
        }
        AnswerKind::FreeText => (!raw.trim().is_empty()).then(|| raw.trim().to_string()),
    }
}

struct SessionLogger {
    file: fs::File,
    next_event: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct ReplayedAnswer {
    turn: u64,
    question_ref: String,
    question_text: String,
    raw_answer: String,
    normalized_answer: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct SessionReplay {
    branch_id: String,
    answers: Vec<ReplayedAnswer>,
    next_question_ref: Option<String>,
    next_turn: u64,
}

impl SessionReplay {
    fn load(path: &Path, branch_id: &str) -> Result<Self> {
        let file = File::open(path)?;
        Self::from_reader(file, branch_id)
    }

    fn from_reader(reader: impl Read, branch_id: &str) -> Result<Self> {
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

    fn render(&self, output: &mut impl Write) -> Result<()> {
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
    ) -> Result<()> {
        let event_id = self.event_id();
        self.write(json!({
            "event_id": event_id,
            "event_type": "session_started",
            "occurred_at": Utc::now().to_rfc3339(),
            "session_id": session_id,
            "user_id": user_id,
            "branch_id": branch_id,
            "seed_question_ref": seed_question_ref,
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

pub fn parse_question_show(output: &str) -> Result<Question> {
    let id = prefixed_line(output, "ID:")
        .ok_or_else(|| QuizdomError::Parse("aida show output missing ID".to_string()))?;
    let title = prefixed_line(output, "Title:")
        .ok_or_else(|| QuizdomError::Parse("aida show output missing Title".to_string()))?;
    let tags = split_tags(&prefixed_line(output, "Tags:").unwrap_or_default());
    let answer_kind = answer_kind_from_tags(&tags)
        .ok_or_else(|| QuizdomError::Parse(format!("{id} missing answer:* tag")))?;
    let weight = tags
        .iter()
        .find_map(|tag| tag.strip_prefix("weight:")?.parse::<u32>().ok())
        .unwrap_or(0);

    Ok(Question {
        id,
        title,
        tags,
        answer_kind,
        weight,
    })
}

fn answer_kind_from_tags(tags: &[String]) -> Option<AnswerKind> {
    tags.iter().find_map(|tag| {
        if tag == "answer:yes-no" {
            Some(AnswerKind::YesNo)
        } else if tag == "answer:free-text" {
            Some(AnswerKind::FreeText)
        } else {
            tag.strip_prefix("answer:choice[").and_then(|rest| {
                let options = rest.strip_suffix(']')?;
                let options = options
                    .split([',', '|'])
                    .map(str::trim)
                    .filter(|option| !option.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                (!options.is_empty()).then_some(AnswerKind::Choice(options))
            })
        }
    })
}

fn split_tags(line: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let mut current = String::new();
    let mut bracket_depth = 0_u32;

    for character in line.chars() {
        match character {
            '[' => {
                bracket_depth += 1;
                current.push(character);
            }
            ']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                current.push(character);
            }
            ',' if bracket_depth == 0 => {
                let tag = current.trim();
                if !tag.is_empty() {
                    tags.push(tag.to_string());
                }
                current.clear();
            }
            _ => current.push(character),
        }
    }

    let tag = current.trim();
    if !tag.is_empty() {
        tags.push(tag.to_string());
    }

    tags
}

fn prefixed_line(output: &str, prefix: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix(prefix).map(str::trim))
        .map(str::to_string)
}

pub fn parse_begets_rel_list(output: &str) -> Vec<QuestionRef> {
    output
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty()
                || trimmed.starts_with("FROM")
                || trimmed.starts_with("(no outgoing")
                || trimmed.ends_with("edges")
            {
                return None;
            }
            let mut columns = trimmed.split_whitespace();
            let _from = columns.next()?;
            let relationship_type = columns.next()?;
            let to = columns.next()?;
            (relationship_type == "begets").then(|| QuestionRef { id: to.to_string() })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::{LLMError, LLMFuture, ToolDef};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};
    use std::rc::Rc;

    #[test]
    fn parses_question_answer_kind_and_weight_from_aida_show() {
        let output = r#"ID: Q-99
Title: Pick a definition
Tags: topic:free-will, answer:choice[libertarian, compatibilist], weight:42
"#;

        let question = parse_question_show(output).unwrap();

        assert_eq!(question.id, "Q-99");
        assert_eq!(question.title, "Pick a definition");
        assert_eq!(
            question.answer_kind,
            AnswerKind::Choice(vec!["libertarian".to_string(), "compatibilist".to_string()])
        );
        assert_eq!(question.weight, 42);
    }

    #[test]
    fn parses_begets_relationships_from_aida_rel_list() {
        let output = r#"FROM  TYPE    TO    TITLE
  Q-23  begets  Q-26  Do you mean the ability…
  Q-23  begets  Q-27  Can a choice be free?

2 edges
"#;

        let refs = parse_begets_rel_list(output);

        assert_eq!(
            refs,
            vec![
                QuestionRef {
                    id: "Q-26".to_string()
                },
                QuestionRef {
                    id: "Q-27".to_string()
                }
            ]
        );
    }

    #[test]
    fn deterministic_strategy_uses_highest_weight_then_lowest_id() {
        let bank = FakeBank::new([
            question("Q-1", 0, AnswerKind::YesNo),
            question("Q-3", 80, AnswerKind::YesNo),
            question("Q-2", 80, AnswerKind::YesNo),
        ])
        .with_edges("Q-1", ["Q-3", "Q-2"]);

        let next = DeterministicNextQuestionStrategy
            .next_question(
                &bank.load_question("Q-1").unwrap(),
                &strategy_context("yes"),
                &bank,
            )
            .unwrap()
            .unwrap();

        assert_eq!(next.id, "Q-2");
    }

    #[test]
    fn llm_strategy_selects_existing_candidate_from_model_json() {
        let bank = FakeBank::new([
            question("Q-1", 0, AnswerKind::YesNo),
            question("Q-2", 40, AnswerKind::FreeText),
            question("Q-3", 10, AnswerKind::YesNo),
        ])
        .with_edges("Q-1", ["Q-2", "Q-3"]);
        let strategy =
            LlmNextQuestionStrategy::new(MockLlm::ok(r#"{"action":"select","id":"Q-3"}"#));

        let next = strategy
            .next_question(
                &bank.load_question("Q-1").unwrap(),
                &strategy_context("because it matters"),
                &bank,
            )
            .unwrap()
            .unwrap();

        assert_eq!(next.id, "Q-3");
    }

    #[test]
    fn llm_strategy_returns_generated_question_in_memory() {
        let bank = FakeBank::new([question("Q-1", 0, AnswerKind::YesNo)]);
        let strategy = LlmNextQuestionStrategy::new(MockLlm::ok(
            r#"{"action":"generate","question":"What do you mean by responsibility?","answer_mode":"free-text"}"#,
        ));

        let next = strategy
            .next_question(
                &bank.load_question("Q-1").unwrap(),
                &strategy_context("yes"),
                &bank,
            )
            .unwrap()
            .unwrap();

        assert_eq!(next.id, "generated:llm");
        assert_eq!(next.title, "What do you mean by responsibility?");
        assert_eq!(next.answer_kind, AnswerKind::FreeText);
    }

    #[test]
    fn llm_strategy_persists_generated_question_when_configured() {
        let bank = FakeBank::new([question_with_tags(
            "Q-1",
            0,
            AnswerKind::YesNo,
            ["topic:free-will", "answer:yes-no", "weight:70"],
        )]);
        let runner = RecordingCommandRunner::new([
            command_output(true, "Added: Q-42\n", ""),
            command_output(true, "relationship added\n", ""),
        ]);
        let strategy = LlmNextQuestionStrategy::with_generated_question_persister(
            MockLlm::ok(
                r#"{"action":"generate","question":"What definition of responsibility are you using?","answer_mode":"free-text"}"#,
            ),
            AidaCliGeneratedQuestionPersister::new("aida", runner.clone()),
        );

        let next = strategy
            .next_question(
                &bank.load_question("Q-1").unwrap(),
                &strategy_context("yes"),
                &bank,
            )
            .unwrap()
            .unwrap();

        assert_eq!(next.id, "Q-42");
        assert_eq!(
            next.tags,
            vec![
                "topic:free-will".to_string(),
                "answer:free-text".to_string(),
                "weight:50".to_string(),
                "seed".to_string()
            ]
        );
        assert_eq!(
            runner.calls(),
            vec![
                strings([
                    "aida",
                    "add",
                    "--prefix",
                    "Q",
                    "--type",
                    "functional",
                    "--status",
                    "approved",
                    "--priority",
                    "medium",
                    "--title",
                    "What definition of responsibility are you using?",
                    "--description",
                    "LLM-generated quizdom question.\n\nanswer: free-text\norigin: Q-1\n\nGenerated from origin question: Q-1",
                    "--tags",
                    "topic:free-will,answer:free-text,weight:50,seed",
                ]),
                strings([
                    "aida", "rel", "add", "--from", "Q-1", "--to", "Q-42", "--type", "begets",
                ]),
            ]
        );
    }

    #[test]
    fn llm_strategy_prefers_near_identical_existing_candidate_over_duplicate() {
        let bank = FakeBank::new([
            question("Q-1", 0, AnswerKind::YesNo),
            Question {
                id: "Q-2".to_string(),
                title: "What definition of responsibility are you using?".to_string(),
                tags: vec!["topic:free-will".to_string(), "weight:50".to_string()],
                answer_kind: AnswerKind::FreeText,
                weight: 50,
            },
        ])
        .with_edges("Q-1", ["Q-2"]);
        let runner = RecordingCommandRunner::new([]);
        let strategy = LlmNextQuestionStrategy::with_generated_question_persister(
            MockLlm::ok(
                r#"{"action":"generate","question":"  What definition of responsibility are you using?  ","answer_mode":"free-text"}"#,
            ),
            AidaCliGeneratedQuestionPersister::new("aida", runner.clone()),
        );

        let next = strategy
            .next_question(
                &bank.load_question("Q-1").unwrap(),
                &strategy_context("yes"),
                &bank,
            )
            .unwrap()
            .unwrap();

        assert_eq!(next.id, "Q-2");
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn llm_strategy_falls_back_to_deterministic_on_model_error() {
        let bank = FakeBank::new([
            question("Q-1", 0, AnswerKind::YesNo),
            question("Q-3", 80, AnswerKind::YesNo),
            question("Q-2", 80, AnswerKind::YesNo),
        ])
        .with_edges("Q-1", ["Q-3", "Q-2"]);
        let strategy = LlmNextQuestionStrategy::new(MockLlm::err(LLMError::Provider(
            "provider unavailable".to_string(),
        )));

        let next = strategy
            .next_question(
                &bank.load_question("Q-1").unwrap(),
                &strategy_context("yes"),
                &bank,
            )
            .unwrap()
            .unwrap();

        assert_eq!(next.id, "Q-2");
    }

    #[test]
    #[ignore = "requires ANTHROPIC_API_KEY and makes a live provider call"]
    fn live_llm_strategy_smoke() {
        if std::env::var("ANTHROPIC_API_KEY").is_err() {
            return;
        }
        let bank = FakeBank::new([
            question("Q-1", 0, AnswerKind::YesNo),
            question("Q-2", 10, AnswerKind::FreeText),
        ])
        .with_edges("Q-1", ["Q-2"]);
        let strategy = LlmNextQuestionStrategy::new(AnthropicClient::from_env().unwrap());

        let next = strategy
            .next_question(
                &bank.load_question("Q-1").unwrap(),
                &strategy_context("yes"),
                &bank,
            )
            .unwrap();

        assert!(next.is_some());
    }

    #[test]
    fn accepts_all_answer_kinds() {
        assert_eq!(
            normalize_answer(&AnswerKind::YesNo, "Y"),
            Some("yes".to_string())
        );
        assert_eq!(
            normalize_answer(
                &AnswerKind::Choice(vec!["one".to_string(), "two".to_string()]),
                "2"
            ),
            Some("two".to_string())
        );
        assert_eq!(
            normalize_answer(&AnswerKind::FreeText, "  because  "),
            Some("because".to_string())
        );
    }

    #[test]
    fn renders_all_question_kinds() {
        let cases = [
            (AnswerKind::YesNo, "Answer yes or no, or /end"),
            (
                AnswerKind::Choice(vec!["libertarian".to_string(), "compatibilist".to_string()]),
                "2. compatibilist",
            ),
            (AnswerKind::FreeText, "Answer in your own words, or /end"),
        ];

        for (answer_kind, expected) in cases {
            let mut output = Vec::new();
            render_question(&question("Q-test", 0, answer_kind), &mut output).unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains(expected), "{output}");
        }
    }

    #[test]
    fn resume_replays_exact_answered_path_and_continues_from_saved_next_question() {
        let log = [
            r#"{"event_id":"evt-000001","event_type":"session_started","occurred_at":"2026-05-31T17:00:00Z","session_id":"sess-test","user_id":"user","seed_question_ref":"Q-1"}"#,
            r#"{"event_id":"evt-000002","event_type":"question_presented","occurred_at":"2026-05-31T17:00:01Z","session_id":"sess-test","user_id":"user","turn":0,"question_ref":"Q-1","question_text":"First question?","answer_mode":"yes-no"}"#,
            r#"{"event_id":"evt-000003","event_type":"answer_recorded","occurred_at":"2026-05-31T17:00:02Z","session_id":"sess-test","user_id":"user","turn":0,"question_ref":"Q-1","answer_mode":"yes-no","raw_answer":"yes","normalized_answer":"yes"}"#,
            r#"{"event_id":"evt-000004","event_type":"next_question_selected","occurred_at":"2026-05-31T17:00:03Z","session_id":"sess-test","user_id":"user","turn":0,"question_ref":"Q-1","selected_next_question_ref":"Q-2","selection_reason":"test"}"#,
            r#"{"event_id":"evt-000005","event_type":"session_ended","occurred_at":"2026-05-31T17:00:04Z","session_id":"sess-test","user_id":"user","turn":0,"summary":"User ended session."}"#,
        ]
        .join("\n");
        let replay = SessionReplay::from_reader(log.as_bytes(), "main").unwrap();
        let mut output = Vec::new();

        replay.render(&mut output).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("First question?"));
        assert!(output.contains("answer: yes"));
        assert_eq!(replay.next_question_ref, Some("Q-2".to_string()));
        assert_eq!(replay.next_turn, 1);
    }

    #[test]
    fn answered_saved_next_question_clears_resume_target() {
        let log = [
            r#"{"event_id":"evt-000001","event_type":"question_presented","occurred_at":"2026-05-31T17:00:01Z","session_id":"sess-test","user_id":"user","turn":0,"question_ref":"Q-1","question_text":"First question?","answer_mode":"yes-no"}"#,
            r#"{"event_id":"evt-000002","event_type":"answer_recorded","occurred_at":"2026-05-31T17:00:02Z","session_id":"sess-test","user_id":"user","turn":0,"question_ref":"Q-1","answer_mode":"yes-no","raw_answer":"yes","normalized_answer":"yes"}"#,
            r#"{"event_id":"evt-000003","event_type":"next_question_selected","occurred_at":"2026-05-31T17:00:03Z","session_id":"sess-test","user_id":"user","turn":0,"question_ref":"Q-1","selected_next_question_ref":"Q-2","selection_reason":"test"}"#,
            r#"{"event_id":"evt-000004","event_type":"question_presented","occurred_at":"2026-05-31T17:00:04Z","session_id":"sess-test","user_id":"user","turn":1,"question_ref":"Q-2","question_text":"Second question?","answer_mode":"yes-no"}"#,
            r#"{"event_id":"evt-000005","event_type":"answer_recorded","occurred_at":"2026-05-31T17:00:05Z","session_id":"sess-test","user_id":"user","turn":1,"question_ref":"Q-2","answer_mode":"yes-no","raw_answer":"no","normalized_answer":"no"}"#,
        ]
        .join("\n");

        let replay = SessionReplay::from_reader(log.as_bytes(), "main").unwrap();

        assert_eq!(replay.next_question_ref, None);
        assert_eq!(replay.next_turn, 2);
    }

    #[test]
    fn start_end_resume_round_trip_replays_path_and_finishes() {
        let path = std::env::temp_dir().join(format!(
            "quizdom-story-20-test-{}.jsonl",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let bank = FakeBank::new([
            question("Q-1", 10, AnswerKind::YesNo),
            question("Q-2", 5, AnswerKind::YesNo),
        ])
        .with_edges("Q-1", ["Q-2"]);
        let strategy = DeterministicNextQuestionStrategy;
        let config = CliConfig {
            command: SessionCommand::Start,
            seed: "Q-1".to_string(),
            user_id: "test-user".to_string(),
            session_id: "sess-test".to_string(),
            log_path: path.clone(),
            branch_id: "main".to_string(),
            proposition: None,
            agree_seed: None,
            disagree_seed: None,
            strategy: StrategyKind::Deterministic,
        };
        let mut start_output = Vec::new();

        run_session(
            &config,
            &bank,
            &strategy,
            "yes\n/end\n".as_bytes(),
            &mut start_output,
        )
        .unwrap();

        let mut resume_output = Vec::new();
        let mut resume_config = config.clone();
        resume_config.command = SessionCommand::Resume;
        resume_session(
            &resume_config,
            &bank,
            &strategy,
            "no\n".as_bytes(),
            &mut resume_output,
        )
        .unwrap();

        let resume_output = String::from_utf8(resume_output).unwrap();
        assert!(resume_output.contains("Replaying previous session path for branch 'main':"));
        assert!(resume_output.contains("[turn 0] Q-1"));
        assert!(resume_output.contains("answer: yes"));
        assert!(resume_output.contains("Q-2"));

        let log = fs::read_to_string(&path).unwrap();
        assert!(log.contains(r#""question_ref":"Q-1""#));
        assert!(log.contains(r#""question_ref":"Q-2""#));
        assert!(log.contains(r#""normalized_answer":"no""#));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn forked_agree_and_disagree_branches_are_recoverable_independently() {
        let path = std::env::temp_dir().join(format!(
            "quizdom-story-19-test-{}.jsonl",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let bank = FakeBank::new([
            question("Q-agree", 10, AnswerKind::YesNo),
            question("Q-disagree", 10, AnswerKind::YesNo),
        ]);
        let strategy = DeterministicNextQuestionStrategy;
        let fork_config = CliConfig {
            command: SessionCommand::Fork,
            seed: "Q-1".to_string(),
            user_id: "test-user".to_string(),
            session_id: "sess-test".to_string(),
            log_path: path.clone(),
            branch_id: "main".to_string(),
            proposition: Some("Free will requires alternatives".to_string()),
            agree_seed: Some("Q-agree".to_string()),
            disagree_seed: Some("Q-disagree".to_string()),
            strategy: StrategyKind::Deterministic,
        };
        let mut fork_output = Vec::new();
        fork_session(&fork_config, &mut fork_output).unwrap();

        let mut agree_config = fork_config.clone();
        agree_config.command = SessionCommand::Resume;
        agree_config.branch_id = "agree".to_string();
        let mut agree_output = Vec::new();
        resume_session(
            &agree_config,
            &bank,
            &strategy,
            "yes\n".as_bytes(),
            &mut agree_output,
        )
        .unwrap();

        let mut disagree_config = fork_config.clone();
        disagree_config.command = SessionCommand::Resume;
        disagree_config.branch_id = "disagree".to_string();
        let mut disagree_output = Vec::new();
        resume_session(
            &disagree_config,
            &bank,
            &strategy,
            "no\n".as_bytes(),
            &mut disagree_output,
        )
        .unwrap();

        let agree_output = String::from_utf8(agree_output).unwrap();
        let disagree_output = String::from_utf8(disagree_output).unwrap();
        assert!(agree_output.contains("branch 'agree'"));
        assert!(agree_output.contains("Q-agree"));
        assert!(disagree_output.contains("branch 'disagree'"));
        assert!(disagree_output.contains("Q-disagree"));

        let agree_replay = SessionReplay::load(&path, "agree").unwrap();
        let disagree_replay = SessionReplay::load(&path, "disagree").unwrap();
        assert_eq!(agree_replay.answers[0].question_ref, "Q-agree");
        assert_eq!(agree_replay.answers[0].normalized_answer, "yes");
        assert_eq!(disagree_replay.answers[0].question_ref, "Q-disagree");
        assert_eq!(disagree_replay.answers[0].normalized_answer, "no");

        let _ = fs::remove_file(path);
    }

    fn question(id: &str, weight: u32, answer_kind: AnswerKind) -> Question {
        question_with_tags(id, weight, answer_kind, [format!("weight:{weight}")])
    }

    fn question_with_tags(
        id: &str,
        weight: u32,
        answer_kind: AnswerKind,
        tags: impl IntoIterator<Item = impl Into<String>>,
    ) -> Question {
        Question {
            id: id.to_string(),
            title: id.to_string(),
            tags: tags.into_iter().map(Into::into).collect(),
            answer_kind,
            weight,
        }
    }

    fn strategy_context(raw: &str) -> StrategyContext {
        StrategyContext {
            answer: Answer {
                raw: raw.to_string(),
                normalized: raw.to_string(),
            },
            recent_path: Vec::new(),
        }
    }

    #[derive(Clone)]
    struct MockLlm {
        result: std::result::Result<(String, Vec<llm::ToolCall>), LLMError>,
    }

    impl MockLlm {
        fn ok(text: &str) -> Self {
            Self {
                result: Ok((text.to_string(), Vec::new())),
            }
        }

        fn err(error: LLMError) -> Self {
            Self { result: Err(error) }
        }
    }

    impl LLMClient for MockLlm {
        fn call<'a>(
            &'a self,
            _system: &'a str,
            _messages: &'a [Message],
            _tools: &'a [ToolDef],
        ) -> LLMFuture<'a> {
            Box::pin(std::future::ready(self.result.clone()))
        }
    }

    #[derive(Clone)]
    struct RecordingCommandRunner {
        calls: Rc<RefCell<Vec<Vec<String>>>>,
        outputs: Rc<RefCell<Vec<Output>>>,
    }

    impl RecordingCommandRunner {
        fn new(outputs: impl IntoIterator<Item = Output>) -> Self {
            Self {
                calls: Rc::new(RefCell::new(Vec::new())),
                outputs: Rc::new(RefCell::new(outputs.into_iter().collect())),
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.borrow().clone()
        }
    }

    impl CommandRunner for RecordingCommandRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<Output> {
            let mut call = vec![program.to_string()];
            call.extend(args.iter().cloned());
            self.calls.borrow_mut().push(call);
            if self.outputs.borrow().is_empty() {
                return Err(QuizdomError::Aida("unexpected command".to_string()));
            }
            Ok(self.outputs.borrow_mut().remove(0))
        }
    }

    fn command_output(success: bool, stdout: &str, stderr: &str) -> Output {
        Output {
            status: if success {
                ExitStatus::from_raw(0)
            } else {
                ExitStatus::from_raw(1)
            },
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn strings(items: impl IntoIterator<Item = &'static str>) -> Vec<String> {
        items.into_iter().map(str::to_string).collect()
    }

    struct FakeBank {
        questions: HashMap<String, Question>,
        edges: HashMap<String, Vec<QuestionRef>>,
    }

    impl FakeBank {
        fn new(questions: impl IntoIterator<Item = Question>) -> Self {
            Self {
                questions: questions
                    .into_iter()
                    .map(|question| (question.id.clone(), question))
                    .collect(),
                edges: HashMap::new(),
            }
        }

        fn with_edges(mut self, from: &str, to: impl IntoIterator<Item = &'static str>) -> Self {
            self.edges.insert(
                from.to_string(),
                to.into_iter()
                    .map(|id| QuestionRef { id: id.to_string() })
                    .collect(),
            );
            self
        }
    }

    impl QuestionBank for FakeBank {
        fn load_question(&self, id: &str) -> Result<Question> {
            self.questions
                .get(id)
                .cloned()
                .ok_or_else(|| QuizdomError::Parse(format!("missing {id}")))
        }

        fn begets(&self, id: &str) -> Result<Vec<QuestionRef>> {
            Ok(self.edges.get(id).cloned().unwrap_or_default())
        }
    }
}
