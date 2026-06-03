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
}
