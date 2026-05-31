use chrono::Utc;
use serde_json::json;
use std::fmt;
use std::fs::{self, OpenOptions};
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
    seed: String,
    user_id: String,
    session_id: String,
    log_path: PathBuf,
}

impl CliConfig {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut seed = DEFAULT_SEED.to_string();
        let mut user_id = DEFAULT_USER.to_string();
        let mut session_id = format!("sess-{}", Utc::now().timestamp());
        let mut log_path = None;
        let mut args = args.into_iter().peekable();

        if matches!(args.peek().map(String::as_str), Some("session")) {
            args.next();
        }

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--seed" => seed = next_arg(&mut args, "--seed")?,
                "--user" => user_id = next_arg(&mut args, "--user")?,
                "--session" => session_id = next_arg(&mut args, "--session")?,
                "--log" => log_path = Some(PathBuf::from(next_arg(&mut args, "--log")?)),
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
            seed,
            user_id,
            session_id,
            log_path,
        })
    }
}

fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| QuizdomError::Usage(format!("{name} requires a value")))
}

fn usage() -> String {
    "usage: quizdom [session] [--seed Q-23] [--user local-user] [--session sess-id] [--log path]"
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
    run_session(&config, &bank, &strategy, input, &mut output)
}

fn run_session(
    config: &CliConfig,
    bank: &dyn QuestionBank,
    strategy: &dyn NextQuestionStrategy,
    input: impl Read,
    output: &mut impl Write,
) -> Result<()> {
    // trace:STORY-17 | ai:codex
    let mut input = BufReader::new(input);
    let mut logger = SessionLogger::open(&config.log_path)?;
    let mut current = bank.load_question(&config.seed)?;
    let mut turn = 0_u64;

    logger.session_started(&config.session_id, &config.user_id, &current.id)?;

    loop {
        logger.question_presented(&config.session_id, &config.user_id, turn, &current)?;
        render_question(&current, output)?;
        let answer = read_valid_answer(&current.answer_kind, &mut input, output)?;
        logger.answer_recorded(&config.session_id, &config.user_id, turn, &current, &answer)?;

        match strategy.next_question(&current, bank)? {
            Some(next) => {
                logger.next_question_selected(
                    &config.session_id,
                    &config.user_id,
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
                    turn,
                    "No outgoing begets successor.",
                )?;
                break;
            }
        }
    }

    Ok(())
}

fn render_question(question: &Question, output: &mut impl Write) -> Result<()> {
    writeln!(output, "\n{}", question.title)?;
    match &question.answer_kind {
        AnswerKind::YesNo => writeln!(output, "Answer yes or no.")?,
        AnswerKind::Choice(options) => {
            for (index, option) in options.iter().enumerate() {
                writeln!(output, "{}. {}", index + 1, option)?;
            }
        }
        AnswerKind::FreeText => writeln!(output, "Answer in your own words.")?,
    }
    write!(output, "> ")?;
    output.flush()?;
    Ok(())
}

fn read_valid_answer(
    kind: &AnswerKind,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<Answer> {
    loop {
        let mut raw = String::new();
        if input.read_line(&mut raw)? == 0 {
            return Err(QuizdomError::Parse("no answer provided".to_string()));
        }
        let raw = raw.trim().to_string();
        if let Some(normalized) = normalize_answer(kind, &raw) {
            return Ok(Answer { raw, normalized });
        }
        write!(output, "Please enter a valid answer: ")?;
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

impl SessionLogger {
    fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file,
            next_event: 1,
        })
    }

    fn session_started(
        &mut self,
        session_id: &str,
        user_id: &str,
        seed_question_ref: &str,
    ) -> Result<()> {
        let event_id = self.event_id();
        self.write(json!({
            "event_id": event_id,
            "event_type": "session_started",
            "occurred_at": Utc::now().to_rfc3339(),
            "session_id": session_id,
            "user_id": user_id,
            "seed_question_ref": seed_question_ref,
        }))
    }

    fn question_presented(
        &mut self,
        session_id: &str,
        user_id: &str,
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
            "turn": turn,
            "summary": summary,
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
            (AnswerKind::YesNo, "Answer yes or no."),
            (
                AnswerKind::Choice(vec!["libertarian".to_string(), "compatibilist".to_string()]),
                "2. compatibilist",
            ),
            (AnswerKind::FreeText, "Answer in your own words."),
        ];

        for (answer_kind, expected) in cases {
            let mut output = Vec::new();
            render_question(&question("Q-test", 0, answer_kind), &mut output).unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains(expected), "{output}");
        }
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
