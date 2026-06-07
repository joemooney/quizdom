use crate::bank::{find_near_duplicate, NearDuplicate, QuestionBank, DEDUP_SIMILARITY_THRESHOLD};
use crate::error::{QuizdomError, Result};
use crate::model::{
    answer_kind_from_tags, from_answer_tag, Answer, AnswerKind, Question, RefinementProposal,
    TermDefinition, TermMappingProposal,
};
use crate::persist::{GeneratedQuestionPersister, NoopGeneratedQuestionPersister};
use llm::{LLMClient, Message};
use serde_json::Value;
use std::collections::BTreeSet;

const SOCRATIC_SYSTEM_PROMPT: &str = "You are quizdom's Socratic belief-exploration engine. There are no correct answers. Explore and challenge the user's beliefs, probe semantic nuance, and prefer formal or shared definitions before bespoke meanings. Decide whether to select an existing follow-up question or generate one new concise follow-up question.";

// trace:STORY-161 | ai:claude
/// System prompt for DEBATE mode (`--mode debate`): instead of neutrally
/// challenging the user's own position, the questioner explicitly STEELMANS the
/// OPPOSING side and presses the strongest two-sided case for it. It remains
/// STRICTLY belief-neutral on TRUTH — it argues the opposing side's CRAFT to
/// stress-test the user, never asserting that side's belief is actually true.
const DEBATE_SYSTEM_PROMPT: &str = "You are quizdom's DEBATE-mode questioner. There are no correct answers and you are STRICTLY belief-neutral on which belief is TRUE. Your job is to STEELMAN the OPPOSING position to the user's: build and press the STRONGEST, most charitable version of the case AGAINST the user's stated view, so they must argue two-sided. Generate or select the follow-up question that best advances the opposing case — surface its best evidence, its sharpest objection to the user, the consideration their view has not yet answered. You are arguing the opposing side's CRAFT (to test the user), NEVER asserting that the opposing belief is actually true and NEVER advocating it as correct. Decide whether to select an existing follow-up question or generate one new concise follow-up question that advances the opposing case.";

// trace:STORY-161 | ai:claude
/// The two session MODES (the EPIC-158 toggle). `Socratic` (default) keeps the
/// questioner a NEUTRAL CHALLENGER of the user's own position; `Debate` makes it
/// STEELMAN the OPPOSING side two-sided. Belief-neutral throughout — neither mode
/// asserts which belief is true; debate mode judges argument CRAFT, not truth.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum SessionMode {
    /// Default: the questioner neutrally challenges the user's OWN position.
    #[default]
    Socratic,
    /// Opt-in: the questioner steelmans the OPPOSING position and debates it
    /// two-sided; the verdict judges which CASE was better-argued structurally.
    Debate,
}

impl SessionMode {
    /// The wire/CLI token for this mode (`socratic` / `debate`), used for the
    /// `--mode` flag, the session log, and resume restore.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Socratic => "socratic",
            Self::Debate => "debate",
        }
    }

    /// Parse a `--mode` value (or a logged mode token) into a [`SessionMode`].
    /// Returns `None` for an unrecognized token so the caller can report a usage
    /// error (CLI) or fall back to the default (log restore).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "socratic" | "neutral" => Some(Self::Socratic),
            "debate" => Some(Self::Debate),
            _ => None,
        }
    }
}

// trace:STORY-188 | ai:claude
/// The result of ONE interrogator turn-call (ADR-187): the next question PLUS the
/// two META by-products — an optional STRUCTURAL objection and an optional
/// goal-offer — that the LLM strategy now returns as a single structured envelope
/// instead of the session loop spawning a separate full-history `claude -p` probe
/// for each per turn.
///
/// Belief-NEUTRAL by contract: `objection` names a STRUCTURAL tension (an
/// inconsistency / unmet burden / ambiguity), never a counter-belief; `goal_offer`
/// names the QUESTION the exploration is circling, never a belief to adopt. The
/// session loop applies the SURFACING rules (free-flow + one-shot guards) over
/// these fields — they say only "the model SAW a tension / a crystallized thesis",
/// not "surface it now".
///
/// Non-LLM strategies (deterministic / offline) leave `objection` and `goal_offer`
/// `None`, so they make NO extra calls and degrade exactly as before — the meta
/// by-products are a near-free addition to the call the LLM strategy already makes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TurnEnvelope {
    /// The selected / generated follow-up question, or `None` for a dead end (no
    /// begets successor) — identical to the legacy `next_question` return so the
    /// dead-end menu path is unchanged.
    pub next_question: Option<Question>,
    /// A STRUCTURAL objection the interrogator could raise this turn, if the model
    /// saw a genuine material unaddressed tension. `None` = nothing to object over.
    /// Belief-neutral; the session loop gates whether it is actually SURFACED.
    pub objection: Option<String>,
    /// A crystallized-thesis goal-offer, if the model judged one has emerged.
    /// `None` = nothing crystallized yet. Belief-neutral (a QUESTION to settle);
    /// the session loop gates whether it is actually SURFACED.
    pub goal_offer: Option<crate::observer::GoalProposal>,
}

pub trait NextQuestionStrategy {
    fn next_question(
        &self,
        current: &Question,
        context: &StrategyContext,
        bank: &dyn QuestionBank,
    ) -> Result<Option<Question>>;

    // trace:STORY-188 | ai:claude
    /// Compute the whole turn ENVELOPE in one shot (ADR-187): the next question
    /// plus the optional structural objection and goal-offer META by-products.
    ///
    /// The default implementation simply wraps [`next_question`] with `objection =
    /// None` and `goal_offer = None`, so deterministic / offline / non-LLM
    /// strategies make NO extra calls and surface no objection/goal-offer — exactly
    /// as today. The LLM strategy overrides this to request all three fields in the
    /// SINGLE structured-output call it already makes with the full history.
    fn next_turn(
        &self,
        current: &Question,
        context: &StrategyContext,
        bank: &dyn QuestionBank,
    ) -> Result<TurnEnvelope> {
        Ok(TurnEnvelope {
            next_question: self.next_question(current, context, bank)?,
            objection: None,
            goal_offer: None,
        })
    }

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

    // trace:STORY-86 | ai:claude
    /// REFINE step of the approve flow: critique and improve a user-authored
    /// question before it is persisted.
    ///
    /// Returns a [`RefinementProposal`] the user can approve (adopt the refined
    /// wording / answer shape) or reject (keep their own). The default returns
    /// `None` — no refinement available — so non-LLM strategies and the offline
    /// path add the question verbatim.
    fn refine_user_question(
        &self,
        _title: &str,
        _answer_kind: &AnswerKind,
    ) -> Result<Option<RefinementProposal>> {
        Ok(None)
    }
}

// trace:STORY-86 | ai:claude
/// The outcome of the LLM-assisted pre-persistence pass over a user-authored
/// question: the two approve-flow steps (DEDUP then REFINE) collapsed into one
/// decision for the caller to act on.
///
/// `Duplicate` short-circuits persistence — the user is offered the existing
/// bank question to reuse / link. Otherwise `Refinement` carries the LLM's
/// proposal for the user to approve, and `Verbatim` means no duplicate and no
/// refinement (e.g. the offline path), so the question is added as written.
#[derive(Debug, Clone, PartialEq)]
pub enum UserQuestionAssist {
    /// A near-duplicate exists in the bank; offer it for reuse / linking.
    Duplicate(NearDuplicate),
    /// The LLM proposed an improvement for the user to approve or reject.
    Refinement(RefinementProposal),
    /// No duplicate and no refinement — persist the question verbatim.
    Verbatim,
}

// trace:STORY-86 | ai:claude
/// Run the two-step LLM-assisted approve flow for a user-authored question.
///
/// 1. **DEDUP** — search `bank` for a near-duplicate; if one clears
///    [`DEDUP_SIMILARITY_THRESHOLD`] the result short-circuits to
///    [`UserQuestionAssist::Duplicate`], offering reuse over a rephrasing.
/// 2. **REFINE** — otherwise ask `strategy` to critique the phrasing; a
///    proposal becomes [`UserQuestionAssist::Refinement`] for the user to
///    approve.
///
/// Degrades gracefully offline: the dedup search is pure (always runs), and a
/// failing / absent LLM yields no refinement, so the flow falls through to
/// [`UserQuestionAssist::Verbatim`] and the caller adds the question as
/// written.
pub fn assist_user_question(
    strategy: &dyn NextQuestionStrategy,
    title: &str,
    answer_kind: &AnswerKind,
    bank: &[Question],
) -> UserQuestionAssist {
    if let Some(duplicate) = find_near_duplicate(title, bank, DEDUP_SIMILARITY_THRESHOLD) {
        return UserQuestionAssist::Duplicate(duplicate);
    }
    match strategy.refine_user_question(title, answer_kind) {
        Ok(Some(proposal)) => UserQuestionAssist::Refinement(proposal),
        // Offline / error / no-op strategy: add the question verbatim.
        Ok(None) | Err(_) => UserQuestionAssist::Verbatim,
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct StrategyContext {
    pub answer: Answer,
    pub recent_path: Vec<AnsweredQuestion>,
    // trace:STORY-159 | ai:claude
    /// The session GOAL/thesis the exploration is trying to resolve, if one has
    /// been set (via `--goal`, an in-session command, or an Observer proposal).
    /// When present it ORIENTS the LLM next-question prompt — questions aim at
    /// resolving the goal. `None` means free-flowing (no goal set yet), and the
    /// prompt is unchanged. Belief-neutral: the goal is the question being
    /// settled, never a belief to advocate.
    pub goal: Option<String>,
    // trace:STORY-161 | ai:claude
    /// The session MODE (the EPIC-158 toggle). `Socratic` (default) keeps the
    /// next-question prompt a neutral challenge of the user's OWN position;
    /// `Debate` switches the prompt + system prompt to STEELMAN the OPPOSING
    /// side. Belief-neutral throughout: debate mode argues the opposing case's
    /// CRAFT, never asserts the opposing belief is true.
    pub mode: SessionMode,
    // trace:STORY-175 | ai:claude
    /// The OPEN OBJECTION the exchange is PINNED on, if one is active. While set it
    /// NARROWS the next-question prompt to the contested point (a mini-goal that
    /// reuses the STORY-159 goal-narrow path), taking PRIORITY over the session goal
    /// so questions probe the objection until it is `/resolved` or `/judge`-d.
    /// `None` = no pin (normal flow). Belief-neutral: the objection is a STRUCTURAL
    /// tension to probe, never a belief to advocate.
    pub objection: Option<String>,
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

// trace:STORY-67 | ai:claude
/// The randomness seam for weighted-probabilistic selection. Implementors
/// return a value in `[0, total)`; injecting a fixed sampler makes selection
/// fully deterministic under test.
pub trait WeightSampler {
    /// A value in `[0, total)`. `total` is guaranteed to be greater than zero.
    fn roll(&self, total: u64) -> u64;
}

// trace:STORY-67 | ai:claude
/// Map a `roll` in `[0, sum(weights))` to the index whose proportional slice of
/// the line it lands in. Each entry occupies a slice as wide as its weight, so
/// the chance of an index is `weight[i] / sum(weights)`. Returns `None` only
/// when every weight is zero (an empty or all-excluded candidate set).
///
/// Pure and total — the deterministic core of weighted sampling. The roll is
/// reduced modulo the total as a defensive guard so an out-of-range sampler can
/// never panic or fall off the end.
fn weighted_index(weights: &[u32], roll: u64) -> Option<usize> {
    let total: u64 = weights.iter().map(|&weight| u64::from(weight)).sum();
    if total == 0 {
        return None;
    }
    let target = roll % total;
    let mut cumulative = 0u64;
    for (index, &weight) in weights.iter().enumerate() {
        cumulative += u64::from(weight);
        if target < cumulative {
            return Some(index);
        }
    }
    // Unreachable while total > 0, but stay total rather than panicking.
    weights.iter().rposition(|&weight| weight > 0)
}

// trace:STORY-67 | ai:claude
/// A small, dependency-free xorshift64 sampler seeded from the wall clock.
/// Good enough to spread successor selection across a session; not for
/// cryptographic use.
pub struct XorShiftWeightSampler {
    state: std::cell::Cell<u64>,
}

impl XorShiftWeightSampler {
    /// Seed explicitly — used by tests that want a reproducible sequence.
    pub fn with_seed(seed: u64) -> Self {
        // A zero seed makes xorshift degenerate; nudge it off zero.
        Self {
            state: std::cell::Cell::new(if seed == 0 { 0x9E3779B97F4A7C15 } else { seed }),
        }
    }

    /// Seed from the current time so successive sessions diverge.
    pub fn from_entropy() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15);
        Self::with_seed(seed)
    }

    fn next_u64(&self) -> u64 {
        let mut x = self.state.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state.set(x);
        x
    }
}

impl Default for XorShiftWeightSampler {
    fn default() -> Self {
        Self::from_entropy()
    }
}

impl WeightSampler for XorShiftWeightSampler {
    fn roll(&self, total: u64) -> u64 {
        self.next_u64() % total
    }
}

// trace:STORY-67 | ai:claude
/// Selects the next question by sampling eligible `begets`-successors in
/// proportion to their weight, so heavier questions surface more often while
/// lighter ones still get a turn and variety emerges (STORY-67). `weight:0`
/// successors are never selected, and STORY-48 from-answer filtering is honored
/// first (only the highest relevance tier participates). The `WeightSampler`
/// seam keeps the choice deterministic under test.
pub struct WeightedNextQuestionStrategy<S = XorShiftWeightSampler> {
    sampler: S,
}

impl WeightedNextQuestionStrategy {
    /// A strategy seeded from the wall clock.
    pub fn from_entropy() -> Self {
        Self {
            sampler: XorShiftWeightSampler::from_entropy(),
        }
    }
}

impl<S> WeightedNextQuestionStrategy<S> {
    /// Inject a sampler — the deterministic test seam.
    pub fn with_sampler(sampler: S) -> Self {
        Self { sampler }
    }
}

impl Default for WeightedNextQuestionStrategy {
    fn default() -> Self {
        Self::from_entropy()
    }
}

impl<S> NextQuestionStrategy for WeightedNextQuestionStrategy<S>
where
    S: WeightSampler,
{
    fn next_question(
        &self,
        current: &Question,
        context: &StrategyContext,
        bank: &dyn QuestionBank,
    ) -> Result<Option<Question>> {
        let eligible = eligible_for_sampling(current, &context.answer, bank)?;
        let weights = eligible
            .iter()
            .map(|question| question.weight)
            .collect::<Vec<_>>();
        let total: u64 = weights.iter().map(|&weight| u64::from(weight)).sum();
        if total == 0 {
            return Ok(None);
        }
        let roll = self.sampler.roll(total);
        Ok(weighted_index(&weights, roll).map(|index| eligible[index].clone()))
    }
}

fn successor_questions(current: &Question, bank: &dyn QuestionBank) -> Result<Vec<Question>> {
    bank.begets(&current.id)?
        .into_iter()
        .map(|question_ref| bank.load_question(&question_ref.id))
        .collect()
}

// trace:STORY-53 | ai:codex
/// Select a punt target outside the current question's direct `begets` thread
/// and outside the current topic. Ordered by weight then id to keep curation
/// jumps deterministic while preferring stronger bank questions.
pub(crate) fn different_topic_punt_question(
    current: &Question,
    recent_path: &[AnsweredQuestion],
    bank: &dyn QuestionBank,
) -> Result<Option<Question>> {
    let current_topic = question_topic(current);
    let thread_ids = bank
        .begets(&current.id)?
        .into_iter()
        .map(|question_ref| question_ref.id)
        .collect::<BTreeSet<_>>();
    let answered_ids = recent_path
        .iter()
        .map(|answer| answer.question_ref.clone())
        .collect::<BTreeSet<_>>();
    let mut candidates = bank
        .all_questions()?
        .into_iter()
        .filter(|question| question.id != current.id)
        .filter(|question| !thread_ids.contains(&question.id))
        .filter(|question| !answered_ids.contains(&question.id))
        .filter(|question| question_topic(question) != current_topic)
        .filter(|question| question.weight > 0)
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .weight
            .cmp(&left.weight)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(candidates.into_iter().next())
}

fn question_topic(question: &Question) -> Option<&str> {
    question
        .tags
        .iter()
        .find_map(|tag| tag.strip_prefix("topic:"))
        .map(str::trim)
        .filter(|topic| !topic.is_empty())
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

/// Successors eligible for the current answer paired with their STORY-48
/// relevance score, ordered by relevance, then weight, then id. Successors
/// conditioned on a different answer are dropped so different answers branch to
/// different follow-ups (STORY-48).
fn scored_successors(
    current: &Question,
    answer: &Answer,
    bank: &dyn QuestionBank,
) -> Result<Vec<(u8, Question)>> {
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
    Ok(successors)
}

/// Successors eligible for the current answer, ordered by answer relevance,
/// then weight, then id. Successors conditioned on a different answer are
/// dropped so different answers branch to different follow-ups (STORY-48).
fn relevant_successors(
    current: &Question,
    answer: &Answer,
    bank: &dyn QuestionBank,
) -> Result<Vec<Question>> {
    Ok(scored_successors(current, answer, bank)?
        .into_iter()
        .map(|(_, question)| question)
        .collect())
}

// trace:STORY-67 | ai:claude
/// Successors eligible for *weighted-probabilistic* selection: the highest
/// available STORY-48 relevance tier only — so answer-conditioned branches keep
/// strict precedence over unconditional follow-ons — with every `weight:0`
/// successor excluded, since a zero-weight question is never auto-selected.
/// Ordered by id so a given roll maps to a stable choice.
fn eligible_for_sampling(
    current: &Question,
    answer: &Answer,
    bank: &dyn QuestionBank,
) -> Result<Vec<Question>> {
    let scored = scored_successors(current, answer, bank)?;
    let Some(top_relevance) = scored.first().map(|(relevance, _)| *relevance) else {
        return Ok(Vec::new());
    };
    let mut eligible = scored
        .into_iter()
        .take_while(|(relevance, _)| *relevance == top_relevance)
        .map(|(_, question)| question)
        .filter(|question| question.weight > 0)
        .collect::<Vec<_>>();
    eligible.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(eligible)
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
    let mut prompt = String::new();
    // trace:STORY-161 | ai:claude
    // In DEBATE mode, lead the prompt by naming the steelman stance so the model
    // builds the OPPOSING case rather than neutrally challenging the user's own
    // view. Belief-neutral: it presses the strongest version of the other side to
    // stress-test the user, never asserting the opposing belief is actually true.
    if context.mode == SessionMode::Debate {
        prompt.push_str(
            "DEBATE MODE: steelman the OPPOSING position to the user's. Pick or generate the follow-up that best advances the STRONGEST case AGAINST the user's stated view — its best evidence, its sharpest objection — so they must argue two-sided. Stay belief-neutral on truth: argue the opposing side's CRAFT to test the user, never assert the opposing belief is actually true.\n\n",
        );
    }
    // trace:STORY-175 | ai:claude
    // An OPEN OBJECTION PINS the exchange: narrow the next question to the contested
    // point so the questioner probes it (a mini-goal). It leads the prompt and takes
    // PRIORITY over the session goal — while pinned, every follow-up presses the
    // objection until it is resolved or judged. Belief-neutral: the objection is a
    // STRUCTURAL tension to probe, never a belief to advocate.
    if let Some(objection) = context
        .objection
        .as_deref()
        .map(str::trim)
        .filter(|o| !o.is_empty())
    {
        prompt.push_str(&format!(
            "OPEN OBJECTION (the exchange is PINNED on this contested point): {objection}\nNarrow the next question to this objection — pick or generate the follow-up that best PROBES the contested point so it can be settled. This takes priority over any session goal until the objection is resolved. Stay belief-neutral: probe the structural tension, never advocate which answer is true.\n\n"
        ));
    }
    // trace:STORY-159 | ai:claude
    // When a session GOAL/thesis is set, lead the prompt with it so the model
    // ORIENTS its selection toward RESOLVING the goal — picking or generating the
    // follow-up that best moves the user toward settling it. Belief-neutral: the
    // goal frames WHICH QUESTION is being settled, never which answer is right.
    if let Some(goal) = context
        .goal
        .as_deref()
        .map(str::trim)
        .filter(|g| !g.is_empty())
    {
        prompt.push_str(&format!(
            "Session goal (the claim/question being resolved): {goal}\nOrient the next question toward resolving this goal — pick or generate the follow-up that best moves the exploration toward settling it. Stay belief-neutral: aim at resolving the question, never advocate which answer is true.\n\n"
        ));
    }
    prompt.push_str(&format!(
        "Current question ({id}): {title}\nAnswer mode: {mode}\nUser raw answer: {raw}\nUser normalized answer: {normalized}\n\nRecent path:\n",
        id = current.id,
        title = current.title,
        mode = current.answer_kind.mode(),
        raw = context.answer.raw,
        normalized = context.answer.normalized,
    ));
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
    // trace:STORY-188 | ai:claude — request the whole TURN ENVELOPE in this one
    // structured-output call (ADR-187): the next-question decision PLUS the two
    // belief-neutral META by-products (a structural objection, a crystallized
    // goal-offer) the session used to spawn a separate full-history probe for each
    // per turn. Both meta fields default to null — the model includes them only
    // when a GENUINE, material, still-unaddressed tension / a single clearly
    // crystallized thesis exists, mirroring the old rare-by-design probes. They are
    // belief-neutral: the objection names a STRUCTURAL tension (never a
    // counter-belief); the goal is the QUESTION being settled (never a belief to
    // adopt). The session loop decides whether to SURFACE them (free-flow +
    // one-shot guards).
    prompt.push_str(
        "\nReturn only JSON with these fields:\n\
         - the next-question decision: to select a bank question use \"action\":\"select\",\"id\":\"Q-...\"; to generate one use \"action\":\"generate\",\"question\":\"...\",\"answer_mode\":\"yes-no|free-text\".\n\
         - \"objection\": null, OR — RARELY, only when a GENUINE, MATERIAL, still-UNADDRESSED structural tension exists in the user's positions — a string naming that single tension, phrased belief-neutrally as a STRUCTURAL challenge (an inconsistency / unmet burden / ambiguity), NEVER a counter-belief and NEVER an assertion the user's belief is false. When in doubt, use null.\n\
         - \"goal_offer\": null, OR — only when a SINGLE THESIS has clearly crystallized (one underlying claim/question the whole exploration is circling) — an object {\"goal\":\"the thesis as a belief-neutral QUESTION to settle\",\"rationale\":\"short neutral reason\"}. The goal MUST be phrased as the QUESTION being resolved, never a belief to adopt, and you must NOT assert which answer is correct. When no single thesis has crystallized, use null.\n\
         Example: {\"action\":\"select\",\"id\":\"Q-12\",\"objection\":null,\"goal_offer\":null}.",
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

// trace:STORY-188 | ai:claude
/// Parse the belief-neutral META by-products out of the turn-envelope JSON: the
/// optional structural `objection` and the optional crystallized `goal_offer`.
/// Both are conservative — a missing, null, blank, or malformed field yields
/// `None`, so the model never has a tension / goal fabricated for it (the same
/// "rare by design, decline by default" posture as the old separate probes).
fn parse_envelope_meta(value: &Value) -> (Option<String>, Option<crate::observer::GoalProposal>) {
    let objection = value
        .get("objection")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|o| !o.is_empty())
        .map(str::to_string);
    let goal_offer = value.get("goal_offer").and_then(|offer| {
        let goal = offer
            .get("goal")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|goal| !goal.is_empty())?
            .to_string();
        let rationale = offer
            .get("rationale")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        Some(crate::observer::GoalProposal { goal, rationale })
    });
    (objection, goal_offer)
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

    // trace:STORY-188 | ai:claude
    /// ONE structured-output call returns the whole turn ENVELOPE (ADR-187): the
    /// next-question decision PLUS the belief-neutral objection / goal-offer META
    /// by-products. On a failing / malformed call it degrades to the deterministic
    /// next question with NO objection and NO goal-offer — exactly the offline
    /// posture (no fabricated meta), and the same fallback `next_question` uses.
    fn next_turn(
        &self,
        current: &Question,
        context: &StrategyContext,
        bank: &dyn QuestionBank,
    ) -> Result<TurnEnvelope> {
        match self.llm_next_turn(current, context, bank) {
            Ok(envelope) => Ok(envelope),
            Err(_) => Ok(TurnEnvelope {
                next_question: self.deterministic.next_question(current, context, bank)?,
                objection: None,
                goal_offer: None,
            }),
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

    // trace:STORY-86 | ai:claude
    /// REFINE step: ask the model to improve the user's phrasing. A failing
    /// call (offline, provider error) degrades to `None` so the caller adds the
    /// question verbatim.
    fn refine_user_question(
        &self,
        title: &str,
        answer_kind: &AnswerKind,
    ) -> Result<Option<RefinementProposal>> {
        self.llm_refine_user_question(title, answer_kind)
            .or(Ok(None))
    }
}

impl<C, P> LlmNextQuestionStrategy<C, P>
where
    C: LLMClient,
    P: GeneratedQuestionPersister,
{
    // trace:STORY-188 | ai:claude — `llm_next_question` is now a thin projection of
    // `llm_next_turn`: the question half of the one envelope call. Kept so the
    // dead-end menu / `next_question` callers that don't need the META by-products
    // are unchanged.
    fn llm_next_question(
        &self,
        current: &Question,
        context: &StrategyContext,
        bank: &dyn QuestionBank,
    ) -> Result<Option<Question>> {
        Ok(self.llm_next_turn(current, context, bank)?.next_question)
    }

    // trace:STORY-188 | ai:claude
    /// The single per-turn LLM call (ADR-187): build the prompt from the history we
    /// ALREADY send, request the structured ENVELOPE, and parse the next-question
    /// decision PLUS the belief-neutral objection / goal-offer by-products from the
    /// SAME response. This replaces the old design where the session loop spawned a
    /// separate full-history probe for the goal-offer and the objection EVERY turn.
    fn llm_next_turn(
        &self,
        current: &Question,
        context: &StrategyContext,
        bank: &dyn QuestionBank,
    ) -> Result<TurnEnvelope> {
        let candidates = relevant_successors(current, &context.answer, bank).unwrap_or_default();
        let prompt = strategy_prompt(current, context, &candidates);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(QuizdomError::Io)?;
        // trace:BUG-100 | ai:claude
        // The 'thinking' spinner is no longer started here. STORY-83 scoped it
        // to just this LLM `block_on`, which left visible frozen gaps for the
        // surrounding AIDA shell-outs: candidate gathering (above) before the
        // call, and persistence (below) after it. The session loop now holds a
        // single spinner across the whole `next_question` computation so the
        // indicator spans the entire delay between answer and next question.
        // trace:STORY-161 | ai:claude — debate mode swaps in the steelman system
        // prompt so the questioner argues the OPPOSING side; default stays
        // Socratic-neutral-challenger.
        let system_prompt = match context.mode {
            SessionMode::Socratic => SOCRATIC_SYSTEM_PROMPT,
            SessionMode::Debate => DEBATE_SYSTEM_PROMPT,
        };
        let (text, _tool_calls) = runtime
            .block_on(
                self.client
                    .call(system_prompt, &[Message::user(prompt)], &[]),
            )
            .map_err(|error| QuizdomError::Aida(error.to_string()))?;
        // Parse the next-question decision and the META by-products from the SAME
        // structured response (one call, three fields).
        let value: Value = serde_json::from_str(text.trim())
            .map_err(|error| QuizdomError::Parse(format!("invalid LLM strategy JSON: {error}")))?;
        let (objection, goal_offer) = parse_envelope_meta(&value);
        let next = apply_llm_decision(&text, &candidates)?;
        let next_question = match next {
            Some(question) if question.id == "generated:llm" => self
                .generated_question_persister
                // trace:STORY-48 | ai:claude
                .persist_generated_question(
                    current,
                    &question,
                    triggering_answer(current, context).as_deref(),
                )
                .map(Some)?,
            other => other,
        };
        Ok(TurnEnvelope {
            next_question,
            objection,
            goal_offer,
        })
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

    // trace:STORY-86 | ai:claude
    fn llm_refine_user_question(
        &self,
        title: &str,
        answer_kind: &AnswerKind,
    ) -> Result<Option<RefinementProposal>> {
        let prompt = refine_question_prompt(title, answer_kind);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(QuizdomError::Io)?;
        let (text, _tool_calls) = {
            let _spinner = crate::spinner::Spinner::start("refining");
            runtime.block_on(
                self.client
                    .call(REFINE_SYSTEM_PROMPT, &[Message::user(prompt)], &[]),
            )
        }
        .map_err(|error| QuizdomError::Aida(error.to_string()))?;
        parse_refinement(&text, title, answer_kind)
    }
}

// trace:STORY-86 | ai:claude
const REFINE_SYSTEM_PROMPT: &str = "You are quizdom's Socratic question editor. Critique a user-authored belief-exploration question and improve its phrasing without changing its intent. Prefer open, non-leading wording that invites the user to examine a belief; flag questions that are leading, purely factual, or answerable with a single fact as weak-Socratic. Suggest the answer shape (yes-no, free-text, or choice) that best fits the refined question.";

// trace:STORY-86 | ai:claude
fn refine_question_prompt(title: &str, answer_kind: &AnswerKind) -> String {
    format!(
        "User-authored question: {title}\nProposed answer mode: {mode}\n\nReturn only JSON: {{\"refined\":\"improved phrasing\",\"answer_mode\":\"yes-no|free-text|choice[a,b]\",\"weak_socratic\":true|false,\"rationale\":\"short reason\"}}. Keep the user's intent; improve clarity and Socratic openness.",
        mode = answer_kind.mode(),
    )
}

// trace:STORY-86 | ai:claude
/// Parse the REFINE step's JSON into a [`RefinementProposal`].
///
/// Missing fields fall back to the user's own values: an absent or blank
/// `refined` keeps `title`, an unrecognized `answer_mode` keeps the proposed
/// `answer_kind`. Returns `None` when the proposal is a no-op — same wording,
/// same shape, and not flagged weak — so the caller adds the question verbatim
/// rather than prompting the user to approve an identical rewrite.
pub(crate) fn parse_refinement(
    text: &str,
    title: &str,
    answer_kind: &AnswerKind,
) -> Result<Option<RefinementProposal>> {
    let value: Value = serde_json::from_str(text.trim())
        .map_err(|error| QuizdomError::Parse(format!("invalid refinement JSON: {error}")))?;
    let refined_title = value
        .get("refined")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|refined| !refined.is_empty())
        .unwrap_or(title)
        .to_string();
    let suggested_answer_kind = value
        .get("answer_mode")
        .and_then(Value::as_str)
        .and_then(parse_answer_mode)
        .unwrap_or_else(|| answer_kind.clone());
    let weak_socratic = value
        .get("weak_socratic")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let rationale = value
        .get("rationale")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    // A no-op proposal (nothing changed, no warning) is not worth surfacing.
    if refined_title == title && &suggested_answer_kind == answer_kind && !weak_socratic {
        return Ok(None);
    }
    Ok(Some(RefinementProposal {
        refined_title,
        suggested_answer_kind,
        weak_socratic,
        rationale,
    }))
}

// trace:STORY-86 | ai:claude
/// Parse an `answer_mode` string (`yes-no`, `free-text`, or `choice[a,b]`) into
/// an [`AnswerKind`], reusing the tag parser for the bracketed choice form.
fn parse_answer_mode(mode: &str) -> Option<AnswerKind> {
    match mode.trim() {
        "yes-no" => Some(AnswerKind::YesNo),
        "free-text" => Some(AnswerKind::FreeText),
        other if other.starts_with("choice[") => {
            answer_kind_from_tags(&[format!("answer:{other}")])
        }
        _ => None,
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

// trace:STORY-67 | ai:claude
#[cfg(test)]
mod weighted_index_tests {
    use super::{weighted_index, WeightSampler, XorShiftWeightSampler};

    #[test]
    fn empty_or_all_zero_weights_select_nothing() {
        assert_eq!(weighted_index(&[], 0), None);
        assert_eq!(weighted_index(&[0, 0, 0], 7), None);
    }

    #[test]
    fn each_roll_lands_in_its_proportional_slice() {
        // Weights 3 and 1 partition [0, 4): rolls 0..3 -> index 0, roll 3 -> index 1.
        let weights = [3, 1];
        assert_eq!(weighted_index(&weights, 0), Some(0));
        assert_eq!(weighted_index(&weights, 1), Some(0));
        assert_eq!(weighted_index(&weights, 2), Some(0));
        assert_eq!(weighted_index(&weights, 3), Some(1));
    }

    #[test]
    fn sweeping_every_roll_matches_the_weight_distribution() {
        let weights = [5u32, 2, 1];
        let total: u64 = weights.iter().map(|&weight| u64::from(weight)).sum();
        let mut counts = [0u32; 3];
        for roll in 0..total {
            let index = weighted_index(&weights, roll).unwrap();
            counts[index] += 1;
        }
        // Each index is chosen exactly as many times as its weight.
        assert_eq!(counts, weights);
    }

    #[test]
    fn out_of_range_roll_is_reduced_modulo_total() {
        // A sampler that over-shoots must still resolve to a real index.
        assert_eq!(weighted_index(&[3, 1], 4), Some(0));
        assert_eq!(weighted_index(&[3, 1], 7), Some(1));
    }

    #[test]
    fn seeded_sampler_is_reproducible_and_in_range() {
        let total = 10;
        let first = XorShiftWeightSampler::with_seed(42);
        let second = XorShiftWeightSampler::with_seed(42);
        for _ in 0..32 {
            let a = first.roll(total);
            let b = second.roll(total);
            assert_eq!(a, b);
            assert!(a < total);
        }
    }
}

// trace:STORY-159 | ai:claude
#[cfg(test)]
mod goal_orientation_tests {
    use super::{strategy_prompt, AnsweredQuestion, SessionMode, StrategyContext};
    use crate::model::{Answer, AnswerKind, Question};

    fn question() -> Question {
        Question {
            id: "Q-1".to_string(),
            title: "Is free will real?".to_string(),
            tags: vec!["topic:free-will".to_string()],
            answer_kind: AnswerKind::YesNo,
            weight: 50,
        }
    }

    fn context(goal: Option<&str>) -> StrategyContext {
        StrategyContext {
            answer: Answer {
                raw: "yes".to_string(),
                normalized: "yes".to_string(),
            },
            recent_path: Vec::<AnsweredQuestion>::new(),
            goal: goal.map(str::to_string),
            mode: SessionMode::Socratic,
            // trace:STORY-175 | ai:claude — no pinned objection by default.
            objection: None,
        }
    }

    // trace:STORY-161 | ai:claude
    fn debate_context(goal: Option<&str>) -> StrategyContext {
        StrategyContext {
            mode: SessionMode::Debate,
            ..context(goal)
        }
    }

    #[test]
    fn prompt_orients_toward_the_goal_when_set() {
        // When a goal is set, the next-question prompt names it and asks the model
        // to ORIENT selection toward resolving it — so questions aim at the goal.
        let goal = "can libertarian free will be held consistently?";
        let prompt = strategy_prompt(&question(), &context(Some(goal)), &[]);
        assert!(prompt.contains(goal), "the goal text must appear: {prompt}");
        assert!(
            prompt.contains("Orient the next question toward resolving this goal"),
            "the prompt must instruct orientation: {prompt}"
        );
        // Belief-neutral: it aims at resolving the QUESTION, never at a belief.
        assert!(prompt.to_lowercase().contains("belief-neutral"));
        assert!(!prompt.to_lowercase().contains("which answer is true\","));
    }

    #[test]
    fn an_open_objection_narrows_the_prompt_and_takes_priority_over_the_goal() {
        // trace:STORY-175 | ai:claude — while an objection is pinned, the next-question
        // prompt narrows to the contested point and that preamble leads (priority over
        // the goal), so questions probe the objection until it is resolved/judged.
        let mut ctx = context(Some("can free will survive causation?"));
        ctx.objection = Some("you never defined what 'free' means".to_string());
        let prompt = strategy_prompt(&question(), &ctx, &[]);
        assert!(
            prompt.contains("OPEN OBJECTION"),
            "must name the pin: {prompt}"
        );
        assert!(prompt.contains("you never defined what 'free' means"));
        assert!(prompt.contains("Narrow the next question to this objection"));
        // The objection preamble PRECEDES the goal preamble (priority).
        let obj_at = prompt.find("OPEN OBJECTION").unwrap();
        let goal_at = prompt.find("Session goal").unwrap();
        assert!(obj_at < goal_at, "objection must lead the goal: {prompt}");
        assert!(prompt.to_lowercase().contains("belief-neutral"));
    }

    #[test]
    fn no_objection_leaves_the_prompt_unnarrowed() {
        // trace:STORY-175 | ai:claude — no pin → no objection preamble.
        let prompt = strategy_prompt(&question(), &context(None), &[]);
        assert!(!prompt.contains("OPEN OBJECTION"));
    }

    #[test]
    fn prompt_is_free_flowing_when_no_goal_is_set() {
        // No goal → the prompt carries no goal/orientation preamble (free-flowing).
        let prompt = strategy_prompt(&question(), &context(None), &[]);
        assert!(!prompt.contains("Session goal"));
        assert!(!prompt.contains("Orient the next question"));
    }

    #[test]
    fn a_blank_goal_does_not_orient() {
        // A whitespace-only goal is treated as no goal — no orientation preamble.
        let prompt = strategy_prompt(&question(), &context(Some("   ")), &[]);
        assert!(!prompt.contains("Session goal"));
    }

    // trace:STORY-161 | ai:claude
    #[test]
    fn debate_mode_prompt_steelmans_the_opposing_side() {
        // Debate mode leads the prompt with the steelman stance so the model
        // argues the OPPOSING case, not a neutral challenge of the user's view.
        let prompt = strategy_prompt(&question(), &debate_context(None), &[]);
        assert!(
            prompt.contains("DEBATE MODE"),
            "debate prompt must announce the mode: {prompt}"
        );
        assert!(
            prompt
                .to_lowercase()
                .contains("steelman the opposing position"),
            "debate prompt must instruct steelmanning the opposing side: {prompt}"
        );
        // Belief-neutral on TRUTH: it argues the opposing side's CRAFT to test
        // the user, it never asserts the opposing belief is actually true.
        assert!(prompt.to_lowercase().contains("belief-neutral"));
        assert!(
            prompt.contains("never assert the opposing belief is actually true"),
            "debate prompt must stay belief-neutral on truth: {prompt}"
        );
    }

    // trace:STORY-161 | ai:claude
    #[test]
    fn default_mode_carries_no_debate_preamble() {
        // Default (Socratic) mode is unchanged — no steelman/debate preamble.
        let prompt = strategy_prompt(&question(), &context(None), &[]);
        assert!(!prompt.contains("DEBATE MODE"));
        assert!(!prompt.to_lowercase().contains("steelman the opposing"));
    }

    // trace:STORY-161 | ai:claude
    #[test]
    fn debate_mode_composes_with_a_goal() {
        // Debate + a goal: both preambles appear; the steelman leads, the goal
        // still orients. Belief-neutral throughout.
        let goal = "can compatibilist free will answer the consequence argument?";
        let prompt = strategy_prompt(&question(), &debate_context(Some(goal)), &[]);
        assert!(prompt.contains("DEBATE MODE"));
        assert!(prompt.contains(goal));
        assert!(prompt.contains("Orient the next question toward resolving this goal"));
    }
}

// trace:STORY-188 | ai:claude
#[cfg(test)]
mod turn_envelope_meta_tests {
    use super::parse_envelope_meta;
    use serde_json::json;

    #[test]
    fn parses_belief_neutral_objection_and_goal_offer() {
        // The fields carry the model's STRUCTURAL objection + crystallized goal
        // VERBATIM — belief-neutral text, not asserting which belief is true.
        let value = json!({
            "action": "select",
            "id": "Q-2",
            "objection": "you affirm both free will and full causation without reconciling them",
            "goal_offer": {
                "goal": "can libertarian free will be held consistently?",
                "rationale": "the arc keeps circling it"
            }
        });
        let (objection, goal_offer) = parse_envelope_meta(&value);
        assert_eq!(
            objection.as_deref(),
            Some("you affirm both free will and full causation without reconciling them")
        );
        let offer = goal_offer.expect("a crystallized goal");
        assert_eq!(
            offer.goal,
            "can libertarian free will be held consistently?"
        );
        assert_eq!(offer.rationale, "the arc keeps circling it");
    }

    #[test]
    fn null_missing_or_blank_meta_fields_yield_none() {
        // Decline by default: null, omitted, blank, or goal-less offers fabricate
        // nothing — the same rare-by-design posture as the old separate probes.
        let (objection, goal_offer) =
            parse_envelope_meta(&json!({"objection": null, "goal_offer": null}));
        assert!(objection.is_none() && goal_offer.is_none());

        let (objection, goal_offer) =
            parse_envelope_meta(&json!({"action": "select", "id": "Q-1"}));
        assert!(objection.is_none() && goal_offer.is_none());

        let (objection, _) = parse_envelope_meta(&json!({"objection": "   "}));
        assert!(objection.is_none(), "a blank objection is not a tension");

        let (_, goal_offer) = parse_envelope_meta(&json!({"goal_offer": {"goal": "  "}}));
        assert!(
            goal_offer.is_none(),
            "a blank goal is not a crystallized thesis"
        );
    }
}
