use chrono::Utc;
use serde_json::json;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_SEED: &str = "Q-23";
const DEFAULT_USER: &str = "local-user";

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

pub trait NextQuestionStrategy {
    fn next_question(
        &self,
        current: &Question,
        bank: &dyn QuestionBank,
    ) -> Result<Option<Question>>;
}

pub struct DeterministicNextQuestionStrategy;

impl NextQuestionStrategy for DeterministicNextQuestionStrategy {
    fn next_question(
        &self,
        current: &Question,
        bank: &dyn QuestionBank,
    ) -> Result<Option<Question>> {
        let mut successors = bank
            .begets(&current.id)?
            .into_iter()
            .map(|question_ref| bank.load_question(&question_ref.id))
            .collect::<Result<Vec<_>>>()?;

        successors.sort_by(|left, right| {
            right
                .weight
                .cmp(&left.weight)
                .then_with(|| left.id.cmp(&right.id))
        });

        Ok(successors.into_iter().next())
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
        })
    }
}

fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| QuizdomError::Usage(format!("{name} requires a value")))
}

fn usage() -> String {
    "usage: quizdom [session] [start|resume|fork] [--seed Q-23] [--branch main] [--user local-user] [--session sess-id] [--log path] [--proposition text --agree-seed Q --disagree-seed Q]"
        .to_string()
}

pub fn run_cli(
    args: impl IntoIterator<Item = String>,
    input: impl Read,
    mut output: impl Write,
) -> Result<()> {
    let config = CliConfig::parse(args)?;
    let bank = AidaCliQuestionBank::default();
    let strategy = DeterministicNextQuestionStrategy;
    match config.command {
        SessionCommand::Start => run_session(&config, &bank, &strategy, input, &mut output),
        SessionCommand::Resume => resume_session(&config, &bank, &strategy, input, &mut output),
        SessionCommand::Fork => fork_session(&config, &mut output),
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
    run_session_from_current(config, bank, strategy, input, output, 0, true)
}

fn run_session_from_current(
    config: &CliConfig,
    bank: &dyn QuestionBank,
    strategy: &dyn NextQuestionStrategy,
    input: impl Read,
    output: &mut impl Write,
    mut turn: u64,
    write_start_event: bool,
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

        match strategy.next_question(&current, bank)? {
            Some(next) => {
                logger.next_question_selected(
                    &config.session_id,
                    &config.user_id,
                    &config.branch_id,
                    turn,
                    &current.id,
                    &next.id,
                    "Deterministic begets traversal: highest weight, then id.",
                )?;
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

    let Some(next_question_ref) = replay.next_question_ref else {
        writeln!(output, "No saved follow-up question. Session complete.")?;
        return Ok(());
    };

    let mut resumed_config = config.clone();
    resumed_config.seed = next_question_ref;
    run_session_from_current(
        &resumed_config,
        bank,
        strategy,
        input,
        output,
        replay.next_turn,
        false,
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
    use std::collections::HashMap;

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
            .next_question(&bank.load_question("Q-1").unwrap(), &bank)
            .unwrap()
            .unwrap();

        assert_eq!(next.id, "Q-2");
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
        Question {
            id: id.to_string(),
            title: id.to_string(),
            tags: vec![format!("weight:{weight}")],
            answer_kind,
            weight,
        }
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
