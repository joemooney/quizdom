use crate::bank::QuestionBank;
use crate::error::{QuizdomError, Result};
use crate::model::{
    answer_kind_from_tags, from_answer_tag, Answer, AnswerKind, Question, TermDefinition,
    TermMappingProposal,
};
use crate::persist::{GeneratedQuestionPersister, NoopGeneratedQuestionPersister};
use llm::{LLMClient, Message};
use serde_json::Value;

const SOCRATIC_SYSTEM_PROMPT: &str = "You are quizdom's Socratic belief-exploration engine. There are no correct answers. Explore and challenge the user's beliefs, probe semantic nuance, and prefer formal or shared definitions before bespoke meanings. Decide whether to select an existing follow-up question or generate one new concise follow-up question.";

pub trait NextQuestionStrategy {
    fn next_question(
        &self,
        current: &Question,
        context: &StrategyContext,
        bank: &dyn QuestionBank,
    ) -> Result<Option<Question>>;

    fn loaded_terms(&self, _current: &Question, _answer: &Answer) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn map_term_meaning(
        &self,
        _term_label: &str,
        _meaning: &str,
        _definitions: &[TermDefinition],
    ) -> Result<Option<TermMappingProposal>> {
        Ok(None)
    }
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

impl NextQuestionStrategy for DeterministicNextQuestionStrategy {
    fn next_question(
        &self,
        current: &Question,
        context: &StrategyContext,
        bank: &dyn QuestionBank,
    ) -> Result<Option<Question>> {
        let successors = relevant_successors(current, &context.answer, bank)?;
        Ok(successors.into_iter().next())
    }
}

fn successor_questions(current: &Question, bank: &dyn QuestionBank) -> Result<Vec<Question>> {
    bank.begets(&current.id)?
        .into_iter()
        .map(|question_ref| bank.load_question(&question_ref.id))
        .collect()
}

// trace:STORY-48 | ai:claude
/// How relevant a `begets` successor is to the answer just given. A higher
/// score is preferred; a score of `0` excludes the successor from automatic
/// selection because it was conditioned on a different answer.
fn successor_relevance(question: &Question, answer: &Answer) -> u8 {
    match from_answer_tag(&question.tags) {
        // Conditioned on this exact answer — answer-conditioned branching.
        Some(tag) if answer_tag_matches(tag, answer) => 2,
        // Conditioned on a different answer — not for this branch.
        Some(_) => 0,
        // Unconditional follow-on — always eligible (legacy behavior).
        None => 1,
    }
}

fn answer_tag_matches(tag_value: &str, answer: &Answer) -> bool {
    let needle = tag_value.trim().to_ascii_lowercase();
    !needle.is_empty()
        && (needle == answer.normalized.trim().to_ascii_lowercase()
            || needle == answer.raw.trim().to_ascii_lowercase())
}

/// Successors eligible for the current answer, ordered by answer relevance,
/// then weight, then id. Successors conditioned on a different answer are
/// dropped so different answers branch to different follow-ups (STORY-48).
fn relevant_successors(
    current: &Question,
    answer: &Answer,
    bank: &dyn QuestionBank,
) -> Result<Vec<Question>> {
    let mut successors = successor_questions(current, bank)?
        .into_iter()
        .filter_map(|question| {
            let relevance = successor_relevance(&question, answer);
            (relevance > 0).then_some((relevance, question))
        })
        .collect::<Vec<_>>();
    successors.sort_by(|(left_relevance, left), (right_relevance, right)| {
        right_relevance
            .cmp(left_relevance)
            .then_with(|| right.weight.cmp(&left.weight))
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(successors
        .into_iter()
        .map(|(_, question)| question)
        .collect())
}

// trace:STORY-48 | ai:claude
/// The answer value to record on a generated follow-on so it can be matched
/// later. Only bounded answers (yes/no, choice) branch usefully; free-text
/// answers are open-ended, so their follow-ons stay unconditional.
fn triggering_answer(current: &Question, context: &StrategyContext) -> Option<String> {
    match current.answer_kind {
        AnswerKind::YesNo | AnswerKind::Choice(_) => {
            let normalized = context.answer.normalized.trim();
            (!normalized.is_empty()).then(|| normalized.to_ascii_lowercase())
        }
        AnswerKind::FreeText => None,
    }
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

    fn loaded_terms(&self, current: &Question, answer: &Answer) -> Result<Vec<String>> {
        self.llm_loaded_terms(current, answer).or(Ok(Vec::new()))
    }

    fn map_term_meaning(
        &self,
        term_label: &str,
        meaning: &str,
        definitions: &[TermDefinition],
    ) -> Result<Option<TermMappingProposal>> {
        self.llm_map_term_meaning(term_label, meaning, definitions)
            .or(Ok(None))
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
        let candidates = relevant_successors(current, &context.answer, bank).unwrap_or_default();
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
                // trace:STORY-48 | ai:claude
                .persist_generated_question(
                    current,
                    &question,
                    triggering_answer(current, context).as_deref(),
                )
                .map(Some),
            other => Ok(other),
        }
    }

    fn llm_loaded_terms(&self, current: &Question, answer: &Answer) -> Result<Vec<String>> {
        let prompt = format!(
            "Question: {question}\nAnswer: {answer}\n\nReturn only JSON: {{\"loaded_terms\":[\"term\"]}}. Include loaded philosophical or semantic terms whose competing definitions would help interpret the answer. Use an empty list if none.",
            question = current.title,
            answer = answer.raw,
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(QuizdomError::Io)?;
        let (text, _tool_calls) = runtime
            .block_on(self.client.call(
                "Detect loaded terms in the user's answer.",
                &[Message::user(prompt)],
                &[],
            ))
            .map_err(|error| QuizdomError::Aida(error.to_string()))?;
        parse_loaded_terms(&text)
    }

    fn llm_map_term_meaning(
        &self,
        term_label: &str,
        meaning: &str,
        definitions: &[TermDefinition],
    ) -> Result<Option<TermMappingProposal>> {
        if definitions.is_empty() {
            return Ok(None);
        }
        let prompt = term_mapping_prompt(term_label, meaning, definitions);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(QuizdomError::Io)?;
        let (text, _tool_calls) = runtime
            .block_on(self.client.call(
                "Map the user's term meaning to the closest formal bank definition.",
                &[Message::user(prompt)],
                &[],
            ))
            .map_err(|error| QuizdomError::Aida(error.to_string()))?;
        parse_term_mapping(&text, definitions)
    }
}

pub(crate) fn parse_loaded_terms(text: &str) -> Result<Vec<String>> {
    let value: Value = serde_json::from_str(text.trim())
        .map_err(|error| QuizdomError::Parse(format!("invalid loaded-term JSON: {error}")))?;
    let Some(items) = value.get("loaded_terms").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    Ok(items
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(str::to_string)
        .collect())
}

pub(crate) fn term_mapping_prompt(
    term_label: &str,
    meaning: &str,
    definitions: &[TermDefinition],
) -> String {
    let mut prompt =
        format!("Loaded term: {term_label}\nUser meaning: {meaning}\n\nBank definitions:\n");
    for definition in definitions {
        prompt.push_str(&format!(
            "- {} | {} | {}\n",
            definition.id, definition.title, definition.definition
        ));
    }
    prompt.push_str(
        "\nReturn only JSON: {\"term_id\":\"TERM-...\",\"rationale\":\"short reason\"}. Choose the closest formal/academic bank definition.",
    );
    prompt
}

pub(crate) fn parse_term_mapping(
    text: &str,
    definitions: &[TermDefinition],
) -> Result<Option<TermMappingProposal>> {
    let value: Value = serde_json::from_str(text.trim())
        .map_err(|error| QuizdomError::Parse(format!("invalid term-mapping JSON: {error}")))?;
    let Some(term_id) = value.get("term_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(definition) = definitions
        .iter()
        .find(|definition| definition.id == term_id)
    else {
        return Ok(None);
    };
    Ok(Some(TermMappingProposal {
        term_id: definition.id.clone(),
        term_title: definition.title.clone(),
        definition: definition.definition.clone(),
        rationale: value
            .get("rationale")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    }))
}

// trace:STORY-66 | ai:claude
/// A quality signal observed for a question after it was asked.
///
/// Drives the re-weighting engine: `Insightful` bumps a question's weight so it
/// surfaces sooner again, `Unhelpful`/`Punted` decay it, and `Neutral` leaves
/// it unchanged. Decoupled from the session loop — callers map their own UX
/// into one of these variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualitySignal {
    Insightful,
    Neutral,
    Unhelpful,
    Punted,
}

/// Lower bound for a question weight.
pub const WEIGHT_MIN: u32 = 0;
/// Upper bound for a question weight.
pub const WEIGHT_MAX: u32 = 100;

const INSIGHTFUL_BUMP: i32 = 12;
const UNHELPFUL_DECAY: i32 = 12;
const PUNTED_DECAY: i32 = 20;

impl QualitySignal {
    /// The signed weight delta this signal applies before clamping.
    pub fn weight_delta(self) -> i32 {
        match self {
            QualitySignal::Insightful => INSIGHTFUL_BUMP,
            QualitySignal::Neutral => 0,
            QualitySignal::Unhelpful => -UNHELPFUL_DECAY,
            QualitySignal::Punted => -PUNTED_DECAY,
        }
    }

    /// The `quality:*` tag value this signal records on the question.
    pub fn quality_tag(self) -> &'static str {
        match self {
            QualitySignal::Insightful => "quality:insightful",
            QualitySignal::Neutral => "quality:neutral",
            QualitySignal::Unhelpful => "quality:unhelpful",
            QualitySignal::Punted => "quality:punted",
        }
    }
}

/// Apply a quality signal to a current weight, clamped to
/// `[WEIGHT_MIN, WEIGHT_MAX]`.
///
/// Pure and total: saturating on both ends so a decayed-to-zero or
/// bumped-past-100 weight settles at the bound rather than wrapping.
// trace:STORY-66 | ai:claude
pub fn reweight(current: u32, signal: QualitySignal) -> u32 {
    let adjusted = current as i32 + signal.weight_delta();
    adjusted.clamp(WEIGHT_MIN as i32, WEIGHT_MAX as i32) as u32
}

// trace:STORY-66 | ai:claude
#[cfg(test)]
mod reweight_tests {
    use super::{reweight, QualitySignal, WEIGHT_MAX, WEIGHT_MIN};

    #[test]
    fn insightful_bumps_weight() {
        assert_eq!(reweight(50, QualitySignal::Insightful), 62);
    }

    #[test]
    fn neutral_leaves_weight_unchanged() {
        assert_eq!(reweight(50, QualitySignal::Neutral), 50);
    }

    #[test]
    fn unhelpful_and_punted_decay_weight() {
        assert_eq!(reweight(50, QualitySignal::Unhelpful), 38);
        assert_eq!(reweight(50, QualitySignal::Punted), 30);
    }

    #[test]
    fn bump_is_clamped_to_max() {
        assert_eq!(reweight(95, QualitySignal::Insightful), WEIGHT_MAX);
        assert_eq!(reweight(WEIGHT_MAX, QualitySignal::Insightful), WEIGHT_MAX);
    }

    #[test]
    fn decay_is_clamped_to_min() {
        assert_eq!(reweight(10, QualitySignal::Unhelpful), 0);
        assert_eq!(reweight(5, QualitySignal::Punted), WEIGHT_MIN);
        assert_eq!(reweight(WEIGHT_MIN, QualitySignal::Unhelpful), WEIGHT_MIN);
    }

    #[test]
    fn quality_tags_cover_every_signal() {
        assert_eq!(
            QualitySignal::Insightful.quality_tag(),
            "quality:insightful"
        );
        assert_eq!(QualitySignal::Neutral.quality_tag(), "quality:neutral");
        assert_eq!(QualitySignal::Unhelpful.quality_tag(), "quality:unhelpful");
        assert_eq!(QualitySignal::Punted.quality_tag(), "quality:punted");
    }
}
