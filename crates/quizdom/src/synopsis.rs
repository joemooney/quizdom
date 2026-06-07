// trace:STORY-128 | ai:claude
//! The GLOBAL synopsis mode of the Observer: a BELIEF-NEUTRAL, CLARIFY-ONLY
//! reading of a *whole session* rather than a single exchange (STORY-127).
//!
//! Where the per-exchange observer (`observer.rs`) reads one question / answer /
//! rebuttal turn, the synopsis reads the full JSONL session log and summarizes
//! the **arc**:
//!
//! - the positions taken across the session,
//! - how they evolved (what shifted, what held),
//! - the internal consistency of those positions (the "tensions"),
//! - where the user now stands, and
//! - what is still unresolved (the open threads),
//!
//! plus a short belief-neutral **engagement** read (clarity / consistency /
//! precision) — *never* belief grading. It never says which position is
//! "right", never advocates a belief, and never supplies the user's answer. It
//! only helps the user see the shape of their own session more clearly.
//!
//! Like the per-exchange observer, the synopsis is produced by an LLM (default
//! backend: claude-cli). When the LLM is unavailable (offline, not logged in,
//! malformed response), it degrades to a **structural summary** derived purely
//! from the log — the recorded turns and positions, with no model and no belief
//! content invented.
//!
//! It is surfaced two ways:
//! - the standalone `quizdom session synopsis <id> [--user]` command, and
//! - an in-session key (handled in `session.rs`).

use crate::error::{QuizdomError, Result};
use crate::session::normalize_session_id;
use llm::{LLMClient, Message};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;

/// Default quizdom user whose session logs the synopsis command reads when no
/// `--user` is given — matching the session loop's own default.
const DEFAULT_USER: &str = "local-user";

/// One recorded step of a session: the question that was asked and the position
/// the user took on it. Extracted from the JSONL log purely structurally — no
/// belief content is invented, only the user's own recorded words are carried.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SessionTurn {
    /// The turn number, as recorded in the log.
    pub turn: u64,
    /// The branch this turn belongs to (default `"main"`).
    pub branch: String,
    /// The question text that was asked.
    pub question: String,
    /// The user's raw position / answer on that question.
    pub position: String,
}

/// The structural arc of a session: the ordered turns plus the standalone
/// markers (definitions settled, contradictions surfaced) that colour it.
///
/// This is the belief-neutral substrate both the LLM synopsis and the offline
/// structural summary are built from. It carries only what the user recorded —
/// it never adds a position, a judgement, or a belief.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct SessionArc {
    /// The session id, if the log carried one.
    pub session_id: String,
    /// The user id, if the log carried one.
    pub user_id: String,
    /// The ordered positions the user took.
    pub turns: Vec<SessionTurn>,
    /// Terms the user settled a working definition for (term + definition).
    pub definitions: Vec<(String, String)>,
    /// Tensions already surfaced in-session (contradictions the session flagged),
    /// rendered as a short "left vs right" label. May feed the synopsis's
    /// consistency read (EPIC-9 contradiction detection).
    pub tensions: Vec<String>,
    // trace:STORY-159 | ai:claude
    /// The session GOAL/thesis the exploration is resolving, if one was set. When
    /// present, the roundedness score is measured WITH RESPECT TO it ("have you
    /// settled X?") so the goal becomes the convergence target. Read structurally
    /// from the most recent `session_started`/`goal_set` event in the log;
    /// belief-neutral — the goal is a question being settled, never a belief.
    pub goal: Option<String>,
    // trace:STORY-161 | ai:claude
    /// The session MODE (the EPIC-158 toggle), read from the `session_started`
    /// event (and any later `mode_set`). In `Debate` mode the verdict judges which
    /// CASE was better-ARGUED structurally; in `Socratic` (default) it assesses
    /// the user's own position's roundedness. Belief-neutral in both: the score is
    /// STRUCTURE, never which belief is true.
    pub mode: crate::strategy::SessionMode,
}

impl SessionArc {
    /// True when the log carried no recorded position at all — there is nothing
    /// to summarize.
    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }
}

// trace:STORY-156 | ai:claude
/// One belief-neutral line summarizing the user's OWN most recent recorded
/// position, used by the graceful conclude path. Purely structural: it echoes
/// the user's own words (no model, no belief invented, never advocating). When
/// no position was recorded it returns `None`, so the conclude path can fall
/// back rather than fabricate a standing the user never took.
pub fn concluding_standing(arc: &SessionArc) -> Option<String> {
    arc.turns
        .iter()
        .rev()
        .find(|turn| !turn.position.is_empty())
        .map(|last| {
            let asked = first_sentence(&last.question);
            if asked.is_empty() {
                last.position.clone()
            } else {
                format!("On \"{asked}\": {}", last.position)
            }
        })
}

/// Build the structural [`SessionArc`] from a session JSONL log.
///
/// `branch` filters to a single branch (matching `branch_id`, default `"main"`);
/// pass `None` to fold every branch into the arc. Purely structural: it reads
/// the recorded events and carries the user's own words, inventing nothing.
pub fn arc_from_session_log(reader: impl Read, branch: Option<&str>) -> Result<SessionArc> {
    let reader = BufReader::new(reader);
    let mut arc = SessionArc::default();
    // Question text is logged on `question_presented`; the answer arrives on a
    // later `answer_recorded` for the same turn, so we stage the text by turn.
    let mut pending_question: Vec<(u64, String, String)> = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value =
            serde_json::from_str(&line).map_err(|error| QuizdomError::Parse(error.to_string()))?;

        let event_branch = value
            .get("branch_id")
            .and_then(Value::as_str)
            .unwrap_or("main");
        // `branch_forked` carries no branch_id and is not branch-specific; the
        // arc never renders it, so we just skip non-matching branches uniformly.
        if let Some(branch) = branch {
            if event_branch != branch {
                continue;
            }
        }

        if arc.session_id.is_empty() {
            if let Some(id) = value.get("session_id").and_then(Value::as_str) {
                arc.session_id = id.to_string();
            }
        }
        if arc.user_id.is_empty() {
            if let Some(id) = value.get("user_id").and_then(Value::as_str) {
                arc.user_id = id.to_string();
            }
        }

        // trace:STORY-159 | ai:claude — capture the session goal from the start
        // event or any later `goal_set` event (the most recent wins, so an
        // in-session / Observer-proposed goal overrides the `--goal` flag).
        if matches!(
            value.get("event_type").and_then(Value::as_str),
            Some("session_started") | Some("goal_set")
        ) {
            if let Some(goal) = value
                .get("goal")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|goal| !goal.is_empty())
            {
                arc.goal = Some(goal.to_string());
            }
        }

        // trace:STORY-161 | ai:claude — capture the session MODE from the start
        // event or any later `mode_set` event (the most recent wins, so an
        // in-session toggle overrides the `--mode` flag). An unrecognized token is
        // ignored, leaving the default Socratic mode.
        if matches!(
            value.get("event_type").and_then(Value::as_str),
            Some("session_started") | Some("mode_set")
        ) {
            if let Some(mode) = value
                .get("mode")
                .and_then(Value::as_str)
                .and_then(crate::strategy::SessionMode::parse)
            {
                arc.mode = mode;
            }
        }

        match value.get("event_type").and_then(Value::as_str) {
            Some("question_presented") => {
                if let (Some(turn), Some(text)) = (
                    value.get("turn").and_then(Value::as_u64),
                    value.get("question_text").and_then(Value::as_str),
                ) {
                    pending_question.push((turn, event_branch.to_string(), text.to_string()));
                }
            }
            Some("answer_recorded") => {
                let Some(turn) = value.get("turn").and_then(Value::as_u64) else {
                    continue;
                };
                let position = value
                    .get("raw_answer")
                    .and_then(Value::as_str)
                    .or_else(|| value.get("normalized_answer").and_then(Value::as_str))
                    .unwrap_or("")
                    .trim()
                    .to_string();
                // Pair with the most recent matching question on this branch.
                let question = pending_question
                    .iter()
                    .rev()
                    .find(|(t, b, _)| *t == turn && b == event_branch)
                    .map(|(_, _, text)| text.clone())
                    .unwrap_or_default();
                if question.is_empty() && position.is_empty() {
                    continue;
                }
                arc.turns.push(SessionTurn {
                    turn,
                    branch: event_branch.to_string(),
                    question,
                    position,
                });
            }
            Some("term_interpreted") => {
                let term = value.get("term").and_then(Value::as_str).unwrap_or("");
                let definition = value
                    .get("adopted_definition")
                    .and_then(Value::as_str)
                    .or_else(|| value.get("raw_definition").and_then(Value::as_str))
                    .unwrap_or("");
                if !term.is_empty() && !definition.is_empty() {
                    arc.definitions
                        .push((term.to_string(), definition.to_string()));
                }
            }
            // trace:STORY-160 | ai:claude — fold the closing ritual into the arc so
            // the verdict reflects it: the user's closing STATEMENTS count as
            // positions (their settled case), and the challenger's objections are
            // recorded as structural tensions the verdict can weigh. Belief-neutral:
            // both are about the STRUCTURE of the case, never which belief is true.
            Some("closing_statement") => {
                let speaker = value.get("speaker").and_then(Value::as_str).unwrap_or("");
                let statement = value
                    .get("statement")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if statement.is_empty() {
                    continue;
                }
                let turn = value.get("turn").and_then(Value::as_u64).unwrap_or(0);
                match speaker {
                    "user" => arc.turns.push(SessionTurn {
                        turn,
                        branch: event_branch.to_string(),
                        question: "Closing statement".to_string(),
                        position: statement,
                    }),
                    "challenger" => arc
                        .tensions
                        .push(format!("Unanswered objection: {statement}")),
                    _ => {}
                }
            }
            Some("contradiction_resolved") => {
                let left = value
                    .get("left_belief")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let right = value
                    .get("right_belief")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !left.is_empty() || !right.is_empty() {
                    arc.tensions.push(format!("\"{left}\" vs \"{right}\""));
                }
            }
            _ => {}
        }
    }

    Ok(arc)
}

// trace:STORY-155 | ai:claude
/// A belief-neutral ROUNDEDNESS assessment of a whole [`SessionArc`].
///
/// This measures STRUCTURE — never belief-correctness. Two opposite, equally
/// well-formed positions score comparably: the score reads how *consistent*,
/// *clear*, *complete*, and *coherent* the arc is, not whether the belief at its
/// centre is "right". Each sub-score is a 0–100 percentage; [`composite`] folds
/// them into a single roundedness %, and [`limiting_gap`] names the single
/// dimension holding the score back — the pull-based "steer toward a conclusion"
/// (EPIC-154).
///
/// The score is only ever produced by the LLM. Offline, there is no fabricated
/// score — the synopsis carries a structural-only "needs LLM" note instead (see
/// [`SessionSynopsis::roundedness`]).
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Roundedness {
    /// Consistency: no internal contradictions across the arc (ties EPIC-9).
    pub consistency: u8,
    /// Definitional clarity: the key terms are pinned (ties EPIC-8).
    pub definitional_clarity: u8,
    /// Completeness: the position addresses the live objections raised.
    pub completeness: u8,
    /// Coherence: the parts follow — the arc hangs together.
    pub coherence: u8,
    /// The single LIMITING GAP holding the composite back, named
    /// belief-neutrally (e.g. "you haven't addressed whether determinism
    /// undermines responsibility"). Never advocates a belief.
    pub limiting_gap: String,
}

// trace:STORY-156 | ai:claude
/// The composite roundedness % at (or above) which a position is "well-rounded":
/// coherent, consistent, clearly-defined, and having addressed its live
/// objections. Crossing it is what makes the on-demand synopsis OFFER to
/// conclude (EPIC-154). Belief-neutral: the threshold reads STRUCTURE only — it
/// never asserts the belief at the centre is correct, only that the *argument*
/// is well-formed. Set below 100 so a strong-but-imperfect arc still earns the
/// offer; the EPIC's 75% worked example sits just under it (shows the gap, no
/// offer yet).
pub const WELL_ROUNDED_THRESHOLD: u8 = 80;

impl Roundedness {
    /// The four belief-neutral structural dimensions, lowest-first won't matter
    /// here — order is fixed (consistency, clarity, completeness, coherence) so
    /// the breakdown renders stably.
    fn dimensions(&self) -> [(&'static str, u8); 4] {
        [
            ("consistency", self.consistency),
            ("definitional clarity", self.definitional_clarity),
            ("completeness", self.completeness),
            ("coherence", self.coherence),
        ]
    }

    /// The composite roundedness %, the mean of the four sub-scores rounded to
    /// the nearest whole percent. Belief-neutral: it folds STRUCTURE, never
    /// belief-correctness.
    pub fn composite(&self) -> u8 {
        let sum = self.consistency as u16
            + self.definitional_clarity as u16
            + self.completeness as u16
            + self.coherence as u16;
        // Round-to-nearest over four dimensions.
        ((sum + 2) / 4) as u8
    }

    /// The dimension dragging the composite down the most, as a
    /// `(label, score)` pair. Ties break toward the fixed dimension order
    /// (consistency first), so the breakdown and the limiting line agree.
    pub fn weakest_dimension(&self) -> (&'static str, u8) {
        self.dimensions()
            .into_iter()
            .min_by_key(|(_, score)| *score)
            .unwrap_or(("consistency", 0))
    }

    // trace:STORY-156 | ai:claude
    /// True when the composite has crossed the well-rounded threshold — the arc
    /// is coherent, consistent, clearly-defined, and has addressed its live
    /// objections, so the synopsis OFFERS to conclude. Belief-neutral: this reads
    /// STRUCTURE only and never asserts the centred belief is correct.
    pub fn is_well_rounded(&self) -> bool {
        self.composite() >= WELL_ROUNDED_THRESHOLD
    }
}

/// A belief-neutral, clarify-only synopsis of a whole [`SessionArc`].
///
/// Every field is descriptive, never prescriptive: it names the shape of the
/// session — positions, evolution, consistency, open threads — without
/// supplying an answer or asserting which belief is correct.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SessionSynopsis {
    /// The positions the user took, in their own terms.
    pub positions: Vec<String>,
    /// How the positions evolved across the session (what shifted, what held).
    pub evolution: String,
    /// The internal consistency of the positions — the tensions, named
    /// belief-neutrally, never resolved in the user's stead.
    pub consistency: String,
    /// Where the user now stands, as a structural summary of their arc.
    pub standing: String,
    /// What is still unresolved — the open threads the session left dangling.
    pub open_threads: Vec<String>,
    /// A short belief-neutral engagement read (clarity / consistency /
    /// precision) — NOT a grade of which belief is right.
    pub engagement: String,
    // trace:STORY-155 | ai:claude
    /// The belief-neutral roundedness assessment — a composite % over the
    /// structural dimensions plus the limiting gap. `Some` only when the LLM
    /// produced a usable score; `None` when offline / degraded, in which case
    /// the synopsis carries a "needs LLM" note rather than a fabricated number.
    pub roundedness: Option<Roundedness>,
    // trace:STORY-159 | ai:claude
    /// The session GOAL/thesis this synopsis was measured against, if one was set
    /// — carried through from the [`SessionArc`] so the render can surface the
    /// convergence target. Belief-neutral: the question being settled.
    pub goal: Option<String>,
    // trace:STORY-161 | ai:claude
    /// The session MODE this synopsis was produced under, carried from the
    /// [`SessionArc`] so the render can frame the verdict correctly: in `Debate`
    /// mode the header says it judges which CASE was better-ARGUED (structure),
    /// in `Socratic` it assesses the user's own position. Belief-neutral in both.
    pub mode: crate::strategy::SessionMode,
    /// True when this synopsis was synthesized structurally (offline / degraded)
    /// rather than by the LLM.
    pub degraded: bool,
}

impl SessionSynopsis {
    // trace:STORY-156 | ai:claude
    /// True when this synopsis crossed the well-rounded threshold and so OFFERS
    /// to conclude. `false` offline / when no score was produced — there is no
    /// offer without a real number. The in-session conclude path keys off this
    /// to prompt the user; below threshold the synopsis just shows the gap.
    pub fn offers_conclude(&self) -> bool {
        self.roundedness
            .as_ref()
            .map(Roundedness::is_well_rounded)
            .unwrap_or(false)
    }

    // trace:STORY-156 | ai:claude
    /// The composite roundedness %, if scored. Used by the conclude offer line.
    pub fn composite(&self) -> Option<u8> {
        self.roundedness.as_ref().map(Roundedness::composite)
    }
}

/// System prompt pinning the synopsis to its belief-neutral, clarify-only
/// contract. Mirrors the per-exchange observer's contract, scaled to a whole
/// session.
// trace:STORY-161 | ai:claude
/// System prompt for the DEBATE-mode verdict. Unlike the Socratic synopsis (which
/// scores the user's OWN position's roundedness), the debate verdict assesses
/// which CASE was better-ARGUED — the STRUCTURAL/argument quality of each side —
/// while staying STRICTLY belief-neutral on which belief is actually TRUE. It
/// judges craft (consistency / clarity / completeness / coherence of each case),
/// never truth.
const DEBATE_SYNOPSIS_SYSTEM_PROMPT: &str = "You are quizdom's session Synopsis observer in DEBATE mode. The session was a two-sided debate: the user argued their position and the questioner steelmanned the OPPOSING side. You are STRICTLY belief-neutral on which belief is TRUE — you MUST NOT assert which belief is correct, advocate a position, or say which SIDE is right about reality. Your verdict judges which CASE was better-ARGUED: the STRUCTURAL/argument quality of each side (consistency, definitional clarity, completeness in meeting the other side's objections, coherence). Summarize the arc of the debate: the positions each side took, how the argument evolved, the internal tensions in each case (without resolving them), where the exchange now stands, and what is still unresolved. You ALSO score the ROUNDEDNESS of the better-argued case on four STRUCTURAL dimensions, each 0-100: consistency, definitional_clarity, completeness (addresses the opposing objections raised), coherence. This score measures ARGUMENT CRAFT ONLY — never belief-correctness: a well-argued case for a false-seeming view still scores HIGH on craft, and you NEVER say which side's belief is true. Name the single LIMITING GAP holding the score back, belief-neutrally. Stay descriptive about argument quality, never prescriptive about belief.";

const SYNOPSIS_SYSTEM_PROMPT: &str = "You are quizdom's session Synopsis observer. You are STRICTLY belief-neutral and clarify-only. You read a WHOLE session — the positions the user took across many questions — and summarize the ARC so the user can see their own thinking more clearly. You MUST NOT supply the user's answer, take a side, assert which belief is correct, advocate a position, or grade which belief is better. Assess ENGAGEMENT only: clarity, internal consistency, and precision — never belief correctness. Only: list the positions taken, describe how they evolved, name the internal tensions (without resolving them), summarize where the user now stands, and list what is still unresolved. You ALSO score the ROUNDEDNESS of the arc on four STRUCTURAL dimensions, each 0-100: consistency (no internal contradictions), definitional_clarity (key terms pinned), completeness (addresses the live objections raised), coherence (the parts follow). This score measures STRUCTURE ONLY — never belief-correctness: two OPPOSITE well-formed positions must score COMPARABLY. A confident, clearly-defined, internally-consistent position that has met its objections scores HIGH whatever it concludes. Also name the single LIMITING GAP holding the score back, belief-neutrally (the one dimension to shore up), NEVER advocating a belief. Stay descriptive, not prescriptive.";

/// Build the synopsis prompt for one [`SessionArc`].
fn synopsis_prompt(arc: &SessionArc) -> String {
    let mut log = String::new();
    // trace:STORY-161 | ai:claude
    // In DEBATE mode, frame the verdict around which CASE was better-ARGUED
    // (argument craft) rather than the user's own position's roundedness. Stays
    // belief-neutral on truth: it scores STRUCTURE/craft, never which side is
    // right about reality.
    if arc.mode == crate::strategy::SessionMode::Debate {
        log.push_str(
            "DEBATE MODE: this was a two-sided debate (the user vs the steelmanned opposing side). Judge which CASE was better-ARGUED — the structural/argument quality of each side — and score the better-argued case's roundedness. Stay belief-neutral on TRUTH: never say which side's belief is true; assess argument craft only.\n\n",
        );
    }
    // trace:STORY-159 | ai:claude
    // When the session carries a GOAL, lead with it so the roundedness score is
    // measured WITH RESPECT TO it ("how well-settled is THIS goal?") — the goal
    // becomes the convergence target. Belief-neutral: completeness asks whether
    // the goal's live objections were met, never whether the answer is "right".
    if let Some(goal) = arc.goal.as_deref().map(str::trim).filter(|g| !g.is_empty()) {
        log.push_str(&format!(
            "Session goal (the claim/question being resolved): {goal}\nMeasure roundedness WITH RESPECT TO this goal — completeness/coherence are about how well-settled THIS goal is, and the limiting gap is what still stands between the user and resolving it. Stay belief-neutral: score whether the goal is settled structurally, never which answer is true.\n\n"
        ));
    }
    for turn in &arc.turns {
        log.push_str(&format!(
            "- (turn {}, branch {}) Q: {} | position: {}\n",
            turn.turn,
            turn.branch,
            turn.question,
            if turn.position.is_empty() {
                "(no answer)"
            } else {
                &turn.position
            }
        ));
    }
    if !arc.definitions.is_empty() {
        log.push_str("Working definitions the user settled:\n");
        for (term, definition) in &arc.definitions {
            log.push_str(&format!("- {term}: {definition}\n"));
        }
    }
    if !arc.tensions.is_empty() {
        log.push_str("Tensions already surfaced in-session:\n");
        for tension in &arc.tensions {
            log.push_str(&format!("- {tension}\n"));
        }
    }
    format!(
        "Here is the session arc (the user's own recorded positions):\n{log}\n\
         Return only JSON with these fields: {{\"positions\":[\"a position the user took, in their terms\"],\"evolution\":\"how the positions evolved across the session\",\"consistency\":\"the internal tensions, named neutrally and NOT resolved\",\"standing\":\"where the user now stands\",\"open_threads\":[\"a question the session left unresolved\"],\"engagement\":\"short read of clarity/consistency/precision — NOT which belief is right\",\"roundedness\":{{\"consistency\":0-100,\"definitional_clarity\":0-100,\"completeness\":0-100,\"coherence\":0-100,\"limiting_gap\":\"the one structural dimension to shore up, named belief-neutrally — NOT a belief to adopt\"}}}}. \
         The roundedness scores measure STRUCTURE only (consistency/clarity/completeness/coherence), NEVER belief-correctness: two opposite well-formed positions must score comparably. \
         Do NOT supply an answer, take a side, advocate a position, or grade which belief is right."
    )
}

/// Read a [`SessionArc`] with the supplied LLM client, degrading to a structural
/// summary when the call fails or returns something unusable.
pub fn synopsize<C: LLMClient>(client: &C, arc: &SessionArc) -> SessionSynopsis {
    match llm_synopsize(client, arc) {
        Some(synopsis) => synopsis,
        // Offline / not-logged-in / malformed: fall back to the structural
        // summary rather than failing the synopsis request.
        None => structural_synopsis(arc),
    }
}

/// The LLM leg of [`synopsize`]: run the call on a current-thread runtime, parse
/// the JSON, and enforce the no-answer-supplied guarantee. Returns `None` on any
/// failure so the caller degrades gracefully.
fn llm_synopsize<C: LLMClient>(client: &C, arc: &SessionArc) -> Option<SessionSynopsis> {
    let prompt = synopsis_prompt(arc);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .ok()?;
    // trace:STORY-161 | ai:claude — debate mode swaps in the better-argued-case
    // verdict system prompt; default stays the position-roundedness synopsis.
    let system_prompt = match arc.mode {
        crate::strategy::SessionMode::Socratic => SYNOPSIS_SYSTEM_PROMPT,
        crate::strategy::SessionMode::Debate => DEBATE_SYNOPSIS_SYSTEM_PROMPT,
    };
    let (text, _tool_calls) = runtime
        .block_on(client.call(system_prompt, &[Message::user(prompt)], &[]))
        .ok()?;
    parse_synopsis(&text, arc)
}

/// Parse the synopsis JSON into a [`SessionSynopsis`], enforcing the
/// belief-neutral / no-answer-supplied guarantee.
///
/// Returns `None` when the payload is not the expected JSON object so the caller
/// degrades to the structural summary. Any field (or list item) that reproduces
/// one of the user's own recorded positions verbatim is scrubbed (see
/// [`scrub_supplied_position`]) so a misbehaving model can never hand the user's
/// answer back as if it were guidance.
pub fn parse_synopsis(text: &str, arc: &SessionArc) -> Option<SessionSynopsis> {
    let value: Value = serde_json::from_str(text.trim()).ok()?;
    if !value.is_object() {
        return None;
    }
    let positions: Vec<String> = arc.turns.iter().map(|t| t.position.clone()).collect();
    let field = |key: &str| -> String {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .map(|raw| scrub_supplied_position(raw, &positions))
            .unwrap_or_default()
    };
    let list = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(|item| scrub_supplied_position(item, &positions))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };

    let positions_out = list("positions");
    let evolution = field("evolution");
    let consistency = field("consistency");
    let standing = field("standing");
    let open_threads = list("open_threads");
    let engagement = field("engagement");
    // trace:STORY-155 | ai:claude
    let roundedness = value.get("roundedness").and_then(parse_roundedness);

    // A synopsis with no usable content is no better than the structural
    // summary; let the caller degrade instead of rendering an empty box.
    if positions_out.is_empty()
        && evolution.is_empty()
        && consistency.is_empty()
        && standing.is_empty()
        && open_threads.is_empty()
        && engagement.is_empty()
        && roundedness.is_none()
    {
        return None;
    }

    Some(SessionSynopsis {
        positions: positions_out,
        evolution,
        consistency,
        standing,
        open_threads,
        engagement,
        roundedness,
        // trace:STORY-159 | ai:claude — carry the goal the score was measured
        // against through to the render.
        goal: arc.goal.clone(),
        // trace:STORY-161 | ai:claude — carry the mode so the render frames the
        // verdict (debate = better-argued case; socratic = own position).
        mode: arc.mode,
        degraded: false,
    })
}

// trace:STORY-155 | ai:claude
/// Parse the `roundedness` object from the synopsis JSON into a [`Roundedness`].
///
/// Returns `None` when the payload is missing the four numeric sub-scores, so a
/// model that omits the score (or returns junk) never yields a fabricated number
/// — the synopsis carries the structural "needs LLM" note instead. Sub-scores
/// are clamped to 0–100. The `limiting_gap` is optional text (a score with no
/// stated gap is still usable).
fn parse_roundedness(value: &Value) -> Option<Roundedness> {
    if !value.is_object() {
        return None;
    }
    let score = |key: &str| -> Option<u8> {
        value
            .get(key)
            .and_then(Value::as_i64)
            .map(|n| n.clamp(0, 100) as u8)
    };
    // All four structural dimensions are required — a partial score is not a
    // roundedness reading, so degrade rather than invent the missing axes.
    let consistency = score("consistency")?;
    let definitional_clarity = score("definitional_clarity")?;
    let completeness = score("completeness")?;
    let coherence = score("coherence")?;
    let limiting_gap = value
        .get("limiting_gap")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    Some(Roundedness {
        consistency,
        definitional_clarity,
        completeness,
        coherence,
        limiting_gap,
    })
}

/// The no-answer-supplied guarantee for the global synopsis. The observer must
/// never hand one of the user's own recorded positions back as if it were
/// guidance, so if a field reproduces a position verbatim (case-insensitively)
/// we replace it with a neutral placeholder. Empty positions are ignored
/// (nothing to leak).
fn scrub_supplied_position(field: &str, positions: &[String]) -> String {
    let candidate = field.trim();
    for position in positions {
        let position = position.trim();
        if !position.is_empty() && candidate.eq_ignore_ascii_case(position) {
            return "(withheld: the observer does not supply your answer)".to_string();
        }
    }
    field.to_string()
}

/// The offline / degraded synopsis: a minimal *structural* summary derived
/// purely from the [`SessionArc`]. It invents no belief content — it only
/// restates the recorded positions, counts the turns, surfaces any in-session
/// tensions, and names the questions left without an answer.
pub fn structural_synopsis(arc: &SessionArc) -> SessionSynopsis {
    let positions: Vec<String> = arc
        .turns
        .iter()
        .filter(|turn| !turn.position.is_empty())
        .map(|turn| {
            let asked = first_sentence(&turn.question);
            if asked.is_empty() {
                turn.position.clone()
            } else {
                format!("On \"{asked}\": {}", turn.position)
            }
        })
        .collect();

    let answered = arc
        .turns
        .iter()
        .filter(|turn| !turn.position.is_empty())
        .count();
    let branches: std::collections::BTreeSet<&str> =
        arc.turns.iter().map(|turn| turn.branch.as_str()).collect();
    let evolution = if answered == 0 {
        "No positions recorded yet.".to_string()
    } else if branches.len() > 1 {
        format!(
            "Recorded {answered} position(s) across {} branch(es).",
            branches.len()
        )
    } else {
        format!("Recorded {answered} position(s) along one line of inquiry.")
    };

    let consistency = if arc.tensions.is_empty() {
        "Offline summary: no tensions were surfaced in-session; re-read the positions for consistency yourself.".to_string()
    } else {
        format!(
            "Tensions surfaced in-session: {}. The observer names them but does not resolve them.",
            arc.tensions.join("; ")
        )
    };

    let recent = match arc
        .turns
        .iter()
        .rev()
        .find(|turn| !turn.position.is_empty())
    {
        Some(last) => format!(
            "Most recent position — on \"{}\": {}",
            first_sentence(&last.question),
            last.position
        ),
        None => "No position recorded — nothing to stand on yet.".to_string(),
    };
    // trace:STORY-159 | ai:claude — even offline, the goal still ORIENTS the
    // standing deterministically: it frames the recent position as progress
    // toward resolving the goal, without fabricating a (needs-LLM) score.
    let standing = match arc.goal.as_deref().map(str::trim).filter(|g| !g.is_empty()) {
        Some(goal) => format!("Goal: \"{goal}\". {recent}"),
        None => recent,
    };

    let open_threads: Vec<String> = arc
        .turns
        .iter()
        .filter(|turn| turn.position.is_empty() && !turn.question.is_empty())
        .map(|turn| format!("Unanswered: \"{}\"", first_sentence(&turn.question)))
        .collect();

    SessionSynopsis {
        positions,
        evolution,
        consistency,
        standing,
        open_threads,
        engagement:
            "Offline reading: re-read your positions and check each one is clear, consistent, and precise."
                .to_string(),
        // trace:STORY-155 | ai:claude — no fabricated score offline; the render
        // surfaces a "needs LLM" note in its place.
        roundedness: None,
        // trace:STORY-159 | ai:claude — carry the goal so even the offline render
        // shows the convergence target.
        goal: arc.goal.clone(),
        // trace:STORY-161 | ai:claude — carry the mode so the offline render still
        // frames the (needs-LLM) verdict by mode.
        mode: arc.mode,
        degraded: true,
    }
}

/// The first sentence (or the whole string if it has no terminator), trimmed,
/// used to keep the structural summary compact without echoing a long prompt.
fn first_sentence(text: &str) -> String {
    let trimmed = text.trim();
    trimmed
        .split_inclusive(['.', '?', '!'])
        .next()
        .unwrap_or(trimmed)
        .trim()
        .to_string()
}

/// Render a [`SessionSynopsis`] as a clearly-labeled META voice, visually
/// distinct (style::meta), mirroring the per-exchange observer's rendering.
/// Belief-neutral and clarify-only: it lists positions, describes the arc, names
/// tensions, and lists open threads — it never advocates a belief. Pure over the
/// buffer + synopsis, so it is unit-testable without a live LLM.
pub fn render_synopsis(synopsis: &SessionSynopsis, output: &mut impl Write) -> Result<()> {
    // trace:STORY-161 | ai:claude — in debate mode the header pins the
    // better-argued-case framing (argument STRUCTURE, never which belief is true);
    // Socratic mode keeps the original belief-neutral reading header.
    let header = match (synopsis.mode, synopsis.degraded) {
        (crate::strategy::SessionMode::Debate, true) => {
            "META (synopsis, offline) — debate mode: which case was better-ARGUED (argument STRUCTURE), never which belief is true:"
        }
        (crate::strategy::SessionMode::Debate, false) => {
            "META (synopsis) — debate mode: which case was better-ARGUED (argument STRUCTURE), never which belief is true:"
        }
        (crate::strategy::SessionMode::Socratic, true) => {
            "META (synopsis, offline) — a belief-neutral reading of this session:"
        }
        (crate::strategy::SessionMode::Socratic, false) => {
            "META (synopsis) — a belief-neutral reading of this session:"
        }
    };
    writeln!(
        output,
        "\n{}",
        crate::style::paint(crate::style::meta(), header)
    )?;

    let line = |label: &str, body: &str, output: &mut dyn Write| -> Result<()> {
        if !body.trim().is_empty() {
            writeln!(
                output,
                "{}",
                crate::style::paint(crate::style::meta(), &format!("  {label}: {body}"))
            )?;
        }
        Ok(())
    };
    let bullets = |label: &str, items: &[String], output: &mut dyn Write| -> Result<()> {
        if !items.is_empty() {
            writeln!(
                output,
                "{}",
                crate::style::paint(crate::style::meta(), &format!("  {label}:"))
            )?;
            for item in items {
                writeln!(
                    output,
                    "{}",
                    crate::style::paint(crate::style::meta(), &format!("    - {item}"))
                )?;
            }
        }
        Ok(())
    };

    // trace:STORY-159 | ai:claude — surface the goal at the top of the synopsis
    // when one is set, so the reader sees the convergence target the roundedness
    // is measured against. Belief-neutral: it names the question being settled.
    if let Some(goal) = synopsis
        .goal
        .as_deref()
        .map(str::trim)
        .filter(|g| !g.is_empty())
    {
        line("Goal", goal, output)?;
    }
    bullets("Positions taken", &synopsis.positions, output)?;
    line("How they evolved", &synopsis.evolution, output)?;
    line("Internal consistency", &synopsis.consistency, output)?;
    line("Where you stand", &synopsis.standing, output)?;
    bullets("Still unresolved", &synopsis.open_threads, output)?;
    line("Engagement", &synopsis.engagement, output)?;
    render_roundedness(synopsis, output)?;
    Ok(())
}

// trace:STORY-155 | ai:claude
/// Render the belief-neutral roundedness block: the composite %, the
/// per-dimension breakdown, and the single limiting gap. When the synopsis
/// carries no score (offline / degraded), surfaces a "needs LLM" note in its
/// place rather than a fabricated number. Pure over the synopsis, so the score
/// rendering is unit-testable without a live LLM.
fn render_roundedness(synopsis: &SessionSynopsis, output: &mut impl Write) -> Result<()> {
    let meta = crate::style::meta();
    let Some(rounded) = &synopsis.roundedness else {
        // Belief-neutral, no fabricated score: structural-only sessions get a
        // note pointing at why no percentage is shown.
        let note = if synopsis.degraded {
            "  Roundedness: needs an LLM to score — offline, no structural % is fabricated."
        } else {
            "  Roundedness: not scored for this session."
        };
        writeln!(output, "{}", crate::style::paint(meta, note))?;
        return Ok(());
    };

    writeln!(
        output,
        "{}",
        crate::style::paint(
            meta,
            &format!(
                "  Roundedness: {}% (belief-neutral — structure, not correctness):",
                rounded.composite()
            )
        )
    )?;
    for (label, score) in rounded.dimensions() {
        writeln!(
            output,
            "{}",
            crate::style::paint(meta, &format!("    - {label}: {score}%"))
        )?;
    }
    let (weakest_label, _) = rounded.weakest_dimension();
    let gap = if rounded.limiting_gap.trim().is_empty() {
        format!("the {weakest_label} dimension is the one to shore up.")
    } else {
        rounded.limiting_gap.trim().to_string()
    };
    // trace:STORY-156 | ai:claude
    // At/above the well-rounded threshold the synopsis OFFERS to conclude
    // (coherent, consistent, addresses the main objections) and lets the user
    // decide — preserving agency. Below threshold it just names the gap to
    // close. Belief-neutral throughout: "conclude" summarizes the user's OWN
    // well-formed position, never advocating a belief.
    if rounded.is_well_rounded() {
        writeln!(
            output,
            "{}",
            crate::style::paint(
                meta,
                &format!(
                    "    Your position is ~{}% rounded (coherent, consistent, addresses the main objections).",
                    rounded.composite()
                )
            )
        )?;
        writeln!(
            output,
            "{}",
            crate::style::paint(
                meta,
                "    Conclude with a summary of where you've landed, or keep probing edge cases?"
            )
        )?;
        // The remaining gap is still worth naming as the natural edge case to
        // probe if the user keeps going.
        writeln!(
            output,
            "{}",
            crate::style::paint(meta, &format!("    Edge case to probe: {gap}"))
        )?;
    } else {
        writeln!(
            output,
            "{}",
            crate::style::paint(meta, &format!("    Limiting gap: {gap}"))
        )?;
    }
    Ok(())
}

// trace:STORY-156 | ai:claude
/// Render the FINAL belief-neutral conclusion when the user accepts the offer to
/// conclude. It summarizes the user's OWN well-formed position from the
/// synopsis + the arc — never advocating a belief, never asserting the position
/// is correct, only reflecting where the user has landed. Pure over the synopsis
/// + arc, so it is unit-testable without a live LLM.
///
/// `arc` supplies the user's own most-recent recorded standing as a structural
/// fallback when the LLM synopsis withheld or omitted a standing line.
pub fn render_conclusion(
    synopsis: &SessionSynopsis,
    arc: &SessionArc,
    output: &mut impl Write,
) -> Result<()> {
    let meta = crate::style::meta();
    writeln!(
        output,
        "\n{}",
        crate::style::paint(
            meta,
            "META (conclusion) — a belief-neutral summary of where YOUR position landed:"
        )
    )?;

    // Prefer the LLM's standing read; fall back to the user's own recorded
    // words so a withheld/empty standing never leaves the conclusion blank.
    let standing = if synopsis.standing.trim().is_empty() {
        concluding_standing(arc).unwrap_or_default()
    } else {
        synopsis.standing.trim().to_string()
    };
    if !standing.is_empty() {
        writeln!(
            output,
            "{}",
            crate::style::paint(meta, &format!("  Where you stand: {standing}"))
        )?;
    }

    if !synopsis.positions.is_empty() {
        writeln!(
            output,
            "{}",
            crate::style::paint(meta, "  The position you built, in your own terms:")
        )?;
        for position in &synopsis.positions {
            writeln!(
                output,
                "{}",
                crate::style::paint(meta, &format!("    - {position}"))
            )?;
        }
    }

    if let Some(composite) = synopsis.composite() {
        writeln!(
            output,
            "{}",
            crate::style::paint(
                meta,
                &format!(
                    "  Roundedness: ~{composite}% (belief-neutral — this reflects how well-FORMED your case is, not whether it is \"right\")."
                )
            )
        )?;
    }

    // Open threads remain genuinely open — concluding is not closing them for
    // the user, just marking a coherent stopping point they can resume from.
    if !synopsis.open_threads.is_empty() {
        writeln!(
            output,
            "{}",
            crate::style::paint(
                meta,
                "  Still open if you return — concluding does not settle these for you:"
            )
        )?;
        for thread in &synopsis.open_threads {
            writeln!(
                output,
                "{}",
                crate::style::paint(meta, &format!("    - {thread}"))
            )?;
        }
    }
    Ok(())
}

/// Which LLM backend the standalone synopsis command uses.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SynopsisBackend {
    ClaudeCli,
    Anthropic,
    Disabled,
}

/// Parsed flags for the `quizdom session synopsis <id>` command.
///
/// Mirrors `session show`'s dispatch: `<id>` (or `--session`) names the session;
/// `--log` points at one explicit file; `--user` selects whose sessions
/// directory the id resolves against; `--branch` folds to a single branch.
#[derive(Debug)]
struct SynopsisConfig {
    user_id: String,
    session_id: Option<String>,
    log_path: Option<PathBuf>,
    branch: Option<String>,
    backend: SynopsisBackend,
}

impl SynopsisConfig {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut user_id = DEFAULT_USER.to_string();
        let mut session_id = None;
        let mut log_path = None;
        let mut branch = None;
        let mut backend = env_backend();
        let mut args = args.into_iter().peekable();

        // Strip the `session synopsis` command prefix the dispatcher passes.
        if matches!(args.peek().map(String::as_str), Some("session")) {
            args.next();
        }
        if matches!(args.peek().map(String::as_str), Some("synopsis")) {
            args.next();
        }

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--user" => user_id = next_arg(&mut args, "--user")?,
                "--session" => {
                    session_id = Some(normalize_session_id(&next_arg(&mut args, "--session")?))
                }
                "--log" => log_path = Some(PathBuf::from(next_arg(&mut args, "--log")?)),
                "--branch" => branch = Some(next_arg(&mut args, "--branch")?),
                "--backend" => backend = parse_backend(&next_arg(&mut args, "--backend")?)?,
                "--no-llm" => backend = SynopsisBackend::Disabled,
                "--help" | "-h" => return Err(QuizdomError::Usage(synopsis_usage())),
                other if !other.starts_with('-') => {
                    session_id = Some(normalize_session_id(other));
                }
                other => {
                    return Err(QuizdomError::Usage(format!(
                        "unknown argument: {other}\n{}",
                        synopsis_usage()
                    )))
                }
            }
        }

        Ok(Self {
            user_id,
            session_id,
            log_path,
            branch,
            backend,
        })
    }

    /// The single log file to read: an explicit `--log`, otherwise the `<id>`
    /// resolved against the user's sessions directory. Mirrors `session show`.
    fn log_path(&self) -> Result<PathBuf> {
        if let Some(log_path) = &self.log_path {
            return Ok(log_path.clone());
        }
        let session_id = self.session_id.as_ref().ok_or_else(|| {
            QuizdomError::Usage(format!(
                "session synopsis requires a session id\n{}",
                synopsis_usage()
            ))
        })?;
        Ok(PathBuf::from("data")
            .join("users")
            .join(&self.user_id)
            .join("sessions")
            .join(format!("{session_id}.jsonl")))
    }
}

fn synopsis_usage() -> String {
    "usage: quizdom session synopsis <session-id> [--user local-user] [--branch main] [--log path] [--backend claude-cli|anthropic|none] [--no-llm]"
        .to_string()
}

fn env_backend() -> SynopsisBackend {
    std::env::var("QUIZDOM_BACKEND")
        .ok()
        .and_then(|value| parse_backend(&value).ok())
        .unwrap_or(SynopsisBackend::ClaudeCli)
}

fn parse_backend(value: &str) -> Result<SynopsisBackend> {
    match value {
        "claude-cli" | "claude_cli" | "claude" => Ok(SynopsisBackend::ClaudeCli),
        "anthropic" => Ok(SynopsisBackend::Anthropic),
        "none" | "off" | "disabled" => Ok(SynopsisBackend::Disabled),
        other => Err(QuizdomError::Usage(format!(
            "unknown LLM backend: {other}; expected claude-cli, anthropic, or none"
        ))),
    }
}

fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| QuizdomError::Usage(format!("{name} requires a value")))
}

/// Produce a synopsis for `arc` with the command's configured backend, degrading
/// to the structural summary when the backend is disabled or unavailable.
fn synopsize_with_backend(backend: SynopsisBackend, arc: &SessionArc) -> SessionSynopsis {
    match backend {
        SynopsisBackend::Disabled => structural_synopsis(arc),
        SynopsisBackend::ClaudeCli => {
            let client = llm::ClaudeCliClient::from_env();
            synopsize(&client, arc)
        }
        SynopsisBackend::Anthropic => match llm::AnthropicClient::from_env() {
            Ok(client) => synopsize(&client, arc),
            // No API key / misconfigured: degrade to the structural summary
            // rather than failing the command.
            Err(_) => structural_synopsis(arc),
        },
    }
}

/// Entry point for the standalone `quizdom session synopsis <id>` command.
/// Resolves the session's log file, reads its arc, and renders a belief-neutral
/// global synopsis. Read-only: like `session show`, it never mutates anything.
pub fn run_session_synopsis(
    args: impl IntoIterator<Item = String>,
    output: &mut impl Write,
) -> Result<()> {
    let config = SynopsisConfig::parse(args)?;
    let path = config.log_path()?;
    if !path.exists() {
        return Err(QuizdomError::Usage(format!(
            "no session log at {}",
            path.display()
        )));
    }
    let file = std::fs::File::open(&path)?;
    let arc = arc_from_session_log(file, config.branch.as_deref())?;

    let header = if arc.session_id.is_empty() {
        "Session synopsis".to_string()
    } else if arc.user_id.is_empty() {
        format!("Synopsis for {}", arc.session_id)
    } else {
        format!("Synopsis for {} · user {}", arc.session_id, arc.user_id)
    };
    writeln!(output, "{header}")?;

    if arc.is_empty() {
        writeln!(output, "(no recorded positions to summarize)")?;
        return Ok(());
    }

    let synopsis = synopsize_with_backend(config.backend, &arc);
    render_synopsis(&synopsis, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::{LLMError, LLMFuture, Message, ToolDef};
    use std::cell::RefCell;

    struct MockClient {
        response: RefCell<Option<std::result::Result<String, LLMError>>>,
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

    // A two-turn, two-branch session: a free-text position, a yes/no position,
    // a settled definition, and a surfaced tension — enough to exercise every
    // arc field and the branch filter.
    const SAMPLE_LOG: &str = r#"
{"event_type":"session_started","session_id":"sess-7","user_id":"ada","branch_id":"main","strategy":"deterministic"}
{"event_type":"question_presented","session_id":"sess-7","user_id":"ada","branch_id":"main","turn":1,"question_ref":"Q-1","question_text":"Is free will real?"}
{"event_type":"term_interpreted","session_id":"sess-7","user_id":"ada","branch_id":"main","turn":1,"term":"free","adopted_definition":"uncaused"}
{"event_type":"answer_recorded","session_id":"sess-7","user_id":"ada","branch_id":"main","turn":1,"question_ref":"Q-1","raw_answer":"yes, obviously","normalized_answer":"free-text"}
{"event_type":"contradiction_resolved","session_id":"sess-7","user_id":"ada","branch_id":"main","turn":1,"left_belief":"choices are caused","right_belief":"choices are free","kept_side":"right"}
{"event_type":"question_presented","session_id":"sess-7","user_id":"ada","branch_id":"main","turn":2,"question_ref":"Q-2","question_text":"Can a caused choice be free?"}
{"event_type":"answer_recorded","session_id":"sess-7","user_id":"ada","branch_id":"main","turn":2,"question_ref":"Q-2","raw_answer":"no","normalized_answer":"no"}
{"event_type":"question_presented","session_id":"sess-7","user_id":"ada","branch_id":"agree","turn":3,"question_ref":"Q-3","question_text":"Is determinism compatible with freedom?"}
{"event_type":"answer_recorded","session_id":"sess-7","user_id":"ada","branch_id":"agree","turn":3,"question_ref":"Q-3","raw_answer":"yes","normalized_answer":"yes"}
{"event_type":"session_ended","session_id":"sess-7","user_id":"ada","branch_id":"main","turn":2,"summary":"explored 3 questions"}
"#;

    fn arc(branch: Option<&str>) -> SessionArc {
        arc_from_session_log(SAMPLE_LOG.as_bytes(), branch).expect("arc should parse")
    }

    fn strings<const N: usize>(args: [&str; N]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn arc_extracts_positions_definitions_and_tensions() {
        let arc = arc(None);
        assert_eq!(arc.session_id, "sess-7");
        assert_eq!(arc.user_id, "ada");
        assert_eq!(arc.turns.len(), 3);
        assert_eq!(arc.turns[0].question, "Is free will real?");
        assert_eq!(arc.turns[0].position, "yes, obviously");
        assert_eq!(
            arc.definitions,
            vec![("free".to_string(), "uncaused".to_string())]
        );
        assert_eq!(arc.tensions.len(), 1);
        assert!(arc.tensions[0].contains("choices are caused"));
    }

    #[test]
    fn arc_branch_filter_keeps_one_branch() {
        let arc = arc(Some("main"));
        assert_eq!(arc.turns.len(), 2);
        assert!(arc.turns.iter().all(|turn| turn.branch == "main"));
    }

    #[test]
    fn arc_of_an_empty_log_is_empty() {
        let arc = arc_from_session_log("".as_bytes(), None).expect("empty arc");
        assert!(arc.is_empty());
    }

    #[test]
    fn synopsizes_from_the_llm_belief_neutrally() {
        let client = MockClient::ok(
            r#"{
                "positions": ["Free will is real", "A caused choice is not free"],
                "evolution": "Started confident, then drew a line at caused choices.",
                "consistency": "Tension between 'free' as uncaused and a wish to keep some freedom under causation.",
                "standing": "Holds free will is real but only when uncaused.",
                "open_threads": ["Whether any choice is truly uncaused"],
                "engagement": "Clear positions, but the key term shifts, so precision wavers."
            }"#,
        );
        let synopsis = synopsize(&client, &arc(None));
        assert!(!synopsis.degraded);
        assert_eq!(synopsis.positions.len(), 2);
        assert!(synopsis.evolution.contains("caused"));
        assert!(synopsis.consistency.contains("Tension"));
        assert_eq!(synopsis.open_threads.len(), 1);
        assert!(synopsis.engagement.contains("precision") || synopsis.engagement.contains("term"));
    }

    #[test]
    fn the_prompt_pins_belief_neutrality() {
        let client = MockClient::ok(r#"{"standing":"s"}"#);
        let _ = synopsize(&client, &arc(None));
        let prompt = client.last_prompt.borrow().clone().unwrap();
        assert!(prompt.contains("Do NOT supply an answer"));
        assert!(prompt.contains("grade which belief is right"));
        // The arc text the model sees is the user's own positions.
        assert!(prompt.contains("yes, obviously"));
    }

    #[test]
    fn never_supplies_the_users_position() {
        // A misbehaving model that echoes one of the user's positions verbatim
        // into a field must NOT have it pass through as guidance.
        let client = MockClient::ok(
            r#"{"positions":["yes, obviously"],"standing":"yes, obviously","evolution":"e","consistency":"c","open_threads":[],"engagement":"x"}"#,
        );
        let synopsis = synopsize(&client, &arc(None));
        assert!(
            synopsis.standing.contains("withheld"),
            "the verbatim position must be withheld, got: {}",
            synopsis.standing
        );
        assert!(
            synopsis.positions.iter().all(|p| p != "yes, obviously"),
            "positions must not echo the user's verbatim answer"
        );
    }

    #[test]
    fn degrades_to_a_structural_summary_when_offline() {
        let client = MockClient::err();
        let synopsis = synopsize(&client, &arc(None));
        assert!(synopsis.degraded);
        // The structural summary names the recorded positions, the in-session
        // tension, and where the user last stood — inventing no belief content.
        assert!(synopsis
            .positions
            .iter()
            .any(|p| p.contains("yes, obviously")));
        assert!(synopsis.consistency.contains("choices are caused"));
        assert!(synopsis.standing.contains("no") || synopsis.standing.contains("Most recent"));
    }

    #[test]
    fn degrades_when_the_llm_returns_unparseable_text() {
        let client = MockClient::ok("not json at all");
        let synopsis = synopsize(&client, &arc(None));
        assert!(synopsis.degraded);
    }

    #[test]
    fn degrades_when_the_llm_returns_an_empty_object() {
        let client = MockClient::ok("{}");
        let synopsis = synopsize(&client, &arc(None));
        assert!(synopsis.degraded);
    }

    #[test]
    fn structural_summary_lists_open_threads_for_unanswered_questions() {
        let mut arc = arc(None);
        arc.turns.push(SessionTurn {
            turn: 4,
            branch: "main".to_string(),
            question: "What would change your mind?".to_string(),
            position: String::new(),
        });
        let synopsis = structural_synopsis(&arc);
        assert!(synopsis
            .open_threads
            .iter()
            .any(|thread| thread.contains("What would change your mind?")));
    }

    #[test]
    fn render_marks_offline_and_lists_positions() {
        let synopsis = structural_synopsis(&arc(None));
        let mut out = Vec::new();
        render_synopsis(&synopsis, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("META (synopsis, offline)"));
        assert!(rendered.contains("Positions taken:"));
        assert!(rendered.contains("yes, obviously"));
        // Belief-neutral: it never tells the user which side is right.
        assert!(!rendered.to_lowercase().contains("you should believe"));
    }

    #[test]
    fn run_session_synopsis_renders_an_explicit_log_offline() {
        let path = temp_log(SAMPLE_LOG);
        let mut out = Vec::new();
        run_session_synopsis(
            strings([
                "session",
                "synopsis",
                "--log",
                path.to_str().unwrap(),
                "--no-llm",
            ]),
            &mut out,
        )
        .expect("run should succeed");
        std::fs::remove_file(&path).ok();
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("Synopsis for sess-7 · user ada"));
        assert!(rendered.contains("META (synopsis, offline)"));
        assert!(rendered.contains("Positions taken:"));
    }

    #[test]
    fn run_session_synopsis_empty_log_reports_nothing_to_summarize() {
        let path = temp_log("");
        let mut out = Vec::new();
        run_session_synopsis(
            strings([
                "session",
                "synopsis",
                "--log",
                path.to_str().unwrap(),
                "--no-llm",
            ]),
            &mut out,
        )
        .expect("run should succeed");
        std::fs::remove_file(&path).ok();
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("(no recorded positions to summarize)"));
    }

    #[test]
    fn run_session_synopsis_missing_log_is_a_usage_error() {
        let mut out = Vec::new();
        let error = run_session_synopsis(
            strings([
                "session",
                "synopsis",
                "--log",
                "/tmp/does-not-exist-syn.jsonl",
            ]),
            &mut out,
        )
        .unwrap_err();
        assert!(matches!(error, QuizdomError::Usage(_)));
    }

    #[test]
    fn parse_takes_positional_id_and_flags() {
        let config = SynopsisConfig::parse(strings([
            "session", "synopsis", "1234", "--user", "ada", "--branch", "main",
        ]))
        .expect("parse should succeed");
        assert_eq!(config.user_id, "ada");
        assert_eq!(config.session_id.as_deref(), Some("sess-1234"));
        assert_eq!(config.branch.as_deref(), Some("main"));
    }

    #[test]
    fn parse_no_llm_disables_the_backend() {
        let config = SynopsisConfig::parse(strings(["session", "synopsis", "sess-1", "--no-llm"]))
            .expect("parse");
        assert_eq!(config.backend, SynopsisBackend::Disabled);
    }

    #[test]
    fn log_path_resolves_id_against_user_dir() {
        let config =
            SynopsisConfig::parse(strings(["session", "synopsis", "sess-1", "--user", "ada"]))
                .unwrap();
        assert_eq!(
            config.log_path().expect("path"),
            PathBuf::from("data/users/ada/sessions/sess-1.jsonl")
        );
    }

    #[test]
    fn log_path_without_id_is_a_usage_error() {
        let config = SynopsisConfig::parse(strings(["session", "synopsis"])).unwrap();
        assert!(matches!(config.log_path(), Err(QuizdomError::Usage(_))));
    }

    // ---- STORY-155: belief-neutral roundedness score ----------------------

    /// A synopsis JSON body carrying a full roundedness object, parameterized by
    /// the four sub-scores and the limiting gap, so the belief-neutrality test
    /// can vary the *position* text while holding the structural shape fixed.
    fn synopsis_body(
        standing: &str,
        consistency: u8,
        clarity: u8,
        completeness: u8,
        coherence: u8,
        gap: &str,
    ) -> String {
        format!(
            r#"{{"positions":["a position"],"evolution":"e","consistency":"c","standing":"{standing}","open_threads":[],"engagement":"x","roundedness":{{"consistency":{consistency},"definitional_clarity":{clarity},"completeness":{completeness},"coherence":{coherence},"limiting_gap":"{gap}"}}}}"#
        )
    }

    #[test]
    fn synopsis_carries_the_roundedness_score_and_limiting_gap() {
        let client = MockClient::ok(&synopsis_body(
            "determinism is true",
            90,
            80,
            60,
            70,
            "you have not addressed whether determinism undermines responsibility",
        ));
        let synopsis = synopsize(&client, &arc(None));
        assert!(!synopsis.degraded);
        let rounded = synopsis.roundedness.expect("roundedness scored");
        assert_eq!(rounded.consistency, 90);
        assert_eq!(rounded.definitional_clarity, 80);
        assert_eq!(rounded.completeness, 60);
        assert_eq!(rounded.coherence, 70);
        // Composite is the rounded mean: (90+80+60+70)/4 = 75.
        assert_eq!(rounded.composite(), 75);
        // The limiting gap names the weakest dimension (completeness, 60).
        assert_eq!(rounded.weakest_dimension(), ("completeness", 60));
        assert!(rounded.limiting_gap.contains("responsibility"));
    }

    #[test]
    fn the_prompt_pins_roundedness_to_structure_not_correctness() {
        let client = MockClient::ok(r#"{"standing":"s"}"#);
        let _ = synopsize(&client, &arc(None));
        let prompt = client.last_prompt.borrow().clone().unwrap();
        assert!(prompt.contains("roundedness"));
        assert!(prompt.contains("two opposite well-formed positions must score comparably"));
        assert!(prompt.contains("STRUCTURE only"));
    }

    #[test]
    fn roundedness_is_belief_neutral_opposite_positions_score_comparably() {
        // The SAME structural quality (identical sub-scores) on OPPOSITE
        // positions must yield the SAME composite — the score reads STRUCTURE,
        // never which belief is "right".
        let pro = MockClient::ok(&synopsis_body(
            "free will is real",
            85,
            85,
            85,
            85,
            "edge cases on coercion",
        ));
        let con = MockClient::ok(&synopsis_body(
            "free will is an illusion",
            85,
            85,
            85,
            85,
            "edge cases on coercion",
        ));
        let pro = synopsize(&pro, &arc(None)).roundedness.expect("pro scored");
        let con = synopsize(&con, &arc(None)).roundedness.expect("con scored");
        assert_eq!(pro.composite(), con.composite());
        assert_eq!(pro.composite(), 85);
    }

    #[test]
    fn roundedness_clamps_out_of_range_scores() {
        let client = MockClient::ok(
            r#"{"standing":"s","roundedness":{"consistency":150,"definitional_clarity":-20,"completeness":100,"coherence":0,"limiting_gap":"g"}}"#,
        );
        let rounded = synopsize(&client, &arc(None)).roundedness.expect("scored");
        assert_eq!(rounded.consistency, 100);
        assert_eq!(rounded.definitional_clarity, 0);
        assert_eq!(rounded.completeness, 100);
        assert_eq!(rounded.coherence, 0);
    }

    #[test]
    fn roundedness_is_none_when_the_score_object_is_partial() {
        // A score missing a dimension is not a roundedness reading — degrade to
        // no score rather than fabricate the missing axis.
        let client = MockClient::ok(
            r#"{"standing":"s","roundedness":{"consistency":80,"completeness":80}}"#,
        );
        let synopsis = synopsize(&client, &arc(None));
        // The rest of the synopsis still parsed (standing present), so it is not
        // degraded — but the partial score yields no number.
        assert!(!synopsis.degraded);
        assert!(synopsis.roundedness.is_none());
    }

    #[test]
    fn offline_synopsis_fabricates_no_score() {
        let synopsis = structural_synopsis(&arc(None));
        assert!(synopsis.degraded);
        assert!(
            synopsis.roundedness.is_none(),
            "offline must not fabricate a roundedness score"
        );
    }

    #[test]
    fn render_shows_the_composite_breakdown_and_limiting_gap() {
        let client = MockClient::ok(&synopsis_body(
            "determinism is true",
            90,
            80,
            60,
            70,
            "address whether determinism undermines responsibility",
        ));
        let synopsis = synopsize(&client, &arc(None));
        let mut out = Vec::new();
        render_synopsis(&synopsis, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("Roundedness: 75%"));
        assert!(rendered.contains("consistency: 90%"));
        assert!(rendered.contains("definitional clarity: 80%"));
        assert!(rendered.contains("completeness: 60%"));
        assert!(rendered.contains("coherence: 70%"));
        assert!(rendered
            .contains("Limiting gap: address whether determinism undermines responsibility"));
        // Belief-neutral: it never tells the user which side is right.
        assert!(!rendered.to_lowercase().contains("you should believe"));
    }

    #[test]
    fn render_offline_shows_needs_llm_note_not_a_number() {
        let synopsis = structural_synopsis(&arc(None));
        let mut out = Vec::new();
        render_synopsis(&synopsis, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("needs an LLM to score"));
        // No fabricated roundedness percentage offline: the score line never
        // carries a "NN%" composite the way the LLM-scored render does.
        assert!(!rendered.contains("Roundedness: 0%"));
        for n in 0..=100u8 {
            assert!(
                !rendered.contains(&format!("Roundedness: {n}%")),
                "offline render must not fabricate a roundedness percentage"
            );
        }
    }

    #[test]
    fn render_falls_back_to_weakest_dimension_when_gap_is_blank() {
        let client = MockClient::ok(&synopsis_body("s", 90, 50, 80, 80, ""));
        let synopsis = synopsize(&client, &arc(None));
        let mut out = Vec::new();
        render_synopsis(&synopsis, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");
        // definitional clarity (50) is weakest, so the gap line names it.
        assert!(rendered.contains("Limiting gap: the definitional clarity dimension"));
    }

    // ---- STORY-156: offer-to-conclude at the well-rounded threshold ---------

    #[test]
    fn well_rounded_threshold_gates_the_offer() {
        // trace:STORY-156 | ai:claude
        // At/above the threshold the synopsis offers to conclude; below it does
        // not. The boundary is inclusive.
        let at = MockClient::ok(&synopsis_body(
            "s",
            WELL_ROUNDED_THRESHOLD,
            WELL_ROUNDED_THRESHOLD,
            WELL_ROUNDED_THRESHOLD,
            WELL_ROUNDED_THRESHOLD,
            "edge cases",
        ));
        let synopsis = synopsize(&at, &arc(None));
        assert!(synopsis.offers_conclude(), "at threshold must offer");
        assert_eq!(synopsis.composite(), Some(WELL_ROUNDED_THRESHOLD));

        let below = WELL_ROUNDED_THRESHOLD - 5;
        let under = MockClient::ok(&synopsis_body("s", below, below, below, below, "the gap"));
        let synopsis = synopsize(&under, &arc(None));
        assert!(
            !synopsis.offers_conclude(),
            "below threshold must not offer"
        );
    }

    #[test]
    fn render_offers_conclude_at_or_above_threshold() {
        // trace:STORY-156 | ai:claude
        let client = MockClient::ok(&synopsis_body(
            "free will is real",
            95,
            90,
            85,
            90,
            "edge cases on coercion",
        ));
        let synopsis = synopsize(&client, &arc(None));
        let mut out = Vec::new();
        render_synopsis(&synopsis, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");
        // The OFFER line surfaces with the composite %, framed as a choice.
        assert!(rendered.contains("% rounded"));
        assert!(rendered.contains("Conclude with a summary"));
        assert!(rendered.contains("keep probing edge cases"));
        // Below-threshold "Limiting gap:" framing is NOT used when offering.
        assert!(!rendered.contains("Limiting gap:"));
        // Belief-neutral: never advocates a side.
        assert!(!rendered.to_lowercase().contains("you should believe"));
    }

    #[test]
    fn render_shows_gap_not_offer_below_threshold() {
        // trace:STORY-156 | ai:claude
        let client = MockClient::ok(&synopsis_body(
            "determinism is true",
            70,
            70,
            50,
            70,
            "address responsibility",
        ));
        let synopsis = synopsize(&client, &arc(None));
        assert!(!synopsis.offers_conclude());
        let mut out = Vec::new();
        render_synopsis(&synopsis, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("Limiting gap: address responsibility"));
        assert!(!rendered.contains("Conclude with a summary"));
    }

    #[test]
    fn offline_synopsis_never_offers_to_conclude() {
        // trace:STORY-156 | ai:claude — no score offline, so no offer is ever made.
        let synopsis = structural_synopsis(&arc(None));
        assert!(synopsis.degraded);
        assert!(!synopsis.offers_conclude());
        assert_eq!(synopsis.composite(), None);
    }

    #[test]
    fn opposite_well_formed_positions_both_offer_conclude() {
        // trace:STORY-156 | ai:claude — belief-neutral: two OPPOSITE positions
        // with identical structure both cross the threshold and both offer.
        let pro = MockClient::ok(&synopsis_body("free will is real", 90, 90, 90, 90, "g"));
        let con = MockClient::ok(&synopsis_body(
            "free will is an illusion",
            90,
            90,
            90,
            90,
            "g",
        ));
        assert!(synopsize(&pro, &arc(None)).offers_conclude());
        assert!(synopsize(&con, &arc(None)).offers_conclude());
    }

    #[test]
    fn conclusion_summarizes_the_users_own_position_belief_neutrally() {
        // trace:STORY-156 | ai:claude
        let client = MockClient::ok(&synopsis_body(
            "a coherent compatibilist standing",
            90,
            90,
            90,
            90,
            "g",
        ));
        let arc = arc(None);
        let synopsis = synopsize(&client, &arc);
        let mut out = Vec::new();
        render_conclusion(&synopsis, &arc, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("META (conclusion)"));
        // Summarizes the user's OWN standing + roundedness, framed as well-FORMED
        // (structure), never as correct.
        assert!(rendered.contains("Where you stand: a coherent compatibilist standing"));
        assert!(rendered.contains("Roundedness: ~90%"));
        assert!(rendered.contains("well-FORMED"));
        // Belief-neutral: never advocates which belief is right.
        let lc = rendered.to_lowercase();
        assert!(!lc.contains("you should believe"));
        assert!(!lc.contains("is correct"));
        assert!(!lc.contains("the right answer"));
    }

    #[test]
    fn conclusion_falls_back_to_the_users_own_recorded_standing() {
        // trace:STORY-156 | ai:claude — when the LLM withheld/omitted a standing,
        // the conclusion uses the user's OWN most-recent recorded words from the
        // arc rather than leaving the summary blank or inventing one.
        let arc = arc(None);
        let synopsis = SessionSynopsis {
            positions: Vec::new(),
            evolution: String::new(),
            consistency: String::new(),
            standing: String::new(),
            open_threads: Vec::new(),
            engagement: String::new(),
            roundedness: Some(Roundedness {
                consistency: 90,
                definitional_clarity: 90,
                completeness: 90,
                coherence: 90,
                limiting_gap: String::new(),
            }),
            goal: None,
            mode: crate::strategy::SessionMode::Socratic,
            degraded: false,
        };
        let mut out = Vec::new();
        render_conclusion(&synopsis, &arc, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");
        // The SAMPLE_LOG's last position (turn 3, branch agree) is "yes"; the
        // fallback echoes the user's own recorded standing.
        let expected = concluding_standing(&arc).expect("a recorded standing");
        assert!(rendered.contains(&expected));
    }

    // ---- STORY-159: goal-aware synopsis ------------------------------------

    // A session that set a goal at start, then UPDATED it in-session via a
    // `goal_set` event — exercising both goal sources and the "latest wins" rule.
    const GOAL_LOG: &str = r#"
{"event_type":"session_started","session_id":"sess-9","user_id":"ada","branch_id":"main","strategy":"deterministic","goal":"is free will real?"}
{"event_type":"question_presented","session_id":"sess-9","user_id":"ada","branch_id":"main","turn":1,"question_ref":"Q-1","question_text":"Is free will real?"}
{"event_type":"answer_recorded","session_id":"sess-9","user_id":"ada","branch_id":"main","turn":1,"question_ref":"Q-1","raw_answer":"yes","normalized_answer":"yes"}
{"event_type":"goal_set","session_id":"sess-9","user_id":"ada","branch_id":"main","turn":1,"goal":"can libertarian free will be held consistently?","source":"observer"}
{"event_type":"question_presented","session_id":"sess-9","user_id":"ada","branch_id":"main","turn":2,"question_ref":"Q-2","question_text":"Can a caused choice be free?"}
{"event_type":"answer_recorded","session_id":"sess-9","user_id":"ada","branch_id":"main","turn":2,"question_ref":"Q-2","raw_answer":"no","normalized_answer":"no"}
"#;

    fn goal_arc() -> SessionArc {
        arc_from_session_log(GOAL_LOG.as_bytes(), Some("main")).expect("arc parses")
    }

    #[test]
    fn arc_captures_the_goal_latest_wins() {
        // trace:STORY-159 | ai:claude — the goal is read from the start event and
        // then OVERRIDDEN by the later `goal_set` (in-session / Observer-proposed).
        let arc = goal_arc();
        assert_eq!(
            arc.goal.as_deref(),
            Some("can libertarian free will be held consistently?")
        );
    }

    #[test]
    fn arc_has_no_goal_when_none_was_set() {
        // The SAMPLE_LOG sets no goal — free-flowing.
        assert!(arc(None).goal.is_none());
    }

    #[test]
    fn synopsis_prompt_orients_roundedness_to_the_goal() {
        // trace:STORY-159 | ai:claude — when a goal is set, the prompt asks the
        // model to measure roundedness WITH RESPECT TO it, belief-neutrally.
        let client = MockClient::ok(r#"{"standing":"s"}"#);
        let _ = synopsize(&client, &goal_arc());
        let prompt = client.last_prompt.borrow().clone().unwrap();
        assert!(prompt.contains("can libertarian free will be held consistently?"));
        assert!(prompt.contains("Measure roundedness WITH RESPECT TO this goal"));
        assert!(prompt.contains("never which answer is true"));
    }

    #[test]
    fn synopsis_prompt_has_no_goal_preamble_when_free_flowing() {
        let client = MockClient::ok(r#"{"standing":"s"}"#);
        let _ = synopsize(&client, &arc(None));
        let prompt = client.last_prompt.borrow().clone().unwrap();
        assert!(!prompt.contains("Session goal"));
        assert!(!prompt.contains("Measure roundedness WITH RESPECT TO"));
    }

    #[test]
    fn synopsis_carries_and_renders_the_goal() {
        // trace:STORY-159 | ai:claude — the goal flows through to the synopsis and
        // is surfaced in the render so the convergence target is visible.
        let client = MockClient::ok(&synopsis_body("s", 90, 90, 90, 90, "g"));
        let synopsis = synopsize(&client, &goal_arc());
        assert_eq!(
            synopsis.goal.as_deref(),
            Some("can libertarian free will be held consistently?")
        );
        let mut out = Vec::new();
        render_synopsis(&synopsis, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("Goal: can libertarian free will be held consistently?"));
    }

    #[test]
    fn offline_synopsis_still_orients_to_the_goal() {
        // trace:STORY-159 | ai:claude — offline degrades to the structural summary
        // but the goal STILL orients deterministically: it frames the standing and
        // is carried through for the render, with no fabricated score.
        let synopsis = structural_synopsis(&goal_arc());
        assert!(synopsis.degraded);
        assert!(synopsis.roundedness.is_none());
        assert_eq!(
            synopsis.goal.as_deref(),
            Some("can libertarian free will be held consistently?")
        );
        assert!(synopsis
            .standing
            .contains("Goal: \"can libertarian free will be held consistently?\""));
    }

    // ---- STORY-161: debate-mode verdict -----------------------------------

    // trace:STORY-161 | ai:claude
    // A session started in DEBATE mode (the questioner steelmanned the opposing
    // side). The verdict must judge which CASE was better-argued, never truth.
    const DEBATE_LOG: &str = r#"
{"event_type":"session_started","session_id":"sess-d","user_id":"ada","branch_id":"main","strategy":"deterministic","mode":"debate"}
{"event_type":"question_presented","session_id":"sess-d","user_id":"ada","branch_id":"main","turn":1,"question_ref":"Q-1","question_text":"Is moral realism defensible?"}
{"event_type":"answer_recorded","session_id":"sess-d","user_id":"ada","branch_id":"main","turn":1,"question_ref":"Q-1","raw_answer":"yes","normalized_answer":"yes"}
"#;

    fn debate_arc() -> SessionArc {
        arc_from_session_log(DEBATE_LOG.as_bytes(), Some("main")).expect("arc parses")
    }

    #[test]
    fn arc_captures_the_debate_mode() {
        // trace:STORY-161 | ai:claude — the mode is read from the start event.
        assert_eq!(debate_arc().mode, crate::strategy::SessionMode::Debate);
        // The default-mode SAMPLE_LOG stays Socratic.
        assert_eq!(arc(None).mode, crate::strategy::SessionMode::Socratic);
    }

    #[test]
    fn debate_synopsis_prompt_judges_argument_structure_not_truth() {
        // trace:STORY-161 | ai:claude — the debate verdict prompt asks which CASE
        // was better-ARGUED and stays belief-neutral on which side is true.
        let client = MockClient::ok(r#"{"standing":"s"}"#);
        let _ = synopsize(&client, &debate_arc());
        let prompt = client.last_prompt.borrow().clone().unwrap();
        assert!(prompt.contains("DEBATE MODE"));
        assert!(prompt.contains("which CASE was better-ARGUED"));
        assert!(prompt.to_lowercase().contains("belief-neutral on truth"));
        assert!(prompt.contains("never say which side's belief is true"));
    }

    #[test]
    fn default_synopsis_prompt_has_no_debate_framing() {
        // trace:STORY-161 | ai:claude — Socratic (default) verdict is unchanged.
        let client = MockClient::ok(r#"{"standing":"s"}"#);
        let _ = synopsize(&client, &arc(None));
        let prompt = client.last_prompt.borrow().clone().unwrap();
        assert!(!prompt.contains("DEBATE MODE"));
        assert!(!prompt.contains("which CASE was better-ARGUED"));
    }

    #[test]
    fn debate_synopsis_scores_structure_and_renders_the_better_argued_framing() {
        // trace:STORY-161 | ai:claude — the debate verdict still scores STRUCTURE
        // (argument craft) and the render pins the better-argued-case framing,
        // never asserting a belief is true.
        let client = MockClient::ok(&synopsis_body(
            "the realist case is the tighter one",
            88,
            80,
            75,
            82,
            "the opposing objection on disagreement is unmet",
        ));
        let synopsis = synopsize(&client, &debate_arc());
        assert!(!synopsis.degraded);
        assert_eq!(synopsis.mode, crate::strategy::SessionMode::Debate);
        let rounded = synopsis.roundedness.as_ref().expect("structural score");
        assert_eq!(rounded.consistency, 88);
        let mut out = Vec::new();
        render_synopsis(&synopsis, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("which case was better-ARGUED"));
        assert!(rendered.contains("never which belief is true"));
    }

    #[test]
    fn offline_debate_synopsis_keeps_the_mode_and_framing() {
        // trace:STORY-161 | ai:claude — offline degrades structurally but the mode
        // (and its render framing) survives; no fabricated score.
        let synopsis = structural_synopsis(&debate_arc());
        assert!(synopsis.degraded);
        assert!(synopsis.roundedness.is_none());
        assert_eq!(synopsis.mode, crate::strategy::SessionMode::Debate);
        let mut out = Vec::new();
        render_synopsis(&synopsis, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("debate mode"));
        assert!(rendered.contains("never which belief is true"));
    }

    /// Write `contents` to a unique temp file and return its path.
    fn temp_log(contents: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "quizdom-synopsis-{}-{}.jsonl",
            std::process::id(),
            nonce
        ));
        std::fs::write(&path, contents).expect("write temp log");
        path
    }
}
