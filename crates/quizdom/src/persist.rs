use crate::bank::rewrite_weight_and_quality_tags;
use crate::error::{QuizdomError, Result};
use crate::model::{AnswerKind, Question, TermDefinition};
use crate::strategy::{reweight, QualitySignal};
use std::process::{Command, Output};

pub trait GeneratedQuestionPersister {
    /// Persist a generated follow-on linked to `origin` via a `begets` edge.
    ///
    /// `from_answer` (STORY-48) is the normalized answer that triggered the
    /// follow-on; when present it is recorded as a `from-answer:<value>` tag so
    /// the strategy can branch different answers to different follow-ups.
    fn persist_generated_question(
        &self,
        origin: &Question,
        question: &Question,
        from_answer: Option<&str>,
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
        _from_answer: Option<&str>,
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
        from_answer: Option<&str>,
    ) -> Result<Question> {
        // trace:STORY-38 | ai:codex
        let topic = question_topic(origin);
        let tags = generated_question_tags(&topic, &question.answer_kind, from_answer);
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

// trace:STORY-66 | ai:claude
/// Apply a [`QualitySignal`] re-weighting to a question and persist it.
///
/// Implementations adjust the question's `weight:N` (clamped to `[0,100]` by
/// [`reweight`]) and its `quality:*` tag, then write the new tag set back. The
/// returned [`Question`] carries the updated in-memory `weight`/`tags`. This is
/// the curation engine for STORY-66 — deliberately disjoint from the session
/// loop, so the caller decides when (or whether) to invoke it.
pub trait QuestionReweighter {
    fn reweight_question(&self, question: &Question, signal: QualitySignal) -> Result<Question>;
}

/// Compute the re-weighted question in memory without touching AIDA.
///
/// Useful for previewing a re-weight or for tests; mirrors
/// [`NoopGeneratedQuestionPersister`].
pub struct NoopQuestionReweighter;

impl QuestionReweighter for NoopQuestionReweighter {
    fn reweight_question(&self, question: &Question, signal: QualitySignal) -> Result<Question> {
        Ok(apply_reweight(question, signal))
    }
}

/// Build the re-weighted question (new `weight` + rewritten `tags`) in memory.
fn apply_reweight(question: &Question, signal: QualitySignal) -> Question {
    let new_weight = reweight(question.weight, signal);
    let new_tags = rewrite_weight_and_quality_tags(&question.tags, new_weight, signal);
    let mut updated = question.clone();
    updated.weight = new_weight;
    updated.tags = new_tags;
    updated
}

#[allow(dead_code)]
pub(crate) struct AidaCliQuestionReweighter<R = SystemCommandRunner> {
    command: String,
    runner: R,
}

#[allow(dead_code)]
impl Default for AidaCliQuestionReweighter<SystemCommandRunner> {
    fn default() -> Self {
        Self {
            command: "aida".to_string(),
            runner: SystemCommandRunner,
        }
    }
}

impl<R> AidaCliQuestionReweighter<R>
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

impl<R> QuestionReweighter for AidaCliQuestionReweighter<R>
where
    R: CommandRunner,
{
    fn reweight_question(&self, question: &Question, signal: QualitySignal) -> Result<Question> {
        let updated = apply_reweight(question, signal);
        // `weight:N` and `quality:*` are single-valued tags, so we set the full
        // recomputed tag list back with `aida edit --tags` (replace semantics).
        let args = vec![
            "edit".to_string(),
            question.id.clone(),
            "--tags".to_string(),
            updated.tags.join(","),
        ];
        let output = self.runner.run(&self.command, &args)?;
        if !output.status.success() {
            return Err(QuizdomError::Aida(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }
        Ok(updated)
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

fn generated_question_tags(
    topic: &str,
    answer_kind: &AnswerKind,
    from_answer: Option<&str>,
) -> Vec<String> {
    // trace:STORY-48 | ai:claude
    let mut tags = vec![
        format!("topic:{topic}"),
        format!("answer:{}", answer_kind.mode()),
        "weight:50".to_string(),
        "seed".to_string(),
    ];
    if let Some(answer) = from_answer.map(str::trim).filter(|value| !value.is_empty()) {
        tags.push(format!("from-answer:{answer}"));
    }
    tags
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

// trace:STORY-66 | ai:claude
#[cfg(test)]
mod reweight_tests {
    use super::*;
    use crate::model::AnswerKind;
    use std::cell::RefCell;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};

    /// A `CommandRunner` that records every invocation and returns a canned
    /// exit status. `raw_status` is a unix wait-status: `0` succeeds, `1 << 8`
    /// (exit code 1) fails.
    struct RecordingRunner {
        calls: RefCell<Vec<(String, Vec<String>)>>,
        raw_status: i32,
        stderr: String,
    }

    impl RecordingRunner {
        fn ok() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                raw_status: 0,
                stderr: String::new(),
            }
        }

        fn failing(stderr: &str) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                raw_status: 1 << 8,
                stderr: stderr.to_string(),
            }
        }
    }

    impl CommandRunner for RecordingRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<Output> {
            self.calls
                .borrow_mut()
                .push((program.to_string(), args.to_vec()));
            Ok(Output {
                status: ExitStatus::from_raw(self.raw_status),
                stdout: Vec::new(),
                stderr: self.stderr.clone().into_bytes(),
            })
        }
    }

    fn question() -> Question {
        Question {
            id: "Q-7".to_string(),
            title: "Does meaning require permanence?".to_string(),
            answer_kind: AnswerKind::YesNo,
            tags: vec![
                "topic:meaning".to_string(),
                "weight:50".to_string(),
                "quality:neutral".to_string(),
            ],
            weight: 50,
        }
    }

    #[test]
    fn insightful_bumps_and_persists_full_tag_set() {
        let runner = RecordingRunner::ok();
        let reweighter = AidaCliQuestionReweighter::new("aida", runner);
        let updated = reweighter
            .reweight_question(&question(), QualitySignal::Insightful)
            .expect("reweight should succeed");

        assert_eq!(updated.weight, 62);
        assert_eq!(
            updated.tags,
            vec![
                "topic:meaning".to_string(),
                "weight:62".to_string(),
                "quality:insightful".to_string(),
            ]
        );

        let calls = reweighter.runner.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "aida");
        assert_eq!(
            calls[0].1,
            vec![
                "edit".to_string(),
                "Q-7".to_string(),
                "--tags".to_string(),
                "topic:meaning,weight:62,quality:insightful".to_string(),
            ]
        );
    }

    #[test]
    fn unhelpful_decays_and_updates_quality_tag() {
        let runner = RecordingRunner::ok();
        let reweighter = AidaCliQuestionReweighter::new("aida", runner);
        let updated = reweighter
            .reweight_question(&question(), QualitySignal::Unhelpful)
            .expect("reweight should succeed");

        assert_eq!(updated.weight, 38);
        let calls = reweighter.runner.calls.borrow();
        assert_eq!(
            calls[0].1[3],
            "topic:meaning,weight:38,quality:unhelpful".to_string()
        );
    }

    #[test]
    fn decay_is_clamped_to_floor() {
        let mut low = question();
        low.weight = 5;
        low.tags = vec!["topic:meaning".to_string(), "weight:5".to_string()];
        let runner = RecordingRunner::ok();
        let reweighter = AidaCliQuestionReweighter::new("aida", runner);
        let updated = reweighter
            .reweight_question(&low, QualitySignal::Punted)
            .expect("reweight should succeed");

        assert_eq!(updated.weight, 0);
        let calls = reweighter.runner.calls.borrow();
        assert_eq!(
            calls[0].1[3],
            "topic:meaning,weight:0,quality:punted".to_string()
        );
    }

    #[test]
    fn aida_failure_surfaces_as_error() {
        let runner = RecordingRunner::failing("no such requirement Q-7");
        let reweighter = AidaCliQuestionReweighter::new("aida", runner);
        let result = reweighter.reweight_question(&question(), QualitySignal::Insightful);
        match result {
            Err(QuizdomError::Aida(message)) => {
                assert!(message.contains("no such requirement"));
            }
            other => panic!("expected Aida error, got {other:?}"),
        }
    }

    #[test]
    fn noop_reweighter_updates_memory_without_persisting() {
        let updated = NoopQuestionReweighter
            .reweight_question(&question(), QualitySignal::Insightful)
            .expect("noop reweight should succeed");
        assert_eq!(updated.weight, 62);
        assert_eq!(
            updated.tags,
            vec![
                "topic:meaning".to_string(),
                "weight:62".to_string(),
                "quality:insightful".to_string(),
            ]
        );
    }
}
