use crate::error::{QuizdomError, Result};
use crate::model::{AnswerKind, Question, TermDefinition};
use std::process::{Command, Output};

pub trait GeneratedQuestionPersister {
    fn persist_generated_question(
        &self,
        origin: &Question,
        question: &Question,
    ) -> Result<Question>;
}

pub(crate) trait UserSpecificTermPersister {
    fn persist_user_specific_term(
        &self,
        term_label: &str,
        meaning: &str,
        definitions: &[TermDefinition],
    ) -> Result<TermDefinition>;
}

#[cfg(test)]
pub(crate) struct NoopUserSpecificTermPersister;

#[cfg(test)]
impl UserSpecificTermPersister for NoopUserSpecificTermPersister {
    fn persist_user_specific_term(
        &self,
        _term_label: &str,
        _meaning: &str,
        _definitions: &[TermDefinition],
    ) -> Result<TermDefinition> {
        Err(QuizdomError::Aida(
            "user-specific term persistence is unavailable".to_string(),
        ))
    }
}

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

pub(crate) trait CommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<Output>;
}

pub(crate) struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<Output> {
        Command::new(program)
            .args(args)
            .output()
            .map_err(Into::into)
    }
}

pub(crate) struct AidaCliGeneratedQuestionPersister<R = SystemCommandRunner> {
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

pub(crate) struct AidaCliUserSpecificTermPersister<R = SystemCommandRunner> {
    command: String,
    runner: R,
}

impl Default for AidaCliUserSpecificTermPersister<SystemCommandRunner> {
    fn default() -> Self {
        Self {
            command: "aida".to_string(),
            runner: SystemCommandRunner,
        }
    }
}

impl<R> AidaCliUserSpecificTermPersister<R>
where
    R: CommandRunner,
{
    #[cfg(test)]
    pub(crate) fn new(command: impl Into<String>, runner: R) -> Self {
        Self {
            command: command.into(),
            runner,
        }
    }
}

impl<R> UserSpecificTermPersister for AidaCliUserSpecificTermPersister<R>
where
    R: CommandRunner,
{
    fn persist_user_specific_term(
        &self,
        term_label: &str,
        meaning: &str,
        definitions: &[TermDefinition],
    ) -> Result<TermDefinition> {
        // trace:STORY-43 | ai:codex
        let topic = definitions
            .iter()
            .find_map(|definition| {
                definition
                    .tags
                    .iter()
                    .find_map(|tag| tag.strip_prefix("topic:"))
            })
            .unwrap_or("user-specific");
        let title = format!("{term_label} / user-specific");
        let tags = vec![
            format!("topic:{topic}"),
            "definition:user-specific".to_string(),
            "weight:40".to_string(),
        ];
        let description = format!(
            "source: user-specific quizdom steering fallback.\n\ndefinition: {meaning}\n\nscope: user-specific definition captured only after shared bank definitions did not fit."
        );
        let args = vec![
            "add".to_string(),
            "--type".to_string(),
            "term".to_string(),
            "--status".to_string(),
            "approved".to_string(),
            "--priority".to_string(),
            "medium".to_string(),
            "--title".to_string(),
            title.clone(),
            "--description".to_string(),
            description.clone(),
            "--tags".to_string(),
            tags.join(","),
        ];
        let output = self.runner.run(&self.command, &args)?;
        if !output.status.success() {
            return Err(QuizdomError::Aida(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }
        let id = parse_added_term_id(&String::from_utf8_lossy(&output.stdout))?;
        Ok(TermDefinition {
            id,
            title,
            tags,
            definition: meaning.to_string(),
        })
    }
}

impl<R> AidaCliGeneratedQuestionPersister<R>
where
    R: CommandRunner,
{
    #[cfg(test)]
    pub(crate) fn new(command: impl Into<String>, runner: R) -> Self {
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

fn parse_added_term_id(output: &str) -> Result<String> {
    output
        .split(|character: char| character.is_whitespace() || character == ':')
        .find(|token| token.starts_with("TERM-"))
        .map(str::to_string)
        .ok_or_else(|| QuizdomError::Parse("aida add output did not include TERM id".to_string()))
}
