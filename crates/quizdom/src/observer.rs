// trace:STORY-127 | ai:claude
//! The Observer engine: a BELIEF-NEUTRAL, CLARIFY-ONLY reading of an in-session
//! exchange.
//!
//! Given the current question, the user's answer, and the rebuttal (the
//! follow-up challenge the session has put to that answer), the observer
//! produces a reading that:
//!
//! - translates the rebuttal into plainer terms,
//! - names the precise tension at play,
//! - diagnoses the answer-vs-question mismatch (what was *asked* vs what was
//!   *answered*), and
//! - lists the dimensions a precise answer would have to address,
//!
//! plus a short engagement read (clarity / consistency / did-you-meet-it).
//!
//! It is deliberately **belief-neutral** and **clarify-only**: it never supplies
//! the user's answer, never scaffolds belief-framings, and never judges which
//! belief is "right". It only helps the user see the question and their own
//! answer more clearly, then hands control straight back.
//!
//! The reading is produced by an LLM (default backend: claude-cli). When the LLM
//! is unavailable (offline, not logged in, malformed response), the engine
//! degrades to a minimal *structural* note derived purely from the exchange
//! text — no model, no belief content invented.

use crate::model::{Answer, Question};
use llm::{LLMClient, Message};
use serde_json::Value;

/// The three turns of an in-session exchange the observer reads.
///
/// `question` is what was asked, `answer` is what the user said, and `rebuttal`
/// is the challenge the session has put back to that answer (e.g. the follow-up
/// or contradiction prompt now on screen). The observer never mutates any of
/// these — it only reads them.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Exchange {
    /// The text of the question that was asked.
    pub question: String,
    /// The user's raw answer to that question.
    pub answer: String,
    /// The rebuttal / follow-up challenge now being put to the answer.
    pub rebuttal: String,
}

impl Exchange {
    /// Assemble an exchange from the live session pieces.
    ///
    /// `rebuttal` is the question now on screen (the follow-up that challenges
    /// the prior answer); `prior` is the question/answer pair that produced it.
    pub fn from_turn(
        prior_question: &Question,
        prior_answer: &Answer,
        rebuttal: &Question,
    ) -> Self {
        Self {
            question: prior_question.title.clone(),
            answer: prior_answer.raw.clone(),
            rebuttal: rebuttal.title.clone(),
        }
    }
}

/// A belief-neutral, clarify-only reading of an [`Exchange`].
///
/// Every field is descriptive, never prescriptive: it names structure and
/// tension without supplying an answer or asserting which belief is correct.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ExchangeReading {
    /// The rebuttal restated in plainer, jargon-free terms.
    pub plain_rebuttal: String,
    /// The precise tension the exchange turns on.
    pub tension: String,
    /// What was *asked* vs what was *answered* — the mismatch, if any.
    pub mismatch: String,
    /// The dimensions a precise answer would have to address (not the answer
    /// itself — only the axes it must cover).
    pub dimensions: Vec<String>,
    /// A short engagement read: clarity / consistency / did-you-meet-it.
    pub engagement: String,
    /// True when this reading was synthesized structurally (offline / degraded)
    /// rather than by the LLM.
    pub degraded: bool,
}

/// System prompt that pins the observer to its belief-neutral, clarify-only
/// contract. The guarantees here are mirrored by [`scrub_supplied_answer`] so a
/// model that ignores them still cannot leak an answer through.
const OBSERVER_SYSTEM_PROMPT: &str = "You are quizdom's exchange Observer. You are STRICTLY belief-neutral and clarify-only. Your job is to help the user SEE the exchange more clearly, never to answer it. You MUST NOT supply the user's answer, take a side, assert which belief is correct, or scaffold any belief-framing (do not say what one 'should' believe or which position is stronger). Only: restate the rebuttal in plainer terms, name the precise tension, diagnose the answer-vs-question mismatch, and list the dimensions a precise answer must address. Stay descriptive, not prescriptive.";

/// Build the observer prompt for one [`Exchange`].
fn observer_prompt(exchange: &Exchange) -> String {
    format!(
        "Question asked: {question}\nUser's answer: {answer}\nRebuttal put to the answer: {rebuttal}\n\nReturn only JSON with these fields: {{\"plain_rebuttal\":\"the rebuttal in plainer terms\",\"tension\":\"the precise tension\",\"mismatch\":\"what was asked vs what was answered\",\"dimensions\":[\"axis a precise answer must address\"],\"engagement\":\"short read of clarity/consistency/whether the answer met the question\"}}. Do NOT supply an answer, take a side, or say which belief is right.",
        question = exchange.question,
        answer = exchange.answer,
        rebuttal = exchange.rebuttal,
    )
}

/// Read an [`Exchange`] with the supplied LLM client, degrading to a structural
/// note when the call fails or returns something unusable.
///
/// This is the engine STORY-128 (eXchange-reading network mode) builds on: the
/// shared LLM step plus the belief-neutral guarantee live here, so callers only
/// choose the backend and the rendering.
pub fn read_exchange<C: LLMClient>(client: &C, exchange: &Exchange) -> ExchangeReading {
    match llm_read_exchange(client, exchange) {
        Some(reading) => reading,
        // Offline / not-logged-in / malformed: fall back to the structural note
        // rather than failing the keypress mid-session.
        None => structural_reading(exchange),
    }
}

/// The LLM leg of [`read_exchange`]: run the call on a current-thread runtime,
/// parse the JSON, and enforce the no-answer-supplied guarantee. Returns `None`
/// on any failure so the caller degrades gracefully.
fn llm_read_exchange<C: LLMClient>(client: &C, exchange: &Exchange) -> Option<ExchangeReading> {
    let prompt = observer_prompt(exchange);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .ok()?;
    let (text, _tool_calls) = runtime
        .block_on(client.call(OBSERVER_SYSTEM_PROMPT, &[Message::user(prompt)], &[]))
        .ok()?;
    parse_reading(&text, exchange)
}

/// Parse the observer's JSON into an [`ExchangeReading`], enforcing the
/// belief-neutral / no-answer-supplied guarantee on every text field.
///
/// Returns `None` when the payload is not the expected JSON object so the caller
/// degrades to the structural note. A field that names the user's own answer
/// verbatim is scrubbed (see [`scrub_supplied_answer`]) rather than passed
/// through, so a misbehaving model can never leak the answer back to the user.
pub fn parse_reading(text: &str, exchange: &Exchange) -> Option<ExchangeReading> {
    let value: Value = serde_json::from_str(text.trim()).ok()?;
    if !value.is_object() {
        return None;
    }
    let field = |key: &str| -> String {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .map(|raw| scrub_supplied_answer(raw, &exchange.answer))
            .unwrap_or_default()
    };
    let dimensions = value
        .get("dimensions")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(|item| scrub_supplied_answer(item, &exchange.answer))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let plain_rebuttal = field("plain_rebuttal");
    let tension = field("tension");
    let mismatch = field("mismatch");
    let engagement = field("engagement");

    // A reading with no usable structural content is no better than the
    // structural note; let the caller degrade instead of rendering an empty box.
    if plain_rebuttal.is_empty()
        && tension.is_empty()
        && mismatch.is_empty()
        && engagement.is_empty()
        && dimensions.is_empty()
    {
        return None;
    }

    Some(ExchangeReading {
        plain_rebuttal,
        tension,
        mismatch,
        dimensions,
        engagement,
        degraded: false,
    })
}

/// The no-answer-supplied guarantee. The observer must never hand the user's own
/// answer back as if it were guidance, so if a field reproduces the answer
/// verbatim (case-insensitively) we replace it with a neutral placeholder rather
/// than echo a belief-laden answer. Empty answers are left untouched (nothing to
/// leak).
fn scrub_supplied_answer(field: &str, answer: &str) -> String {
    let answer = answer.trim();
    if answer.is_empty() {
        return field.to_string();
    }
    if field.trim().eq_ignore_ascii_case(answer) {
        return "(withheld: the observer does not supply your answer)".to_string();
    }
    field.to_string()
}

/// The offline / degraded reading: a minimal *structural* note derived purely
/// from the exchange text. It invents no belief content — it only restates the
/// shape of the exchange and points at the gap between question and answer, so
/// the `?` key still does something useful with no model available.
pub fn structural_reading(exchange: &Exchange) -> ExchangeReading {
    let asked = first_sentence(&exchange.question);
    let answered = if exchange.answer.trim().is_empty() {
        "no answer recorded yet".to_string()
    } else {
        "you gave an answer above".to_string()
    };
    ExchangeReading {
        plain_rebuttal: format!(
            "The follow-up presses on: {}",
            first_sentence(&exchange.rebuttal)
        ),
        tension: format!(
            "The exchange turns on whether your answer addresses what \"{asked}\" actually asks."
        ),
        mismatch: format!("Asked: \"{asked}\". Answered: {answered}."),
        dimensions: vec![
            "What the key terms in the question mean to you".to_string(),
            "Which part of the question your answer speaks to".to_string(),
            "What would have to be true for your answer to hold".to_string(),
        ],
        engagement: "Offline reading: re-read the question and check your answer covers each part."
            .to_string(),
        degraded: true,
    }
}

// trace:STORY-159 | ai:claude
/// The Observer's proposal that a thesis has crystallized and could become the
/// session GOAL. Belief-neutral: `goal` is the QUESTION/claim the exploration is
/// settling (e.g. "can libertarian free will be held consistently?"), never a
/// belief to adopt. The session offers it to the user, who accepts or declines —
/// the Observer proposes, it never imposes.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct GoalProposal {
    /// The proposed goal/thesis, phrased belief-neutrally as the question being
    /// resolved.
    pub goal: String,
    /// A short belief-neutral rationale for why this thesis seems to be the one
    /// the session is circling.
    pub rationale: String,
}

/// System prompt pinning the goal-proposal step to its belief-neutral contract.
/// The Observer reads the arc so far and, IF a single thesis has clearly
/// crystallized, names it as a QUESTION to settle — never a belief to advocate.
const GOAL_PROPOSAL_SYSTEM_PROMPT: &str = "You are quizdom's Observer proposing a session GOAL. You are STRICTLY belief-neutral. Read the positions the user has taken so far and decide whether a single THESIS has crystallized — one underlying claim or question the whole exploration is circling. If (and only if) one has, propose it as the session goal, phrased as the QUESTION being resolved (e.g. \"can libertarian free will be held consistently?\"), NEVER as a belief to adopt and NEVER asserting which answer is correct. If no single thesis has crystallized yet, decline. Stay descriptive: you propose the question, the user decides.";

/// Build the goal-proposal prompt from the positions taken so far.
fn goal_proposal_prompt(positions: &[String]) -> String {
    let mut prompt = String::from("Positions the user has taken so far:\n");
    for position in positions {
        prompt.push_str(&format!("- {position}\n"));
    }
    prompt.push_str(
        "\nReturn only JSON: {\"crystallized\":true|false,\"goal\":\"the thesis as a belief-neutral QUESTION to settle\",\"rationale\":\"short neutral reason\"}. Set crystallized=false (and leave goal empty) unless a single clear thesis has emerged. The goal MUST be phrased as a question being resolved, never a belief to adopt.",
    );
    prompt
}

/// Ask the Observer whether a thesis has crystallized into a proposable session
/// goal, given the user's recorded `positions`. Returns `Some(GoalProposal)`
/// only when the model reports a crystallized thesis with a non-empty goal;
/// `None` otherwise — including offline / malformed responses, where no goal is
/// fabricated (the session simply stays free-flowing).
pub fn propose_goal<C: LLMClient>(client: &C, positions: &[String]) -> Option<GoalProposal> {
    if positions.is_empty() {
        return None;
    }
    let prompt = goal_proposal_prompt(positions);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .ok()?;
    let (text, _tool_calls) = runtime
        .block_on(client.call(GOAL_PROPOSAL_SYSTEM_PROMPT, &[Message::user(prompt)], &[]))
        .ok()?;
    parse_goal_proposal(&text)
}

/// Parse the goal-proposal JSON. Returns `None` unless `crystallized` is true and
/// `goal` is a non-empty string, so a hedging or malformed response never yields
/// a fabricated goal.
pub fn parse_goal_proposal(text: &str) -> Option<GoalProposal> {
    let value: Value = serde_json::from_str(text.trim()).ok()?;
    if !value
        .get("crystallized")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let goal = value
        .get("goal")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|goal| !goal.is_empty())?
        .to_string();
    let rationale = value
        .get("rationale")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    Some(GoalProposal { goal, rationale })
}

// trace:STORY-160 | ai:claude
/// The challenger's CLOSING STATEMENT in the closing ritual: its strongest
/// remaining OBJECTION to the user's settled position. Belief-neutral and
/// STRUCTURAL — it presses on a gap/tension/unmet axis in the user's CASE, it
/// never asserts the user's belief is false or advocates an opposing belief.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ClosingObjection {
    /// The strongest remaining objection, phrased as a structural challenge to
    /// the user's case (what it leaves unaddressed), never a counter-belief.
    pub objection: String,
    /// True when this objection was synthesized structurally (offline / degraded)
    /// rather than by the LLM.
    pub degraded: bool,
}

/// System prompt pinning the closing-objection step to its belief-neutral,
/// structural contract. The challenger steelmans the WEAKNESS in the user's case
/// without ever asserting a belief is true or false.
const CLOSING_OBJECTION_SYSTEM_PROMPT: &str = "You are quizdom's challenger making your CLOSING statement. You are STRICTLY belief-neutral. The user has rested their case with a settled position. State the single STRONGEST REMAINING OBJECTION to that position — the most important gap, tension, ambiguity, or unmet burden their case still leaves open. Press on the STRUCTURE of their argument (consistency / clarity / completeness / coherence), NEVER asserting their belief is false and NEVER advocating an opposing belief. You are not trying to win a belief; you are naming what their case has not yet answered. Be concise and specific.";

/// Build the closing-objection prompt from the user's most recent settled
/// position (their just-rested closing statement) and the goal being resolved.
fn closing_objection_prompt(position: &str, goal: Option<&str>) -> String {
    let mut prompt = String::new();
    if let Some(goal) = goal.map(str::trim).filter(|g| !g.is_empty()) {
        prompt.push_str(&format!("The question being resolved (the goal): {goal}\n"));
    }
    prompt.push_str(&format!(
        "The user's settled / closing position: {position}\n\nReturn only JSON: {{\"objection\":\"the single strongest remaining objection, as a STRUCTURAL challenge to their case — what it leaves unaddressed — never a counter-belief and never asserting their belief is false\"}}.",
    ));
    prompt
}

/// Ask the challenger for its strongest remaining objection to the user's rested
/// position, degrading to a structural objection when the call fails or returns
/// something unusable — so the closing ritual always has a challenger turn,
/// online or off.
pub fn read_closing_objection<C: LLMClient>(
    client: &C,
    position: &str,
    goal: Option<&str>,
) -> ClosingObjection {
    match llm_closing_objection(client, position, goal) {
        Some(objection) => objection,
        None => structural_objection(position, goal),
    }
}

/// The LLM leg of [`read_closing_objection`]. Returns `None` on any failure so
/// the caller degrades to the structural objection.
fn llm_closing_objection<C: LLMClient>(
    client: &C,
    position: &str,
    goal: Option<&str>,
) -> Option<ClosingObjection> {
    let prompt = closing_objection_prompt(position, goal);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .ok()?;
    let (text, _tool_calls) = runtime
        .block_on(client.call(
            CLOSING_OBJECTION_SYSTEM_PROMPT,
            &[Message::user(prompt)],
            &[],
        ))
        .ok()?;
    parse_closing_objection(&text, position)
}

/// Parse the challenger's JSON into a [`ClosingObjection`]. Returns `None` when
/// the payload is not the expected object or carries no objection text, so the
/// caller degrades. A field that merely echoes the user's own position verbatim
/// is scrubbed (it is not an objection).
pub fn parse_closing_objection(text: &str, position: &str) -> Option<ClosingObjection> {
    let value: Value = serde_json::from_str(text.trim()).ok()?;
    if !value.is_object() {
        return None;
    }
    let objection = value
        .get("objection")
        .and_then(Value::as_str)
        .map(str::trim)
        .map(|raw| scrub_supplied_answer(raw, position))
        .filter(|objection| !objection.is_empty())?;
    Some(ClosingObjection {
        objection,
        degraded: false,
    })
}

/// The offline / degraded closing objection: a minimal STRUCTURAL challenge
/// derived purely from the rested position (and goal). It invents no
/// counter-belief — it points at the structural burden any rested case still
/// carries, so the challenger always gets a closing turn with no model available.
pub fn structural_objection(position: &str, goal: Option<&str>) -> ClosingObjection {
    let focus = goal
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(|g| format!("whether your case fully resolves \"{}\"", first_sentence(g)))
        .unwrap_or_else(|| "whether your case is complete".to_string());
    let objection = if position.trim().is_empty() {
        format!(
            "Offline objection: no settled position was stated, so {focus} cannot yet be assessed — state the strongest version of your case before the verdict."
        )
    } else {
        format!(
            "Offline objection: the strongest remaining challenge to your case is {focus} — name the one consideration it does not yet address and why your position still holds despite it."
        )
    };
    ClosingObjection {
        objection,
        degraded: true,
    }
}

// trace:STORY-164 | ai:claude
/// An answer from the `/help` channel: a belief-neutral explanation of HOW the
/// tool / dialogue works, sourced from TOOL-CONTEXT (the design: controls, the
/// flow, what a feature does) — NEVER from the user's belief content.
///
/// `/help` is the free-form counterpart to the palette's static per-command `?`
/// help: the user asks a question in their own words ("how do I rest my case?",
/// "what does observe do?") and gets a process answer. It is strictly
/// belief-neutral: it talks only about the TOOL, so it can never supply a belief
/// or take a side. Offline it degrades to a STATIC help index built from the same
/// tool-context — no model, no belief content invented.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HelpAnswer {
    /// The belief-neutral, process-focused answer.
    pub answer: String,
    /// True when this answer was assembled from the STATIC help index (offline /
    /// degraded) rather than produced by the LLM.
    pub degraded: bool,
}

/// System prompt that pins `/help` to its belief-neutral, TOOL-CONTEXT-only
/// contract. The model answers ONLY from the supplied description of the tool's
/// controls and flow; it must never engage with, supply, or take a side on the
/// user's belief content. The guarantee is mirrored structurally: when the model
/// is unavailable the caller degrades to [`static_help_index`], which is built
/// purely from the same tool-context and so is belief-neutral by construction.
const HELP_SYSTEM_PROMPT: &str = "You are quizdom's /help channel. quizdom is a Socratic belief-exploration tool, and YOUR job is to explain HOW THE TOOL AND THE DIALOGUE WORK — the controls, the flow, what each feature does, how to rest a case. You answer ONLY from the TOOL-CONTEXT supplied below (the tool's design). You are STRICTLY belief-neutral: you must NEVER answer the user's belief/philosophical question itself, NEVER supply a belief, NEVER take a side or say which position is correct, and NEVER scaffold a belief-framing. If the user's question is about a belief rather than the tool, redirect them to the process (e.g. /observe or /tutor) without engaging the belief's content. Be concise, concrete, and grounded in the listed controls.";

/// Build the `/help` prompt: the user's free-form process question plus the
/// supplied TOOL-CONTEXT (the design facts the answer must be grounded in). The
/// tool-context carries no belief content, so the prompt cannot leak one.
fn help_prompt(question: &str, tool_context: &str) -> String {
    let question = question.trim();
    let asked = if question.is_empty() {
        "(no specific question — give a brief orientation to the controls and flow)"
    } else {
        question
    };
    format!(
        "TOOL-CONTEXT (the tool's design — the only thing you may answer from):\n{tool_context}\n\nThe user's process question: {asked}\n\nAnswer the user's question about HOW THE TOOL WORKS, grounded in the tool-context above. Stay strictly belief-neutral: do NOT answer any belief/philosophical question, supply a belief, or take a side — if the question is about a belief, point them at the relevant control instead. Reply in plain prose (no JSON), a few sentences at most."
    )
}

// trace:STORY-164 | ai:claude
/// Answer a free-form `/help` question with the supplied LLM client, degrading to
/// the STATIC help index when the call fails or returns nothing usable.
///
/// `tool_context` is the design description the answer must be grounded in (the
/// controls + their detailed help, supplied by the caller from the command
/// registry). Belief-neutral throughout: the system prompt forbids engaging the
/// belief content, and the offline fallback is built purely from the same
/// tool-context — so `/help` can never supply a belief, online or off.
pub fn answer_help<C: LLMClient>(client: &C, question: &str, tool_context: &str) -> HelpAnswer {
    match llm_answer_help(client, question, tool_context) {
        Some(answer) => answer,
        // Offline / not-logged-in / malformed: fall back to the static help index
        // rather than failing the keypress mid-session.
        None => static_help_index(question, tool_context),
    }
}

/// The LLM leg of [`answer_help`]: run the call on a current-thread runtime and
/// return the trimmed prose answer. Returns `None` on any failure (or an empty
/// answer) so the caller degrades to the static help index.
fn llm_answer_help<C: LLMClient>(
    client: &C,
    question: &str,
    tool_context: &str,
) -> Option<HelpAnswer> {
    let prompt = help_prompt(question, tool_context);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .ok()?;
    let (text, _tool_calls) = runtime
        .block_on(client.call(HELP_SYSTEM_PROMPT, &[Message::user(prompt)], &[]))
        .ok()?;
    let answer = text.trim();
    if answer.is_empty() {
        return None;
    }
    Some(HelpAnswer {
        answer: answer.to_string(),
        degraded: false,
    })
}

// trace:STORY-164 | ai:claude
/// The offline / degraded `/help` answer: a STATIC help index assembled purely
/// from the supplied TOOL-CONTEXT. It invents no content and engages no belief —
/// it just surfaces the design facts (the controls and what they do), optionally
/// led by the line that best matches the user's question, so `/help` still does
/// something useful with no model available. Belief-neutral by construction.
pub fn static_help_index(question: &str, tool_context: &str) -> HelpAnswer {
    // Search terms from the question, with punctuation stripped and the short,
    // ubiquitous words ("how", "do", "my", …) dropped so the "Most relevant"
    // lead-in keys on the content word the user cares about ("rest", "observe")
    // rather than a stopword that appears across many control lines.
    let needle = question
        .trim()
        .to_ascii_lowercase()
        .split_whitespace()
        .map(|term| {
            term.trim_matches(|c: char| !c.is_alphanumeric())
                .to_string()
        })
        .filter(|term| term.len() >= 4)
        .collect::<Vec<_>>();
    let lines: Vec<&str> = tool_context
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();

    let mut answer = String::from(
        "Offline help (belief-neutral — about the tool, not your belief). Here are the controls and what they do:\n",
    );

    // When the question has search terms, lead with the single best-matching
    // control line so the static index points at what the user asked about. The
    // match is purely lexical over the tool-context — no belief content is read.
    if !needle.is_empty() {
        if let Some(best) = lines
            .iter()
            .max_by_key(|line| help_match_score(&line.to_ascii_lowercase(), &needle))
            .filter(|line| help_match_score(&line.to_ascii_lowercase(), &needle) > 0)
        {
            answer.push_str(&format!("\nMost relevant: {best}\n"));
        }
    }

    for line in &lines {
        answer.push_str(&format!("  {line}\n"));
    }
    answer.push_str(
        "\nTip: open the palette with '/' and press '?' on any command for its full help.",
    );
    HelpAnswer {
        answer,
        degraded: true,
    }
}

/// How many of the question's search terms appear in a tool-context `line` (both
/// already lowercased). Used only to choose which static-index line to lead with;
/// it reads the TOOL description, never any belief content.
fn help_match_score(line: &str, needle: &[String]) -> usize {
    needle.iter().filter(|term| line.contains(*term)).count()
}

/// The first sentence (or the whole string if it has no terminator), trimmed,
/// used to keep the structural note compact without echoing a long prompt.
fn first_sentence(text: &str) -> String {
    let trimmed = text.trim();
    trimmed
        .split_inclusive(['.', '?', '!'])
        .next()
        .unwrap_or(trimmed)
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::{LLMError, LLMFuture, ToolDef};
    use std::cell::RefCell;

    struct MockClient {
        response: RefCell<Option<Result<String, LLMError>>>,
        last_prompt: RefCell<Option<String>>,
    }

    impl MockClient {
        fn ok(body: &str) -> Self {
            Self {
                response: RefCell::new(Some(Ok(body.to_string()))),
                last_prompt: RefCell::new(None),
            }
        }

        fn err() -> Self {
            Self {
                response: RefCell::new(Some(Err(LLMError::Provider("offline".to_string())))),
                last_prompt: RefCell::new(None),
            }
        }
    }

    impl LLMClient for MockClient {
        fn call<'a>(
            &'a self,
            _system: &'a str,
            messages: &'a [Message],
            _tools: &'a [ToolDef],
        ) -> LLMFuture<'a> {
            if let Some(message) = messages.first() {
                *self.last_prompt.borrow_mut() = Some(message.content.clone());
            }
            let response = self.response.borrow_mut().take();
            Box::pin(async move {
                match response {
                    Some(Ok(text)) => Ok((text, Vec::new())),
                    Some(Err(error)) => Err(error),
                    None => Err(LLMError::Provider("no mock response".to_string())),
                }
            })
        }
    }

    fn exchange() -> Exchange {
        Exchange {
            question: "Is free will real?".to_string(),
            answer: "Yes, obviously.".to_string(),
            rebuttal: "But if every choice is caused, what is left to be free?".to_string(),
        }
    }

    #[test]
    fn reads_clarify_and_coach_output_from_the_llm() {
        let client = MockClient::ok(
            r#"{
                "plain_rebuttal": "If your choices are all caused, the follow-up asks what 'free' adds.",
                "tension": "Whether 'free' means uncaused or merely unconstrained.",
                "mismatch": "Asked whether free will is real; answered with confidence, not with a definition of 'free'.",
                "dimensions": ["What 'free' means", "What 'caused' rules out", "Whether the two can coexist"],
                "engagement": "Clear stance, but the key term is undefined, so the rebuttal lands."
            }"#,
        );
        let reading = read_exchange(&client, &exchange());

        assert!(!reading.degraded);
        assert!(reading.plain_rebuttal.contains("caused"));
        assert!(reading.tension.contains("uncaused") || reading.tension.contains("unconstrained"));
        assert_eq!(reading.dimensions.len(), 3);
        assert!(reading.engagement.contains("undefined") || reading.engagement.contains("term"));
    }

    #[test]
    fn the_prompt_pins_belief_neutrality() {
        let client = MockClient::ok(r#"{"tension":"t"}"#);
        let _ = read_exchange(&client, &exchange());
        let prompt = client.last_prompt.borrow().clone().unwrap();
        assert!(prompt.contains("Do NOT supply an answer"));
        assert!(prompt.contains("which belief is right"));
    }

    #[test]
    fn never_supplies_the_users_answer() {
        // A misbehaving model that echoes the user's answer verbatim into a
        // field must NOT have it pass through to the user.
        let client = MockClient::ok(
            r#"{"plain_rebuttal":"Yes, obviously.","tension":"the meaning of free","mismatch":"m","dimensions":["Yes, obviously."],"engagement":"e"}"#,
        );
        let reading = read_exchange(&client, &exchange());
        assert!(
            !reading.plain_rebuttal.contains("Yes, obviously."),
            "the user's verbatim answer must be withheld, got: {}",
            reading.plain_rebuttal
        );
        assert!(reading.plain_rebuttal.contains("withheld"));
        assert!(
            reading.dimensions.iter().all(|d| d != "Yes, obviously."),
            "dimensions must not echo the user's answer"
        );
    }

    #[test]
    fn degrades_to_a_structural_note_when_offline() {
        let client = MockClient::err();
        let reading = read_exchange(&client, &exchange());
        assert!(reading.degraded);
        assert!(reading.mismatch.contains("Asked:"));
        assert!(!reading.dimensions.is_empty());
        // The structural note must not invent the user's answer back at them.
        assert!(!reading.plain_rebuttal.contains("Yes, obviously."));
    }

    #[test]
    fn degrades_when_the_llm_returns_unparseable_text() {
        let client = MockClient::ok("not json at all");
        let reading = read_exchange(&client, &exchange());
        assert!(reading.degraded);
    }

    #[test]
    fn degrades_when_the_llm_returns_an_empty_object() {
        let client = MockClient::ok("{}");
        let reading = read_exchange(&client, &exchange());
        assert!(reading.degraded);
    }

    #[test]
    fn structural_note_handles_an_empty_answer() {
        let mut exchange = exchange();
        exchange.answer = String::new();
        let reading = structural_reading(&exchange);
        assert!(reading.mismatch.contains("no answer recorded yet"));
    }

    // ---- STORY-159: Observer-proposed goal ---------------------------------

    fn positions() -> Vec<String> {
        vec![
            "On \"Is free will real?\": yes".to_string(),
            "On \"Can a caused choice be free?\": no".to_string(),
        ]
    }

    #[test]
    fn proposes_a_goal_when_a_thesis_has_crystallized() {
        // trace:STORY-159 | ai:claude
        let client = MockClient::ok(
            r#"{"crystallized":true,"goal":"can libertarian free will be held consistently?","rationale":"the user keeps circling whether uncaused choice survives causation"}"#,
        );
        let proposal = propose_goal(&client, &positions()).expect("a crystallized thesis");
        assert_eq!(
            proposal.goal,
            "can libertarian free will be held consistently?"
        );
        assert!(proposal.rationale.contains("causation"));
        // The prompt pins belief-neutrality: a QUESTION to settle, not a belief.
        let prompt = client.last_prompt.borrow().clone().unwrap();
        assert!(prompt.contains("belief-neutral QUESTION"));
        assert!(prompt.contains("On \"Is free will real?\": yes"));
    }

    #[test]
    fn declines_when_no_thesis_has_crystallized() {
        // trace:STORY-159 | ai:claude — crystallized=false yields no goal, so the
        // session stays free-flowing rather than being handed a fabricated thesis.
        let client =
            MockClient::ok(r#"{"crystallized":false,"goal":"","rationale":"still wandering"}"#);
        assert!(propose_goal(&client, &positions()).is_none());
    }

    #[test]
    fn declines_when_crystallized_but_goal_is_empty() {
        // A crystallized flag with no goal text is not a usable proposal.
        let client = MockClient::ok(r#"{"crystallized":true,"goal":"   "}"#);
        assert!(propose_goal(&client, &positions()).is_none());
    }

    #[test]
    fn no_proposal_with_no_positions() {
        // Nothing recorded yet → no thesis can have crystallized; never calls out.
        let client = MockClient::ok(r#"{"crystallized":true,"goal":"a thesis"}"#);
        assert!(propose_goal(&client, &[]).is_none());
    }

    #[test]
    fn declines_offline_or_on_malformed_response() {
        // trace:STORY-159 | ai:claude — offline / junk degrades to no proposal.
        assert!(propose_goal(&MockClient::err(), &positions()).is_none());
        assert!(propose_goal(&MockClient::ok("not json"), &positions()).is_none());
    }

    #[test]
    fn parse_goal_proposal_requires_crystallized_true() {
        assert!(parse_goal_proposal(r#"{"goal":"x"}"#).is_none());
        assert!(parse_goal_proposal(r#"{"crystallized":true,"goal":"x"}"#).is_some());
    }

    // ---- STORY-160: the challenger's closing objection ---------------------

    #[test]
    fn reads_the_challengers_strongest_remaining_objection() {
        // trace:STORY-160 | ai:claude
        let client = MockClient::ok(
            r#"{"objection":"Your case never says what 'free' rules out, so completeness is unmet."}"#,
        );
        let objection = read_closing_objection(
            &client,
            "Free will is real because we deliberate.",
            Some("can libertarian free will be held consistently?"),
        );
        assert!(!objection.degraded);
        assert!(objection.objection.contains("completeness"));
        // The prompt pins belief-neutral STRUCTURE, and carries the goal.
        let prompt = client.last_prompt.borrow().clone().unwrap();
        assert!(prompt.contains("STRUCTURAL challenge"));
        assert!(prompt.contains("can libertarian free will be held consistently?"));
    }

    #[test]
    fn closing_objection_never_echoes_the_users_position_verbatim() {
        // trace:STORY-160 | ai:claude — a model that just parrots the user's
        // position back is not an objection; it is SCRUBBED so the user's own words
        // are never handed back as a "challenge".
        let position = "Free will is real.";
        let client = MockClient::ok(r#"{"objection":"Free will is real."}"#);
        let objection = read_closing_objection(&client, position, None);
        assert!(!objection.objection.contains("Free will is real."));
        assert!(objection.objection.contains("withheld"));
    }

    #[test]
    fn closing_objection_degrades_to_a_structural_challenge_offline() {
        // trace:STORY-160 | ai:claude — offline still produces a belief-neutral
        // STRUCTURAL objection so the closing ritual has a challenger turn.
        let objection = read_closing_objection(
            &MockClient::err(),
            "Determinism is true.",
            Some("is determinism true?"),
        );
        assert!(objection.degraded);
        assert!(objection.objection.contains("is determinism true?"));
        // Belief-neutral: it challenges the STRUCTURE, never asserts a belief.
        assert!(!objection.objection.to_lowercase().contains("you are wrong"));
    }

    #[test]
    fn structural_objection_handles_an_empty_position() {
        let objection = structural_objection("   ", None);
        assert!(objection.degraded);
        assert!(objection.objection.contains("no settled position"));
    }

    // ---- STORY-164: /help (process help, tool-context, belief-neutral) ------

    fn tool_context() -> &'static str {
        "/observe — a belief-neutral reading of the current exchange.\n\
         /rest — rest your case and begin the closing ritual.\n\
         /goal — state or show the session goal."
    }

    #[test]
    fn help_answers_process_questions_from_tool_context() {
        // trace:STORY-164 | ai:claude — /help answers a HOW-the-tool-works question
        // from the supplied TOOL-CONTEXT (the design), and the answer flows through.
        let client = MockClient::ok(
            "To rest your case, use the /rest control — it begins the closing ritual.",
        );
        let answer = answer_help(&client, "how do I rest my case?", tool_context());
        assert!(!answer.degraded);
        assert!(answer.answer.contains("/rest"));
    }

    #[test]
    fn help_prompt_is_grounded_in_tool_context_and_belief_neutral() {
        // trace:STORY-164 | ai:claude — the prompt carries the TOOL-CONTEXT and
        // pins belief-neutrality (answer about the tool, never the belief), and the
        // user's process question rides along.
        let client = MockClient::ok("ok");
        let _ = answer_help(&client, "what does observe do?", tool_context());
        let prompt = client.last_prompt.borrow().clone().unwrap();
        assert!(prompt.contains("TOOL-CONTEXT"));
        assert!(prompt.contains("/observe"));
        assert!(prompt.contains("what does observe do?"));
        assert!(prompt.to_lowercase().contains("belief-neutral"));
        assert!(prompt.contains("do NOT answer any belief"));
    }

    #[test]
    fn help_system_prompt_forbids_supplying_a_belief() {
        // trace:STORY-164 | ai:claude — the contract is tool-context, not belief
        // content: the system prompt must forbid answering the belief itself.
        assert!(HELP_SYSTEM_PROMPT.contains("belief-neutral"));
        assert!(HELP_SYSTEM_PROMPT.contains("NEVER take a side"));
        assert!(HELP_SYSTEM_PROMPT.contains("TOOL-CONTEXT"));
    }

    #[test]
    fn help_degrades_to_a_static_index_when_offline() {
        // trace:STORY-164 | ai:claude — offline, /help still answers from a STATIC
        // help index built purely from the tool-context (no model, no belief).
        let answer = answer_help(&MockClient::err(), "how do I rest my case?", tool_context());
        assert!(answer.degraded);
        // The static index surfaces the controls from the tool-context...
        assert!(answer.answer.contains("/rest"));
        assert!(answer.answer.contains("/observe"));
        // ...and leads with the control that best matches the question.
        assert!(answer.answer.contains("Most relevant:"));
        let relevant_line = answer
            .answer
            .lines()
            .find(|line| line.starts_with("Most relevant:"))
            .unwrap();
        assert!(relevant_line.contains("/rest"));
    }

    #[test]
    fn help_degrades_when_the_llm_returns_empty_or_unusable() {
        // trace:STORY-164 | ai:claude — an empty model answer is no better than
        // offline; degrade to the static index rather than render a blank box.
        let answer = answer_help(&MockClient::ok("   "), "", tool_context());
        assert!(answer.degraded);
        assert!(answer.answer.contains("Offline help"));
    }

    #[test]
    fn static_help_index_handles_an_empty_question() {
        // A bare /help (no question) still lists the controls, with no
        // "Most relevant" lead-in (nothing to match against).
        let answer = static_help_index("", tool_context());
        assert!(answer.degraded);
        assert!(answer.answer.contains("/observe"));
        assert!(!answer.answer.contains("Most relevant:"));
    }

    #[test]
    fn help_never_engages_belief_content() {
        // trace:STORY-164 | ai:claude — even when the user phrases their question as
        // a belief, the offline index answers ONLY from the tool-context (the
        // controls) and never supplies a belief or takes a side. Belief-neutral by
        // construction: the answer is assembled purely from the tool description.
        let answer = static_help_index("is determinism true?", tool_context());
        // The answer is the controls list — it contains none of the belief content.
        assert!(!answer.answer.to_lowercase().contains("determinism is"));
        assert!(answer.answer.contains("about the tool, not your belief"));
        assert!(answer.answer.contains("/goal"));
    }
}
