use crate::bank::rewrite_quality_tags;
#[cfg(test)]
use crate::error::QuizdomError;
use crate::error::Result;
use crate::model::{AnswerKind, Question, TermDefinition};
// trace:STORY-208 | ai:claude — persisters write straight to the Dolt domain
// store since the cutover; the selection weight is a first-class field.
use crate::dolt_store::{domain_store_from_config, DoltDomainStore};
use crate::store::{DomainStore, EdgeKind, NewNode, NodeKind};
use crate::strategy::{reweight, QualitySignal};

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

pub(crate) struct AidaCliGeneratedQuestionPersister<S = DoltDomainStore> {
    store: S,
}

impl Default for AidaCliGeneratedQuestionPersister {
    fn default() -> Self {
        Self {
            store: domain_store_from_config(),
        }
    }
}

pub(crate) struct AidaCliUserSpecificTermPersister<S = DoltDomainStore> {
    store: S,
}

impl Default for AidaCliUserSpecificTermPersister {
    fn default() -> Self {
        Self {
            store: domain_store_from_config(),
        }
    }
}

#[cfg(test)]
impl<S> AidaCliUserSpecificTermPersister<S>
where
    S: DomainStore,
{
    pub(crate) fn with_store(store: S) -> Self {
        Self { store }
    }
}

impl<S> UserSpecificTermPersister for AidaCliUserSpecificTermPersister<S>
where
    S: DomainStore,
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
        ];
        let description = format!(
            "source: user-specific quizdom steering fallback.\n\ndefinition: {meaning}\n\nscope: user-specific definition captured only after shared bank definitions did not fit."
        );
        let id = self.store.create_node(&NewNode {
            kind: NodeKind::Term,
            title: title.clone(),
            description,
            tags: tags.clone(),
            weight: USER_SPECIFIC_TERM_WEIGHT,
        })?;
        Ok(TermDefinition {
            id,
            title,
            tags,
            definition: meaning.to_string(),
        })
    }
}

#[cfg(test)]
impl<S> AidaCliGeneratedQuestionPersister<S>
where
    S: DomainStore,
{
    pub(crate) fn with_store(store: S) -> Self {
        Self { store }
    }
}

impl<S> GeneratedQuestionPersister for AidaCliGeneratedQuestionPersister<S>
where
    S: DomainStore,
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
        let id = self.store.create_node(&NewNode {
            kind: NodeKind::Question,
            title: question.title.clone(),
            description,
            tags: tags.clone(),
            weight: GENERATED_NEUTRAL_WEIGHT,
        })?;
        self.store.create_edge(&origin.id, &id, EdgeKind::Begets)?;

        let mut persisted = question.clone();
        persisted.id = id;
        persisted.tags = tags;
        persisted.weight = GENERATED_NEUTRAL_WEIGHT;
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

// trace:STORY-38 | ai:claude
/// Neutral selection weight for an LLM-generated follow-on question.
const GENERATED_NEUTRAL_WEIGHT: u32 = 50;

// trace:STORY-43 | ai:claude
/// Selection weight for a user-specific term definition — below the shared
/// bank definitions so it only steers when nothing shared fits.
const USER_SPECIFIC_TERM_WEIGHT: u32 = 40;

// trace:STORY-85 | ai:claude
// trace:STORY-88 | ai:claude
// Foundational persister (per the spec): the type + edge wiring land here. The
// standalone `quizdom question add` command (STORY-87) and the in-session
// quick-add control (STORY-88) both drive it via the shared authoring core.
pub(crate) struct AidaCliUserAuthoredQuestionPersister<S = DoltDomainStore> {
    store: S,
}

impl Default for AidaCliUserAuthoredQuestionPersister {
    fn default() -> Self {
        Self {
            store: domain_store_from_config(),
        }
    }
}

#[cfg(test)]
impl<S> AidaCliUserAuthoredQuestionPersister<S>
where
    S: DomainStore,
{
    pub(crate) fn with_store(store: S) -> Self {
        Self { store }
    }
}

impl<S> UserAuthoredQuestionPersister for AidaCliUserAuthoredQuestionPersister<S>
where
    S: DomainStore,
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
        let id = self.store.create_node(&NewNode {
            kind: NodeKind::Question,
            title: question.title.clone(),
            description,
            tags: tags.clone(),
            weight: USER_AUTHORED_NEUTRAL_WEIGHT,
        })?;

        // Wire the requested edge. The edge direction follows the graph schema:
        // `begets` is `origin -> new`, `probes` is `new -> term`. A standalone
        // seed gets no edge.
        if let Some((from, to, edge)) = link.rel_endpoints(&id) {
            self.store.create_edge(&from, &to, edge)?;
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
    /// Resolve the `(from, to, edge)` triple for the edge to create, or `None`
    /// for a standalone seed. `new_id` is the id of the freshly created
    /// Q-object.
    #[allow(dead_code)]
    fn rel_endpoints(&self, new_id: &str) -> Option<(String, String, EdgeKind)> {
        match self {
            QuestionLink::Begets { origin_id } => {
                Some((origin_id.clone(), new_id.to_string(), EdgeKind::Begets))
            }
            QuestionLink::Probes { term_id } => {
                Some((new_id.to_string(), term_id.clone(), EdgeKind::Probes))
            }
            QuestionLink::Standalone => None,
        }
    }
}

// trace:STORY-85 | ai:claude
/// Canonical tag set for a user-authored question: `source:user-authored`,
/// `topic:<t>`, and `answer:<shape>`. A `seed` tag marks it hand-authored,
/// mirroring the seed clusters in the graph schema. The neutral weight is a
/// first-class field, not a tag (STORY-208).
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
/// Implementations adjust the question's numeric weight (clamped to `[0,100]`
/// by [`reweight`]) and its `quality:*` tag, then write both back in one
/// store update. The returned [`Question`] carries the updated in-memory
/// `weight`/`tags`. This is the curation engine for STORY-66 — deliberately
/// disjoint from the session loop, so the caller decides when (or whether) to
/// invoke it.
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
    let new_tags = rewrite_quality_tags(&question.tags, signal);
    let mut updated = question.clone();
    updated.weight = new_weight;
    updated.tags = new_tags;
    updated
}

#[allow(dead_code)]
pub(crate) struct AidaCliQuestionReweighter<S = DoltDomainStore> {
    store: S,
}

#[allow(dead_code)]
impl Default for AidaCliQuestionReweighter {
    fn default() -> Self {
        Self {
            store: domain_store_from_config(),
        }
    }
}

#[cfg(test)]
impl<S> AidaCliQuestionReweighter<S>
where
    S: DomainStore,
{
    pub(crate) fn with_store(store: S) -> Self {
        Self { store }
    }
}

impl<S> QuestionReweighter for AidaCliQuestionReweighter<S>
where
    S: DomainStore,
{
    fn reweight_question(&self, question: &Question, signal: QualitySignal) -> Result<Question> {
        let updated = apply_reweight(question, signal);
        // trace:STORY-208 | ai:claude — one write: the recomputed weight goes
        // to the numeric column, the rewritten quality tag to the tag list.
        self.store
            .update_weight_and_tags(&question.id, updated.weight, &updated.tags)?;
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

// trace:STORY-66 | ai:claude
// trace:STORY-208 | ai:claude — reweight persistence now goes to the Dolt
// store: one UPDATE carrying the numeric weight and the rewritten tags.
#[cfg(test)]
mod reweight_tests {
    use super::*;
    use crate::dolt_store::{DoltDomainStore, ScriptedDoltRunner};
    use crate::model::AnswerKind;

    fn reweighter_with(
        responses: Vec<(i32, &str, &str)>,
    ) -> (
        AidaCliQuestionReweighter<DoltDomainStore<ScriptedDoltRunner>>,
        ScriptedDoltRunner,
    ) {
        let runner = ScriptedDoltRunner::new(responses);
        let handle = runner.clone();
        let reweighter = AidaCliQuestionReweighter::with_store(DoltDomainStore::with_runner(
            "/tmp/quizdom-dolt",
            runner,
        ));
        (reweighter, handle)
    }

    fn question() -> Question {
        Question {
            id: "Q-7".to_string(),
            title: "Does meaning require permanence?".to_string(),
            answer_kind: AnswerKind::YesNo,
            tags: vec!["topic:meaning".to_string(), "quality:neutral".to_string()],
            weight: 50,
        }
    }

    #[test]
    fn insightful_bumps_weight_column_and_quality_tag() {
        let (reweighter, runner) = reweighter_with(vec![(0, "", ""), (0, "", ""), (0, "", "")]);
        let updated = reweighter
            .reweight_question(&question(), QualitySignal::Insightful)
            .expect("reweight should succeed");

        assert_eq!(updated.weight, 62);
        assert_eq!(
            updated.tags,
            vec![
                "topic:meaning".to_string(),
                "quality:insightful".to_string()
            ]
        );

        let calls = runner.calls.borrow();
        let update = ScriptedDoltRunner::sql_of_call(&calls[0]);
        assert!(update.contains("tags = 'topic:meaning,quality:insightful'"));
        assert!(update.contains("weight = 62"));
        assert!(update.contains("WHERE id = 'Q-7'"));
    }

    #[test]
    fn unhelpful_decays_and_updates_quality_tag() {
        let (reweighter, runner) = reweighter_with(vec![(0, "", ""), (0, "", ""), (0, "", "")]);
        let updated = reweighter
            .reweight_question(&question(), QualitySignal::Unhelpful)
            .expect("reweight should succeed");

        assert_eq!(updated.weight, 38);
        let calls = runner.calls.borrow();
        let update = ScriptedDoltRunner::sql_of_call(&calls[0]);
        assert!(update.contains("weight = 38"));
        assert!(update.contains("quality:unhelpful"));
    }

    #[test]
    fn decay_is_clamped_to_floor() {
        let mut low = question();
        low.weight = 5;
        low.tags = vec!["topic:meaning".to_string()];
        let (reweighter, runner) = reweighter_with(vec![(0, "", ""), (0, "", ""), (0, "", "")]);
        let updated = reweighter
            .reweight_question(&low, QualitySignal::Punted)
            .expect("reweight should succeed");

        assert_eq!(updated.weight, 0);
        let calls = runner.calls.borrow();
        let update = ScriptedDoltRunner::sql_of_call(&calls[0]);
        assert!(update.contains("weight = 0"));
        assert!(update.contains("quality:punted"));
    }

    #[test]
    fn store_failure_surfaces_as_error() {
        let (reweighter, _runner) = reweighter_with(vec![(1 << 8, "", "table not found: nodes")]);
        let result = reweighter.reweight_question(&question(), QualitySignal::Insightful);
        match result {
            Err(QuizdomError::Dolt(message)) => {
                assert!(message.contains("table not found"));
            }
            other => panic!("expected Dolt error, got {other:?}"),
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
                "quality:insightful".to_string()
            ]
        );
    }
}

// trace:STORY-85 | ai:claude
// trace:STORY-208 | ai:claude — user-authored persistence lands in the Dolt
// store: create_node mints the id, the link edge is a plain edges insert.
#[cfg(test)]
mod user_authored_tests {
    use super::*;
    use crate::dolt_store::{DoltDomainStore, ScriptedDoltRunner};
    use crate::model::AnswerKind;

    fn persister_with(
        responses: Vec<(i32, &str, &str)>,
    ) -> (
        AidaCliUserAuthoredQuestionPersister<DoltDomainStore<ScriptedDoltRunner>>,
        ScriptedDoltRunner,
    ) {
        let runner = ScriptedDoltRunner::new(responses);
        let handle = runner.clone();
        let persister = AidaCliUserAuthoredQuestionPersister::with_store(
            DoltDomainStore::with_runner("/tmp/quizdom-dolt", runner),
        );
        (persister, handle)
    }

    /// The canned response for the id-mint scan: the highest existing Q id.
    fn mint_scan(highest: &str) -> (i32, String, String) {
        (
            0,
            format!(r#"{{"rows":[{{"id":"{highest}"}}]}}"#),
            String::new(),
        )
    }

    fn persister_minting(
        highest: &str,
    ) -> (
        AidaCliUserAuthoredQuestionPersister<DoltDomainStore<ScriptedDoltRunner>>,
        ScriptedDoltRunner,
    ) {
        let (status, out, err) = mint_scan(highest);
        persister_with(vec![(status, &out, &err)])
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

    #[test]
    fn create_emits_user_authored_tags_and_neutral_weight() {
        let (persister, runner) = persister_minting("Q-20");
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
                "seed".to_string(),
            ]
        );

        let calls = runner.calls.borrow();
        // Standalone -> mint scan + insert + add + commit, no edge insert.
        assert_eq!(calls.len(), 4);
        let insert = ScriptedDoltRunner::sql_of_call(&calls[1]);
        assert!(insert.contains("'Q-21'"));
        assert!(insert.contains("'question'"));
        assert!(insert.contains("'Is the self continuous over time?'"));
        assert!(
            insert.contains("'source:user-authored,topic:identity,answer:yes-no,seed'"),
            "no weight tag in the tags column: {insert}"
        );
        assert!(insert.contains(", 50)"), "weight in the column: {insert}");
    }

    #[test]
    fn begets_link_adds_origin_to_new_edge() {
        let (persister, runner) = persister_minting("Q-29");
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
        let calls = runner.calls.borrow();
        // mint + insert + add + commit, then edge insert + add + commit.
        assert_eq!(calls.len(), 7);
        let edge = ScriptedDoltRunner::sql_of_call(&calls[4]);
        // begets is origin -> new.
        assert!(edge.contains("INSERT INTO edges"));
        assert!(edge.contains("'Q-7', 'Q-30', 'begets'"));
    }

    #[test]
    fn probes_link_adds_new_to_term_edge() {
        let (persister, runner) = persister_minting("Q-30");
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
        let calls = runner.calls.borrow();
        let edge = ScriptedDoltRunner::sql_of_call(&calls[4]);
        // probes is new -> term.
        assert!(edge.contains("'Q-31', 'TERM-3', 'probes'"));
    }

    #[test]
    fn empty_topic_falls_back_to_user_authored() {
        let (persister, _runner) = persister_minting("Q-39");
        let persisted = persister
            .persist_user_authored_question(&question(), "  ", &QuestionLink::Standalone)
            .expect("create should succeed");
        assert!(persisted.tags.contains(&"topic:user-authored".to_string()));
    }

    #[test]
    fn create_failure_surfaces_as_error_and_skips_edge() {
        let (persister, runner) = persister_with(vec![(1 << 8, "", "table not found: nodes")]);
        let result = persister.persist_user_authored_question(
            &question(),
            "identity",
            &QuestionLink::Begets {
                origin_id: "Q-7".to_string(),
            },
        );
        match result {
            Err(QuizdomError::Dolt(message)) => assert!(message.contains("table not found")),
            other => panic!("expected Dolt error, got {other:?}"),
        }
        // The failed create must not be followed by an edge insert.
        assert_eq!(runner.calls.borrow().len(), 1);
    }

    #[test]
    fn edge_failure_surfaces_as_error() {
        let (status, out, err) = mint_scan("Q-49");
        let (persister, _runner) = persister_with(vec![
            (status, &out, &err),
            (0, "", ""),                  // insert
            (0, "", ""),                  // add -A
            (0, "", ""),                  // commit
            (1 << 8, "", "no such node"), // edge insert fails
        ]);
        let result = persister.persist_user_authored_question(
            &question(),
            "identity",
            &QuestionLink::Probes {
                term_id: "TERM-9".to_string(),
            },
        );
        match result {
            Err(QuizdomError::Dolt(message)) => assert!(message.contains("no such node")),
            other => panic!("expected Dolt error, got {other:?}"),
        }
    }

    #[test]
    fn noop_persister_applies_tags_without_touching_the_store() {
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
                "seed".to_string(),
            ]
        );
        // Noop assigns no real id.
        assert!(persisted.id.is_empty());
    }
}
