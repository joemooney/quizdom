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

// trace:STORY-85 | ai:claude
/// How a freshly persisted user-authored question is wired into the domain
/// graph.
///
/// A user can author a question that springs from an existing origin question
/// (`Begets`), that pressure-tests an existing term (`Probes`), or that stands
/// alone as a hand-authored seed with no inbound/outbound edge (`Standalone`).
/// The variant decides which (if any) `aida rel add` is issued after the
/// Q-object is created.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum QuestionLink {
    /// `origin -> new` `begets` edge: the new question follows from `origin`.
    Begets { origin_id: String },
    /// `new -> term` `probes` edge: the new question probes a term definition.
    Probes { term_id: String },
    /// No edge: a hand-authored seed question that bootstraps a cluster.
    Standalone,
}

// trace:STORY-85 | ai:claude
/// Persist a user-authored question as a real Q-object in the AIDA bank.
///
/// Reuses STORY-38's persister shape (create via `aida add --prefix Q --type
/// functional`, then optionally wire an edge) but for hand-authored questions:
/// the Q-object is tagged `source:user-authored`, `answer:<shape>`,
/// `topic:<t>`, and a neutral `weight:50`, and linked according to
/// [`QuestionLink`].
pub trait UserAuthoredQuestionPersister {
    fn persist_user_authored_question(
        &self,
        question: &Question,
        topic: &str,
        link: &QuestionLink,
    ) -> Result<Question>;
}

// trace:STORY-85 | ai:claude
/// Build the user-authored question in memory without touching AIDA.
///
/// Mirrors [`NoopGeneratedQuestionPersister`]: returns the question with the
/// canonical user-authored tag set applied and a neutral `weight:50`, but
/// issues no `aida` commands and assigns no real id.
pub struct NoopUserAuthoredQuestionPersister;

impl UserAuthoredQuestionPersister for NoopUserAuthoredQuestionPersister {
    fn persist_user_authored_question(
        &self,
        question: &Question,
        topic: &str,
        _link: &QuestionLink,
    ) -> Result<Question> {
        let tags = user_authored_question_tags(topic, &question.answer_kind);
        let mut persisted = question.clone();
        persisted.tags = tags;
        persisted.weight = USER_AUTHORED_NEUTRAL_WEIGHT;
        Ok(persisted)
    }
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

// trace:STORY-85 | ai:claude
/// Neutral selection weight applied to a freshly authored user question.
///
/// `50` sits in the schema's "normal reuse" band (`40`-`69`), matching the
/// seed weight STORY-38 gives LLM-minted questions, so user-authored prompts
/// compete on an even footing until curation (STORY-66) re-weights them.
const USER_AUTHORED_NEUTRAL_WEIGHT: u32 = 50;

// trace:STORY-85 | ai:claude
// Foundational persister (per the spec): the type + edge wiring land here so
// later stories can call it from the session loop. Until then it is
// constructed only in tests, mirroring `AidaCliQuestionReweighter` (STORY-66).
#[allow(dead_code)]
pub(crate) struct AidaCliUserAuthoredQuestionPersister<R = SystemCommandRunner> {
    command: String,
    runner: R,
}

#[allow(dead_code)]
impl Default for AidaCliUserAuthoredQuestionPersister<SystemCommandRunner> {
    fn default() -> Self {
        Self {
            command: "aida".to_string(),
            runner: SystemCommandRunner,
        }
    }
}

impl<R> AidaCliUserAuthoredQuestionPersister<R>
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

impl<R> UserAuthoredQuestionPersister for AidaCliUserAuthoredQuestionPersister<R>
where
    R: CommandRunner,
{
    fn persist_user_authored_question(
        &self,
        question: &Question,
        topic: &str,
        link: &QuestionLink,
    ) -> Result<Question> {
        // trace:STORY-85 | ai:claude
        let tags = user_authored_question_tags(topic, &question.answer_kind);
        let description = user_authored_question_description(question, topic, link);
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

        // Wire the requested edge. The edge direction follows the graph schema:
        // `begets` is `origin -> new`, `probes` is `new -> term`. A standalone
        // seed gets no edge.
        if let Some((from, to, edge)) = link.rel_endpoints(&id) {
            let rel_args = vec![
                "rel".to_string(),
                "add".to_string(),
                "--from".to_string(),
                from,
                "--to".to_string(),
                to,
                "--type".to_string(),
                edge.to_string(),
            ];
            let rel_output = self.runner.run(&self.command, &rel_args)?;
            if !rel_output.status.success() {
                return Err(QuizdomError::Aida(
                    String::from_utf8_lossy(&rel_output.stderr).to_string(),
                ));
            }
        }

        let mut persisted = question.clone();
        persisted.id = id;
        persisted.tags = tags;
        persisted.weight = USER_AUTHORED_NEUTRAL_WEIGHT;
        Ok(persisted)
    }
}

// trace:STORY-85 | ai:claude
impl QuestionLink {
    /// Resolve the `(from, to, edge)` triple for `aida rel add`, or `None` for
    /// a standalone seed. `new_id` is the id of the freshly created Q-object.
    #[allow(dead_code)]
    fn rel_endpoints(&self, new_id: &str) -> Option<(String, String, &'static str)> {
        match self {
            QuestionLink::Begets { origin_id } => {
                Some((origin_id.clone(), new_id.to_string(), "begets"))
            }
            QuestionLink::Probes { term_id } => {
                Some((new_id.to_string(), term_id.clone(), "probes"))
            }
            QuestionLink::Standalone => None,
        }
    }
}

// trace:STORY-85 | ai:claude
/// Canonical tag set for a user-authored question: `source:user-authored`,
/// `topic:<t>`, `answer:<shape>`, and a neutral `weight:50`. A `seed` tag marks
/// it hand-authored, mirroring the seed clusters in the graph schema.
fn user_authored_question_tags(topic: &str, answer_kind: &AnswerKind) -> Vec<String> {
    let topic = topic.trim();
    let topic = if topic.is_empty() {
        "user-authored"
    } else {
        topic
    };
    vec![
        "source:user-authored".to_string(),
        format!("topic:{topic}"),
        format!("answer:{}", answer_kind.mode()),
        format!("weight:{USER_AUTHORED_NEUTRAL_WEIGHT}"),
        "seed".to_string(),
    ]
}

// trace:STORY-85 | ai:claude
#[allow(dead_code)]
fn user_authored_question_description(
    question: &Question,
    topic: &str,
    link: &QuestionLink,
) -> String {
    let provenance = match link {
        QuestionLink::Begets { origin_id } => format!("begets from origin question: {origin_id}"),
        QuestionLink::Probes { term_id } => format!("probes term: {term_id}"),
        QuestionLink::Standalone => "standalone seed".to_string(),
    };
    format!(
        "User-authored quizdom question.\n\nanswer: {}\ntopic: {topic}\nlink: {provenance}",
        question.answer_kind.mode()
    )
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

// trace:STORY-85 | ai:claude
#[cfg(test)]
mod user_authored_tests {
    use super::*;
    use crate::model::AnswerKind;
    use std::cell::RefCell;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};

    /// A `CommandRunner` that records every invocation and replays canned
    /// stdout/exit-status per call (FIFO), so we can hand the `aida add` call a
    /// stdout containing the freshly minted Q id while later `rel add` calls
    /// succeed silently.
    struct ScriptedRunner {
        calls: RefCell<Vec<(String, Vec<String>)>>,
        responses: RefCell<Vec<(i32, String, String)>>,
    }

    impl ScriptedRunner {
        /// `responses` are `(raw_status, stdout, stderr)` replayed in order.
        fn new(responses: Vec<(i32, &str, &str)>) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                responses: RefCell::new(
                    responses
                        .into_iter()
                        .map(|(status, out, err)| (status, out.to_string(), err.to_string()))
                        .collect(),
                ),
            }
        }
    }

    impl CommandRunner for ScriptedRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<Output> {
            self.calls
                .borrow_mut()
                .push((program.to_string(), args.to_vec()));
            let (raw_status, stdout, stderr) = {
                let mut responses = self.responses.borrow_mut();
                if responses.is_empty() {
                    (0, String::new(), String::new())
                } else {
                    responses.remove(0)
                }
            };
            Ok(Output {
                status: ExitStatus::from_raw(raw_status),
                stdout: stdout.into_bytes(),
                stderr: stderr.into_bytes(),
            })
        }
    }

    fn question() -> Question {
        Question {
            id: String::new(),
            title: "Is the self continuous over time?".to_string(),
            answer_kind: AnswerKind::YesNo,
            tags: Vec::new(),
            weight: 0,
        }
    }

    /// Find the value following a flag in an `aida` arg vector.
    fn flag<'a>(args: &'a [String], flag: &str) -> &'a str {
        let index = args
            .iter()
            .position(|arg| arg == flag)
            .unwrap_or_else(|| panic!("missing {flag} in {args:?}"));
        &args[index + 1]
    }

    #[test]
    fn create_emits_user_authored_tags_and_neutral_weight() {
        let runner = ScriptedRunner::new(vec![(0, "Added Q-21", "")]);
        let persister = AidaCliUserAuthoredQuestionPersister::new("aida", runner);
        let persisted = persister
            .persist_user_authored_question(&question(), "identity", &QuestionLink::Standalone)
            .expect("standalone create should succeed");

        assert_eq!(persisted.id, "Q-21");
        assert_eq!(persisted.weight, 50);
        assert_eq!(
            persisted.tags,
            vec![
                "source:user-authored".to_string(),
                "topic:identity".to_string(),
                "answer:yes-no".to_string(),
                "weight:50".to_string(),
                "seed".to_string(),
            ]
        );

        let calls = persister.runner.calls.borrow();
        // Standalone -> exactly one call (the add), no rel edge.
        assert_eq!(calls.len(), 1);
        let add = &calls[0].1;
        assert_eq!(add[0], "add");
        assert_eq!(flag(add, "--prefix"), "Q");
        assert_eq!(flag(add, "--type"), "functional");
        assert_eq!(flag(add, "--title"), "Is the self continuous over time?");
        assert_eq!(
            flag(add, "--tags"),
            "source:user-authored,topic:identity,answer:yes-no,weight:50,seed"
        );
    }

    #[test]
    fn begets_link_adds_origin_to_new_edge() {
        let runner = ScriptedRunner::new(vec![(0, "Added Q-30", ""), (0, "", "")]);
        let persister = AidaCliUserAuthoredQuestionPersister::new("aida", runner);
        let persisted = persister
            .persist_user_authored_question(
                &question(),
                "identity",
                &QuestionLink::Begets {
                    origin_id: "Q-7".to_string(),
                },
            )
            .expect("begets create should succeed");

        assert_eq!(persisted.id, "Q-30");
        let calls = persister.runner.calls.borrow();
        assert_eq!(calls.len(), 2);
        let rel = &calls[1].1;
        assert_eq!(rel[0], "rel");
        assert_eq!(rel[1], "add");
        // begets is origin -> new.
        assert_eq!(flag(rel, "--from"), "Q-7");
        assert_eq!(flag(rel, "--to"), "Q-30");
        assert_eq!(flag(rel, "--type"), "begets");
    }

    #[test]
    fn probes_link_adds_new_to_term_edge() {
        let runner = ScriptedRunner::new(vec![(0, "Added Q-31", ""), (0, "", "")]);
        let persister = AidaCliUserAuthoredQuestionPersister::new("aida", runner);
        let mut free_text = question();
        free_text.answer_kind = AnswerKind::FreeText;
        let persisted = persister
            .persist_user_authored_question(
                &free_text,
                "free-will",
                &QuestionLink::Probes {
                    term_id: "TERM-3".to_string(),
                },
            )
            .expect("probes create should succeed");

        assert_eq!(persisted.id, "Q-31");
        assert!(persisted.tags.contains(&"answer:free-text".to_string()));
        let calls = persister.runner.calls.borrow();
        assert_eq!(calls.len(), 2);
        let rel = &calls[1].1;
        // probes is new -> term.
        assert_eq!(flag(rel, "--from"), "Q-31");
        assert_eq!(flag(rel, "--to"), "TERM-3");
        assert_eq!(flag(rel, "--type"), "probes");
    }

    #[test]
    fn empty_topic_falls_back_to_user_authored() {
        let runner = ScriptedRunner::new(vec![(0, "Added Q-40", "")]);
        let persister = AidaCliUserAuthoredQuestionPersister::new("aida", runner);
        let persisted = persister
            .persist_user_authored_question(&question(), "  ", &QuestionLink::Standalone)
            .expect("create should succeed");
        assert!(persisted.tags.contains(&"topic:user-authored".to_string()));
    }

    #[test]
    fn add_failure_surfaces_as_error_and_skips_rel() {
        let runner = ScriptedRunner::new(vec![(1 << 8, "", "duplicate title")]);
        let persister = AidaCliUserAuthoredQuestionPersister::new("aida", runner);
        let result = persister.persist_user_authored_question(
            &question(),
            "identity",
            &QuestionLink::Begets {
                origin_id: "Q-7".to_string(),
            },
        );
        match result {
            Err(QuizdomError::Aida(message)) => assert!(message.contains("duplicate title")),
            other => panic!("expected Aida error, got {other:?}"),
        }
        // The failed add must not be followed by a rel add.
        assert_eq!(persister.runner.calls.borrow().len(), 1);
    }

    #[test]
    fn rel_failure_surfaces_as_error() {
        let runner = ScriptedRunner::new(vec![(0, "Added Q-50", ""), (1 << 8, "", "no such node")]);
        let persister = AidaCliUserAuthoredQuestionPersister::new("aida", runner);
        let result = persister.persist_user_authored_question(
            &question(),
            "identity",
            &QuestionLink::Probes {
                term_id: "TERM-9".to_string(),
            },
        );
        match result {
            Err(QuizdomError::Aida(message)) => assert!(message.contains("no such node")),
            other => panic!("expected Aida error, got {other:?}"),
        }
    }

    #[test]
    fn missing_id_in_add_output_is_a_parse_error() {
        let runner = ScriptedRunner::new(vec![(0, "no id here", "")]);
        let persister = AidaCliUserAuthoredQuestionPersister::new("aida", runner);
        let result = persister.persist_user_authored_question(
            &question(),
            "identity",
            &QuestionLink::Standalone,
        );
        assert!(matches!(result, Err(QuizdomError::Parse(_))));
    }

    #[test]
    fn noop_persister_applies_tags_without_running_aida() {
        let persisted = NoopUserAuthoredQuestionPersister
            .persist_user_authored_question(
                &question(),
                "identity",
                &QuestionLink::Begets {
                    origin_id: "Q-7".to_string(),
                },
            )
            .expect("noop persist should succeed");
        assert_eq!(persisted.weight, 50);
        assert_eq!(
            persisted.tags,
            vec![
                "source:user-authored".to_string(),
                "topic:identity".to_string(),
                "answer:yes-no".to_string(),
                "weight:50".to_string(),
                "seed".to_string(),
            ]
        );
        // Noop assigns no real id.
        assert!(persisted.id.is_empty());
    }
}
