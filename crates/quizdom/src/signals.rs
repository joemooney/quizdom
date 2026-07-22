//! Derive per-question quality signals from session logs (STORY-68).
//!
//! Reads the JSONL session log the session loop writes and tallies, per
//! question, how often it was presented, how often it was punted, and how deep
//! a follow-up chain it seeded. Those tallies classify each question into a
//! [`QualitySignal`], which feeds the STORY-66 re-weighting engine.
//!
//! This is a pure, after-the-fact analysis pass: it only *reads* the log and,
//! when asked, drives the existing [`QuestionReweighter`]. It never edits the
//! session loop — mirroring the disjoint-from-the-loop discipline of STORY-66.

use crate::bank::{QuestionBank, StoreQuestionBank};
use crate::error::{QuizdomError, Result};
use crate::model::Question;
use crate::persist::{QuestionReweighter, StoreQuestionReweighter};
use crate::strategy::QualitySignal;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;

/// Default quizdom user whose session logs `quizdom curate` reads when no
/// `--user` is given — matching the session loop's own default.
const DEFAULT_USER: &str = "local-user";

/// A question punted on at least this fraction of the times it was answered is
/// treated as [`QualitySignal::Unhelpful`].
pub const PUNT_RATE_THRESHOLD: f64 = 0.5;

/// A question that seeds a follow-up chain at least this many hops long is
/// treated as [`QualitySignal::Insightful`] ("leads to deep branches").
pub const DEEP_BRANCH_DEPTH: u32 = 2;

/// Per-question tallies derived from a session log.
///
/// `answered` counts every `answer_recorded` event (punts included), so
/// `punted / answered` is a well-defined punt rate. `branch_depth` is the
/// longest chain of `next_question_selected` follow-ups reachable from this
/// question within the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionSignalStats {
    pub question_ref: String,
    pub presented: u32,
    pub answered: u32,
    pub punted: u32,
    pub branch_depth: u32,
}

impl QuestionSignalStats {
    /// Fraction of answers that were punts, in `[0.0, 1.0]`. `0.0` when the
    /// question was never answered (nothing to be unhelpful about yet).
    pub fn punt_rate(&self) -> f64 {
        if self.answered == 0 {
            0.0
        } else {
            self.punted as f64 / self.answered as f64
        }
    }

    /// Classify this question into a [`QualitySignal`] for re-weighting.
    ///
    /// A high punt rate is the strongest negative signal, so it wins over a
    /// deep branch; a question that seeds a deep follow-up chain is insightful;
    /// everything else is neutral (left unchanged by the engine).
    pub fn signal(&self) -> QualitySignal {
        if self.punted > 0 && self.punt_rate() >= PUNT_RATE_THRESHOLD {
            QualitySignal::Unhelpful
        } else if self.branch_depth >= DEEP_BRANCH_DEPTH {
            QualitySignal::Insightful
        } else {
            QualitySignal::Neutral
        }
    }
}

/// Tally per-question signal stats from a session log (jsonl).
///
/// `branch` filters to a single session branch (matching the `branch_id` field,
/// defaulting to `"main"` when absent); pass `None` to fold every branch
/// together. Output is ordered by question id for deterministic results.
// trace:STORY-68 | ai:claude
pub fn analyze_session_log(
    reader: impl Read,
    branch: Option<&str>,
) -> Result<Vec<QuestionSignalStats>> {
    let reader = BufReader::new(reader);
    let mut presented: BTreeMap<String, u32> = BTreeMap::new();
    let mut answered: BTreeMap<String, u32> = BTreeMap::new();
    let mut punted: BTreeMap<String, u32> = BTreeMap::new();
    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

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
        if let Some(branch) = branch {
            if event_branch != branch {
                continue;
            }
        }
        match value.get("event_type").and_then(Value::as_str) {
            Some("question_presented") => {
                if let Some(question_ref) = value.get("question_ref").and_then(Value::as_str) {
                    *presented.entry(question_ref.to_string()).or_default() += 1;
                }
            }
            Some("answer_recorded") => {
                let Some(question_ref) = value.get("question_ref").and_then(Value::as_str) else {
                    continue;
                };
                *answered.entry(question_ref.to_string()).or_default() += 1;
                let normalized = value
                    .get("normalized_answer")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if normalized == "punt" {
                    *punted.entry(question_ref.to_string()).or_default() += 1;
                }
            }
            Some("next_question_selected") => {
                if let (Some(from), Some(to)) = (
                    value.get("question_ref").and_then(Value::as_str),
                    value
                        .get("selected_next_question_ref")
                        .and_then(Value::as_str),
                ) {
                    edges
                        .entry(from.to_string())
                        .or_default()
                        .insert(to.to_string());
                }
            }
            _ => {}
        }
    }

    // Signals are only meaningful for questions that actually surfaced.
    let mut refs: BTreeSet<String> = BTreeSet::new();
    refs.extend(presented.keys().cloned());
    refs.extend(answered.keys().cloned());

    Ok(refs
        .into_iter()
        .map(|question_ref| {
            let branch_depth = chain_depth(&question_ref, &edges, &mut BTreeSet::new());
            QuestionSignalStats {
                presented: presented.get(&question_ref).copied().unwrap_or(0),
                answered: answered.get(&question_ref).copied().unwrap_or(0),
                punted: punted.get(&question_ref).copied().unwrap_or(0),
                branch_depth,
                question_ref,
            }
        })
        .collect())
}

/// Longest chain of follow-up edges reachable from `node`. `visiting` guards
/// against cycles so a self- or mutually-referential log can't recurse forever.
fn chain_depth(
    node: &str,
    edges: &BTreeMap<String, BTreeSet<String>>,
    visiting: &mut BTreeSet<String>,
) -> u32 {
    if !visiting.insert(node.to_string()) {
        return 0;
    }
    let mut best = 0;
    if let Some(children) = edges.get(node) {
        for child in children {
            best = best.max(1 + chain_depth(child, edges, visiting));
        }
    }
    visiting.remove(node);
    best
}

/// Map each question in the log to its derived [`QualitySignal`].
pub fn signals_from_log(
    reader: impl Read,
    branch: Option<&str>,
) -> Result<BTreeMap<String, QualitySignal>> {
    Ok(analyze_session_log(reader, branch)?
        .into_iter()
        .map(|stats| (stats.question_ref.clone(), stats.signal()))
        .collect())
}

/// The result of re-weighting one question from a derived signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReweightOutcome {
    pub question_ref: String,
    pub signal: QualitySignal,
    /// The question after re-weighting (updated `weight` + `quality:*` tag).
    pub question: Question,
}

/// Derive signals from a session log and feed them to the re-weighting engine.
///
/// For every question whose signal is not [`QualitySignal::Neutral`], loads the
/// current question from `bank` and re-weights it through `reweighter`. Neutral
/// signals are skipped: they leave the weight unchanged, so writing them back
/// would be pure churn against AIDA. Returns one [`ReweightOutcome`] per applied
/// re-weight, in question-id order.
///
/// A `bank` or `reweighter` whose batch result does not correspond one-for-one
/// with the ids requested is an error (`QuizdomError::Parse`), never a silently
/// shortened or padded result: the signal and the question it belongs to are
/// matched by id, so a mismatched batch can never persist a re-weight against
/// the wrong question (TASK-246). All three violations are caught
/// (STORY-293) — a **missing** id ([`take_by_id`]), a **repeated** id
/// ([`index_by_id`]), and an **unrequested extra** ([`reject_extras`]).
// trace:STORY-68 | ai:claude
// trace:STORY-244 | ai:claude — the whole applied set is loaded in one read
// and persisted in one write; this loop used to be `quizdom curate`'s N+1.
pub fn apply_log_signals(
    reader: impl Read,
    branch: Option<&str>,
    bank: &dyn QuestionBank,
    reweighter: &dyn QuestionReweighter,
) -> Result<Vec<ReweightOutcome>> {
    let applied: Vec<(String, QualitySignal)> = analyze_session_log(reader, branch)?
        .into_iter()
        .map(|stat| (stat.question_ref.clone(), stat.signal()))
        .filter(|(_, signal)| *signal != QualitySignal::Neutral)
        .collect();

    let ids: Vec<String> = applied.iter().map(|(id, _)| id.clone()).collect();
    // trace:TASK-246 | ai:claude — pair by question id, never by position. Both
    // batch seams below are contractually "one entry per input, in input order",
    // but that is a convention (`load_questions` is a default trait method any
    // bank may override, and its lenient neighbour `load_terms` is the
    // counterexample). A positional `zip` would truncate silently and pair every
    // question past the drop point with the wrong signal; looking each id up by
    // name turns that into an error at the seam instead.
    let mut loaded = index_by_id(bank.load_questions(&ids)?, "load_questions")?;
    let batch: Vec<(Question, QualitySignal)> = applied
        .iter()
        .map(|(question_ref, signal)| {
            take_by_id(&mut loaded, question_ref, "load_questions")
                .map(|question| (question, *signal))
        })
        .collect::<Result<_>>()?;
    reject_extras(loaded, "load_questions")?;

    let mut reweighted = index_by_id(reweighter.reweight_questions(&batch)?, "reweight_questions")?;
    let outcomes: Vec<ReweightOutcome> = applied
        .into_iter()
        .map(|(question_ref, signal)| {
            let question = take_by_id(&mut reweighted, &question_ref, "reweight_questions")?;
            Ok(ReweightOutcome {
                question_ref,
                signal,
                question,
            })
        })
        .collect::<Result<_>>()?;
    reject_extras(reweighted, "reweight_questions")?;

    Ok(outcomes)
}

// trace:TASK-246 | ai:claude
// trace:STORY-293 | ai:claude — a repeated id used to be swallowed here: two
// entries with the same id collapsed into one map slot, so the batch looked
// one entry short and the *other* id took the blame in `take_by_id`. Catching
// it at the seam names the actual violation.
/// Key a batch result by question id so callers can pair it up by name. A
/// result that names the same question twice is an error: the second entry
/// would silently shadow the first, and only one of them can be the
/// re-weight the caller asked for.
fn index_by_id(questions: Vec<Question>, source: &str) -> Result<BTreeMap<String, Question>> {
    let mut indexed: BTreeMap<String, Question> = BTreeMap::new();
    for question in questions {
        if let Some(shadowed) = indexed.insert(question.id.clone(), question) {
            return Err(QuizdomError::Parse(format!(
                "{source} returned question {} more than once: a batch load must return one entry per requested id",
                shadowed.id
            )));
        }
    }
    Ok(indexed)
}

// trace:TASK-246 | ai:claude
// trace:STORY-293 | ai:claude — TASK-254: the old rationale for removing
// rather than borrowing was "a batch that returned one id twice can't satisfy
// two requests with the same entry", a scenario the caller makes unreachable
// (the ids come from `analyze_session_log`, one stat per question, so no id is
// ever requested twice) and which `index_by_id` now rejects outright anyway.
/// Claim the question `id` from a batch result, or fail loudly naming the
/// seam that dropped it.
///
/// Removing rather than borrowing is what *drains* the map: once every
/// requested id has been claimed, anything still in it is an entry the batch
/// returned without being asked for, which is exactly what [`reject_extras`]
/// then reports.
fn take_by_id(
    questions: &mut BTreeMap<String, Question>,
    id: &str,
    source: &str,
) -> Result<Question> {
    questions.remove(id).ok_or_else(|| {
        QuizdomError::Parse(format!(
            "{source} returned no question for {id}: a batch load must return one entry per requested id"
        ))
    })
}

// trace:STORY-293 | ai:claude — TASK-253: the batch contract used to be
// enforced in one direction only. A missing id errored, but a batch that
// returned MORE than it was asked for had its surplus silently dropped by the
// pairing loop — the seam quietly disagreeing with the caller about which
// questions are in play, which is the same class of bug as a short batch.
/// Fail if the drained batch index still holds entries: whatever is left was
/// returned without being requested.
fn reject_extras(leftover: BTreeMap<String, Question>, source: &str) -> Result<()> {
    if leftover.is_empty() {
        return Ok(());
    }
    let extras: Vec<&str> = leftover.keys().map(String::as_str).collect();
    Err(QuizdomError::Parse(format!(
        "{source} returned {} question(s) that were not requested ({}): a batch load must return one entry per requested id",
        extras.len(),
        extras.join(", ")
    )))
}

// --- `quizdom curate` command wiring (STORY-72) -----------------------------

/// Parsed flags for the `quizdom curate` command.
///
/// Mirrors the `quizdom contradictions` command's log-resolution flags so the
/// two share a mental model: `--log` reads one explicit file, `--session`
/// reads one recorded session, otherwise every session for `--user` is folded
/// together. `--branch` filters to a single session branch (default: all).
#[derive(Debug)]
struct CurateConfig {
    user_id: String,
    session_id: Option<String>,
    log_path: Option<PathBuf>,
    branch: Option<String>,
}

impl CurateConfig {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut user_id = DEFAULT_USER.to_string();
        let mut session_id = None;
        let mut log_path = None;
        let mut branch = None;
        let mut args = args.into_iter().peekable();

        if matches!(args.peek().map(String::as_str), Some("curate")) {
            args.next();
        }

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--user" => user_id = next_arg(&mut args, "--user")?,
                "--session" => session_id = Some(next_arg(&mut args, "--session")?),
                "--log" => log_path = Some(PathBuf::from(next_arg(&mut args, "--log")?)),
                "--branch" => branch = Some(next_arg(&mut args, "--branch")?),
                "--help" | "-h" => return Err(QuizdomError::Usage(curate_usage())),
                other => {
                    return Err(QuizdomError::Usage(format!(
                        "unknown argument: {other}\n{}",
                        curate_usage()
                    )))
                }
            }
        }

        Ok(Self {
            user_id,
            session_id,
            log_path,
            branch,
        })
    }

    /// The log files to read: an explicit `--log`, a single `--session`, or
    /// every session recorded for `--user`. Mirrors the contradictions
    /// command's resolution so both read the same on-disk layout.
    fn log_paths(&self) -> Result<Vec<PathBuf>> {
        if let Some(log_path) = &self.log_path {
            return Ok(vec![log_path.clone()]);
        }
        let sessions_dir = PathBuf::from("data")
            .join("users")
            .join(&self.user_id)
            .join("sessions");
        if let Some(session_id) = &self.session_id {
            return Ok(vec![sessions_dir.join(format!("{session_id}.jsonl"))]);
        }
        if !sessions_dir.exists() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(&sessions_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                paths.push(path);
            }
        }
        paths.sort();
        Ok(paths)
    }
}

fn curate_usage() -> String {
    "usage: quizdom curate [--user local-user] [--session sess-id] [--log path] [--branch main]"
        .to_string()
}

fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| QuizdomError::Usage(format!("{name} requires a value")))
}

/// Concatenate every (existing) log file into one buffer so signals fold
/// across a user's whole history in a single analysis pass. A newline is
/// inserted between files so the last record of one log can't merge with the
/// first record of the next.
fn read_logs(paths: &[PathBuf]) -> Result<Vec<u8>> {
    let mut buffer = Vec::new();
    for path in paths {
        if !path.exists() {
            continue;
        }
        let mut file = std::fs::File::open(path)?;
        file.read_to_end(&mut buffer)?;
        if buffer.last().is_some_and(|byte| *byte != b'\n') {
            buffer.push(b'\n');
        }
    }
    Ok(buffer)
}

/// Print a human-readable summary of what curation changed.
fn render_curation(outcomes: &[ReweightOutcome], output: &mut impl Write) -> Result<()> {
    if outcomes.is_empty() {
        writeln!(
            output,
            "Nothing to curate: no questions earned a re-weight."
        )?;
        return Ok(());
    }
    writeln!(output, "Re-weighted {} question(s):", outcomes.len())?;
    for outcome in outcomes {
        let signal = outcome
            .signal
            .quality_tag()
            .strip_prefix("quality:")
            .unwrap_or("changed");
        writeln!(
            output,
            "  {} [{}] -> weight {}",
            outcome.question_ref, signal, outcome.question.weight
        )?;
    }
    Ok(())
}

/// Run curation with caller-supplied bank + reweighter (the seam the command
/// entry point and tests share).
fn curate(
    config: &CurateConfig,
    bank: &dyn QuestionBank,
    reweighter: &dyn QuestionReweighter,
    output: &mut impl Write,
) -> Result<()> {
    let log = read_logs(&config.log_paths()?)?;
    let outcomes = apply_log_signals(log.as_slice(), config.branch.as_deref(), bank, reweighter)?;
    render_curation(&outcomes, output)
}

/// Entry point for the standalone `quizdom curate` command. Reads the user's
/// session log(s), derives per-question quality signals (STORY-68), and applies
/// the STORY-66 re-weighting — persisting each change to the domain graph — then prints a
/// summary of what moved. This is the wiring STORY-72 adds: the bank-evolution
/// loop was built but, until now, nothing invoked it.
// trace:STORY-72 | ai:claude
pub fn run_curate(args: impl IntoIterator<Item = String>, output: &mut impl Write) -> Result<()> {
    let config = CurateConfig::parse(args)?;
    let bank = StoreQuestionBank::default();
    let reweighter = StoreQuestionReweighter::default();
    curate(&config, &bank, &reweighter, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bank::rewrite_quality_tags;
    use crate::model::{AnswerKind, Question, QuestionRef, TermDefinition, TermRef};
    use crate::strategy::reweight;
    use std::cell::RefCell;

    // A log exercising every classification path:
    //   Q-1 — punted on both of its two answers   -> Unhelpful
    //   Q-2 — answered, seeds chain Q-2->Q-3->Q-4  -> Insightful (depth 2)
    //   Q-3 — answered, seeds Q-3->Q-4 (depth 1)   -> Neutral
    //   Q-4 — answered, no follow-up (depth 0)     -> Neutral
    // plus a stray event on another branch that the `main` filter must ignore.
    const SAMPLE_LOG: &str = r#"
{"event_type":"question_presented","branch_id":"main","turn":1,"question_ref":"Q-1"}
{"event_type":"answer_recorded","branch_id":"main","turn":1,"question_ref":"Q-1","normalized_answer":"punt"}
{"event_type":"question_presented","branch_id":"main","turn":2,"question_ref":"Q-1"}
{"event_type":"answer_recorded","branch_id":"main","turn":2,"question_ref":"Q-1","normalized_answer":"punt"}
{"event_type":"question_presented","branch_id":"main","turn":3,"question_ref":"Q-2"}
{"event_type":"answer_recorded","branch_id":"main","turn":3,"question_ref":"Q-2","normalized_answer":"yes"}
{"event_type":"next_question_selected","branch_id":"main","turn":3,"question_ref":"Q-2","selected_next_question_ref":"Q-3"}
{"event_type":"question_presented","branch_id":"main","turn":4,"question_ref":"Q-3"}
{"event_type":"answer_recorded","branch_id":"main","turn":4,"question_ref":"Q-3","normalized_answer":"no"}
{"event_type":"next_question_selected","branch_id":"main","turn":4,"question_ref":"Q-3","selected_next_question_ref":"Q-4"}
{"event_type":"question_presented","branch_id":"main","turn":5,"question_ref":"Q-4"}
{"event_type":"answer_recorded","branch_id":"main","turn":5,"question_ref":"Q-4","normalized_answer":"yes"}
{"event_type":"answer_recorded","branch_id":"side","turn":9,"question_ref":"Q-9","normalized_answer":"punt"}
"#;

    fn stats_for<'a>(stats: &'a [QuestionSignalStats], id: &str) -> &'a QuestionSignalStats {
        stats
            .iter()
            .find(|stat| stat.question_ref == id)
            .unwrap_or_else(|| panic!("no stats for {id}"))
    }

    #[test]
    fn tallies_presented_answered_and_punted_per_question() {
        let stats = analyze_session_log(SAMPLE_LOG.as_bytes(), Some("main"))
            .expect("analysis should succeed");

        let q1 = stats_for(&stats, "Q-1");
        assert_eq!(q1.presented, 2);
        assert_eq!(q1.answered, 2);
        assert_eq!(q1.punted, 2);
        assert_eq!(q1.punt_rate(), 1.0);

        let q2 = stats_for(&stats, "Q-2");
        assert_eq!(q2.presented, 1);
        assert_eq!(q2.answered, 1);
        assert_eq!(q2.punted, 0);
        assert_eq!(q2.punt_rate(), 0.0);
    }

    #[test]
    fn measures_branch_depth_as_longest_follow_up_chain() {
        let stats = analyze_session_log(SAMPLE_LOG.as_bytes(), Some("main"))
            .expect("analysis should succeed");
        assert_eq!(stats_for(&stats, "Q-2").branch_depth, 2);
        assert_eq!(stats_for(&stats, "Q-3").branch_depth, 1);
        assert_eq!(stats_for(&stats, "Q-4").branch_depth, 0);
    }

    #[test]
    fn branch_filter_excludes_other_branches() {
        let stats = analyze_session_log(SAMPLE_LOG.as_bytes(), Some("main"))
            .expect("analysis should succeed");
        assert!(stats.iter().all(|stat| stat.question_ref != "Q-9"));

        let all =
            analyze_session_log(SAMPLE_LOG.as_bytes(), None).expect("analysis should succeed");
        assert!(all.iter().any(|stat| stat.question_ref == "Q-9"));
    }

    #[test]
    fn classifies_high_punt_rate_as_unhelpful() {
        let signals =
            signals_from_log(SAMPLE_LOG.as_bytes(), Some("main")).expect("signals should derive");
        assert_eq!(signals.get("Q-1"), Some(&QualitySignal::Unhelpful));
    }

    #[test]
    fn classifies_deep_branch_as_insightful() {
        let signals =
            signals_from_log(SAMPLE_LOG.as_bytes(), Some("main")).expect("signals should derive");
        assert_eq!(signals.get("Q-2"), Some(&QualitySignal::Insightful));
    }

    #[test]
    fn classifies_shallow_or_quiet_questions_as_neutral() {
        let signals =
            signals_from_log(SAMPLE_LOG.as_bytes(), Some("main")).expect("signals should derive");
        assert_eq!(signals.get("Q-3"), Some(&QualitySignal::Neutral));
        assert_eq!(signals.get("Q-4"), Some(&QualitySignal::Neutral));
    }

    #[test]
    fn punt_below_threshold_stays_neutral() {
        // One punt out of three answers -> 0.33 < 0.5, no deep branch.
        let log = r#"
{"event_type":"answer_recorded","branch_id":"main","question_ref":"Q-7","normalized_answer":"yes"}
{"event_type":"answer_recorded","branch_id":"main","question_ref":"Q-7","normalized_answer":"punt"}
{"event_type":"answer_recorded","branch_id":"main","question_ref":"Q-7","normalized_answer":"no"}
"#;
        let stats = analyze_session_log(log.as_bytes(), Some("main")).expect("analysis succeeds");
        let q7 = stats_for(&stats, "Q-7");
        assert_eq!(q7.punted, 1);
        assert_eq!(q7.answered, 3);
        assert_eq!(q7.signal(), QualitySignal::Neutral);
    }

    // --- apply_log_signals: feed the re-weighting engine ------------------

    struct FakeBank {
        questions: BTreeMap<String, Question>,
    }

    impl QuestionBank for FakeBank {
        fn load_question(&self, id: &str) -> Result<Question> {
            self.questions
                .get(id)
                .cloned()
                .ok_or_else(|| QuizdomError::Parse(format!("missing {id}")))
        }
        fn begets(&self, _id: &str) -> Result<Vec<QuestionRef>> {
            Ok(Vec::new())
        }
        fn probes(&self, _id: &str) -> Result<Vec<TermRef>> {
            Ok(Vec::new())
        }
        fn load_term(&self, id: &str) -> Result<TermDefinition> {
            Err(QuizdomError::Parse(format!("missing term {id}")))
        }
    }

    #[derive(Default)]
    struct RecordingReweighter {
        applied: RefCell<Vec<(String, QualitySignal)>>,
    }

    impl QuestionReweighter for RecordingReweighter {
        fn reweight_question(
            &self,
            question: &Question,
            signal: QualitySignal,
        ) -> Result<Question> {
            self.applied
                .borrow_mut()
                .push((question.id.clone(), signal));
            let new_weight = reweight(question.weight, signal);
            let mut updated = question.clone();
            updated.tags = rewrite_quality_tags(&question.tags, signal);
            updated.weight = new_weight;
            Ok(updated)
        }
    }

    fn question(id: &str, weight: u32) -> Question {
        Question {
            id: id.to_string(),
            title: format!("question {id}"),
            tags: vec!["answer:yes-no".to_string()],
            answer_kind: AnswerKind::YesNo,
            weight,
        }
    }

    #[test]
    fn apply_reweights_non_neutral_questions_and_skips_neutral() {
        let mut questions = BTreeMap::new();
        questions.insert("Q-1".to_string(), question("Q-1", 50));
        questions.insert("Q-2".to_string(), question("Q-2", 50));
        let bank = FakeBank { questions };
        let reweighter = RecordingReweighter::default();

        let outcomes = apply_log_signals(SAMPLE_LOG.as_bytes(), Some("main"), &bank, &reweighter)
            .expect("apply should succeed");

        // Only Q-1 (Unhelpful) and Q-2 (Insightful) are touched; Q-3/Q-4 are
        // Neutral and skipped.
        let applied = reweighter.applied.borrow();
        assert_eq!(
            *applied,
            vec![
                ("Q-1".to_string(), QualitySignal::Unhelpful),
                ("Q-2".to_string(), QualitySignal::Insightful),
            ]
        );

        assert_eq!(outcomes.len(), 2);
        let q1 = &outcomes[0];
        assert_eq!(q1.signal, QualitySignal::Unhelpful);
        assert_eq!(q1.question.weight, 38); // 50 - 12
        assert!(q1.question.tags.contains(&"quality:unhelpful".to_string()));
        let q2 = &outcomes[1];
        assert_eq!(q2.signal, QualitySignal::Insightful);
        assert_eq!(q2.question.weight, 62); // 50 + 12
        assert!(q2.question.tags.contains(&"quality:insightful".to_string()));
    }

    // trace:STORY-244 | ai:claude
    /// The acceptance measurement in miniature: curation over the real Dolt
    /// backend costs a fixed handful of `dolt` spawns, not the four per
    /// re-weighted question that made `quizdom curate` a 264-spawn, 2m09s run.
    #[test]
    fn curation_over_the_dolt_backend_spawns_a_fixed_handful() {
        use crate::bank::StoreQuestionBank;
        use crate::dolt_store::{DoltDomainStore, ScriptedDoltRunner};
        use crate::persist::StoreQuestionReweighter;

        // Cloning the runner shares one call log and one response queue, so
        // the bank's read and the reweighter's write are counted together.
        let runner = ScriptedDoltRunner::new(vec![
            (
                0,
                r#"{"rows":[
                    {"id":"Q-1","title":"one","body":"","tags":"answer:yes-no","weight":50},
                    {"id":"Q-2","title":"two","body":"","tags":"answer:yes-no","weight":50}]}"#,
                "",
            ),
            (0, "", ""), // the one batched UPDATE
            (0, "", ""), // dolt add -A
            (0, "", ""), // dolt commit
        ]);
        let calls = runner.calls.clone();
        let bank = StoreQuestionBank::with_store(DoltDomainStore::with_runner(
            "/tmp/quizdom-dolt",
            runner.clone(),
        ));
        let reweighter = StoreQuestionReweighter::with_store(DoltDomainStore::with_runner(
            "/tmp/quizdom-dolt",
            runner,
        ));

        let outcomes = apply_log_signals(SAMPLE_LOG.as_bytes(), Some("main"), &bank, &reweighter)
            .expect("curation should succeed");

        assert_eq!(outcomes.len(), 2, "Q-1 unhelpful, Q-2 insightful");
        assert_eq!(outcomes[0].question.weight, 38);
        assert_eq!(outcomes[1].question.weight, 62);
        assert_eq!(
            calls.borrow().len(),
            4,
            "one read + one write + add + commit, whatever the question count"
        );
    }

    // --- TASK-246: batch results are paired by id, never by position ------

    /// How a bank's batch load misbehaves relative to the strict convention.
    enum Quirk {
        /// Returns fewer questions than ids asked for — the `zip`-truncation
        /// case that used to misattribute every question past the drop point.
        DropFirst,
        /// Returns every question, in the wrong order.
        Reverse,
        // trace:STORY-293 | ai:claude — the other direction of the contract.
        /// Returns every requested question *plus* one nobody asked for.
        AddUnrequested,
        // trace:STORY-293 | ai:claude
        /// Returns every question, with the first one twice.
        RepeatFirst,
    }

    struct QuirkyBank {
        inner: FakeBank,
        quirk: Quirk,
    }

    impl QuestionBank for QuirkyBank {
        fn load_question(&self, id: &str) -> Result<Question> {
            self.inner.load_question(id)
        }
        fn begets(&self, _id: &str) -> Result<Vec<QuestionRef>> {
            Ok(Vec::new())
        }
        fn load_questions(&self, ids: &[String]) -> Result<Vec<Question>> {
            let mut loaded: Vec<Question> = ids
                .iter()
                .map(|id| self.inner.load_question(id))
                .collect::<Result<_>>()?;
            match self.quirk {
                Quirk::DropFirst => {
                    loaded.remove(0);
                }
                Quirk::Reverse => loaded.reverse(),
                Quirk::AddUnrequested => loaded.push(question("Q-99", 50)),
                Quirk::RepeatFirst => loaded.push(loaded[0].clone()),
            }
            Ok(loaded)
        }
    }

    fn two_question_bank() -> FakeBank {
        let mut questions = BTreeMap::new();
        questions.insert("Q-1".to_string(), question("Q-1", 50));
        questions.insert("Q-2".to_string(), question("Q-2", 50));
        FakeBank { questions }
    }

    // trace:TASK-246 | ai:claude
    #[test]
    fn apply_errors_when_the_bank_batch_drops_a_question() {
        let bank = QuirkyBank {
            inner: two_question_bank(),
            quirk: Quirk::DropFirst,
        };
        let reweighter = RecordingReweighter::default();

        let error = apply_log_signals(SAMPLE_LOG.as_bytes(), Some("main"), &bank, &reweighter)
            .expect_err("a short batch load must fail, not misattribute");

        let message = error.to_string();
        assert!(message.contains("load_questions"), "{message}");
        assert!(message.contains("Q-1"), "{message}");
        // Nothing was persisted: the seam failed before any re-weight ran.
        assert!(reweighter.applied.borrow().is_empty());
    }

    // trace:TASK-246 | ai:claude
    #[test]
    fn apply_pairs_by_id_when_the_bank_batch_reorders() {
        let bank = QuirkyBank {
            inner: two_question_bank(),
            quirk: Quirk::Reverse,
        };
        let reweighter = RecordingReweighter::default();

        let outcomes = apply_log_signals(SAMPLE_LOG.as_bytes(), Some("main"), &bank, &reweighter)
            .expect("a reordered batch load is still fully answerable");

        // Q-1 is Unhelpful and Q-2 Insightful whatever order the bank hands
        // them back in — positional pairing would have swapped the two.
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].question_ref, "Q-1");
        assert_eq!(outcomes[0].signal, QualitySignal::Unhelpful);
        assert_eq!(outcomes[0].question.id, "Q-1");
        assert_eq!(outcomes[0].question.weight, 38); // 50 - 12
        assert_eq!(outcomes[1].question_ref, "Q-2");
        assert_eq!(outcomes[1].signal, QualitySignal::Insightful);
        assert_eq!(outcomes[1].question.id, "Q-2");
        assert_eq!(outcomes[1].question.weight, 62); // 50 + 12
    }

    // --- STORY-293: the contract holds in BOTH directions ------------------

    // trace:STORY-293 | ai:claude — TASK-253: the mirror of
    // `apply_errors_when_the_bank_batch_drops_a_question`. A batch that
    // returns MORE than was asked for used to have its surplus silently
    // dropped by the pairing loop.
    #[test]
    fn apply_errors_when_the_bank_batch_returns_an_unrequested_question() {
        let bank = QuirkyBank {
            inner: two_question_bank(),
            quirk: Quirk::AddUnrequested,
        };
        let reweighter = RecordingReweighter::default();

        let error = apply_log_signals(SAMPLE_LOG.as_bytes(), Some("main"), &bank, &reweighter)
            .expect_err("an over-returning batch load must fail, not silently drop the surplus");

        let message = error.to_string();
        assert!(message.contains("load_questions"), "{message}");
        assert!(message.contains("Q-99"), "{message}");
        assert!(message.contains("not requested"), "{message}");
        // The surplus is caught before anything is written back.
        assert!(reweighter.applied.borrow().is_empty());
    }

    // trace:STORY-293 | ai:claude — a repeated id is neither a short batch nor
    // a surplus one: it is caught where the index is built, so the error names
    // the duplicate rather than blaming whichever id it shadowed.
    #[test]
    fn apply_errors_when_the_bank_batch_repeats_a_question() {
        let bank = QuirkyBank {
            inner: two_question_bank(),
            quirk: Quirk::RepeatFirst,
        };
        let reweighter = RecordingReweighter::default();

        let error = apply_log_signals(SAMPLE_LOG.as_bytes(), Some("main"), &bank, &reweighter)
            .expect_err("a batch load that names one question twice must fail");

        let message = error.to_string();
        assert!(message.contains("load_questions"), "{message}");
        assert!(message.contains("Q-1"), "{message}");
        assert!(message.contains("more than once"), "{message}");
        assert!(reweighter.applied.borrow().is_empty());
    }

    /// The write-side mirror of [`Quirk::AddUnrequested`]: a reweighter that
    /// hands back a question the caller never put in the batch.
    struct OverReturningReweighter;

    impl QuestionReweighter for OverReturningReweighter {
        fn reweight_question(
            &self,
            question: &Question,
            signal: QualitySignal,
        ) -> Result<Question> {
            let mut updated = question.clone();
            updated.tags = rewrite_quality_tags(&question.tags, signal);
            updated.weight = reweight(question.weight, signal);
            Ok(updated)
        }
        fn reweight_questions(&self, batch: &[(Question, QualitySignal)]) -> Result<Vec<Question>> {
            let mut updated: Vec<Question> = batch
                .iter()
                .map(|(question, signal)| self.reweight_question(question, *signal))
                .collect::<Result<_>>()?;
            updated.push(question("Q-99", 50));
            Ok(updated)
        }
    }

    // trace:STORY-293 | ai:claude — TASK-253, the write seam: symmetric with
    // `apply_errors_when_the_reweighter_batch_truncates`. An extra outcome the
    // caller has no signal for must not reach the caller silently.
    #[test]
    fn apply_errors_when_the_reweighter_batch_over_returns() {
        let bank = two_question_bank();

        let error = apply_log_signals(
            SAMPLE_LOG.as_bytes(),
            Some("main"),
            &bank,
            &OverReturningReweighter,
        )
        .expect_err("an over-returning batch write must fail, not drop the surplus");

        let message = error.to_string();
        assert!(message.contains("reweight_questions"), "{message}");
        assert!(message.contains("Q-99"), "{message}");
        assert!(message.contains("not requested"), "{message}");
    }

    // trace:STORY-293 | ai:claude — TASK-254: `take_by_id` removes rather than
    // borrows so the index is *drained*, which is what makes the leftover
    // check above possible. This pins that behaviour directly: a claimed id is
    // gone from the map, and claiming it twice fails.
    #[test]
    fn take_by_id_drains_the_entry_it_claims() {
        let mut indexed = index_by_id(
            vec![question("Q-1", 50), question("Q-2", 50)],
            "load_questions",
        )
        .expect("two distinct ids index cleanly");

        let claimed = take_by_id(&mut indexed, "Q-1", "load_questions").expect("Q-1 is present");
        assert_eq!(claimed.id, "Q-1");
        assert_eq!(
            indexed.keys().collect::<Vec<_>>(),
            ["Q-2"],
            "the claimed entry is removed, leaving only what was never asked for"
        );

        let error = take_by_id(&mut indexed, "Q-1", "load_questions")
            .expect_err("a drained entry cannot be claimed a second time");
        assert!(error.to_string().contains("no question for Q-1"));

        // What remains after every claim is the surplus `reject_extras` reports.
        let error = reject_extras(indexed, "load_questions")
            .expect_err("Q-2 was never requested in this pairing");
        assert!(error.to_string().contains("Q-2"), "{error}");
    }

    /// A reweighter whose batch write returns one fewer question than it was
    /// given — the second positional `zip` TASK-246 removed.
    struct TruncatingReweighter;

    impl QuestionReweighter for TruncatingReweighter {
        fn reweight_question(
            &self,
            question: &Question,
            signal: QualitySignal,
        ) -> Result<Question> {
            let mut updated = question.clone();
            updated.tags = rewrite_quality_tags(&question.tags, signal);
            updated.weight = reweight(question.weight, signal);
            Ok(updated)
        }
        fn reweight_questions(&self, batch: &[(Question, QualitySignal)]) -> Result<Vec<Question>> {
            let mut updated: Vec<Question> = batch
                .iter()
                .map(|(question, signal)| self.reweight_question(question, *signal))
                .collect::<Result<_>>()?;
            updated.pop();
            Ok(updated)
        }
    }

    // trace:TASK-246 | ai:claude
    #[test]
    fn apply_errors_when_the_reweighter_batch_truncates() {
        let bank = two_question_bank();

        let error = apply_log_signals(
            SAMPLE_LOG.as_bytes(),
            Some("main"),
            &bank,
            &TruncatingReweighter,
        )
        .expect_err("a short batch write must fail, not drop an outcome");

        let message = error.to_string();
        assert!(message.contains("reweight_questions"), "{message}");
        assert!(message.contains("Q-2"), "{message}");
    }

    #[test]
    fn apply_on_empty_log_does_nothing() {
        let bank = FakeBank {
            questions: BTreeMap::new(),
        };
        let reweighter = RecordingReweighter::default();
        let outcomes = apply_log_signals("".as_bytes(), Some("main"), &bank, &reweighter)
            .expect("apply should succeed");
        assert!(outcomes.is_empty());
        assert!(reweighter.applied.borrow().is_empty());
    }

    // --- `quizdom curate` command wiring (STORY-72) ----------------------

    use std::sync::atomic::{AtomicU32, Ordering};

    fn strings<const N: usize>(args: [&str; N]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    /// Write `contents` to a unique temp file and return its path. Uniqueness
    /// comes from pid + a process-wide counter so parallel tests don't collide.
    fn temp_log(contents: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "quizdom-curate-{}-{}.jsonl",
            std::process::id(),
            nonce
        ));
        std::fs::write(&path, contents).expect("write temp log");
        path
    }

    #[test]
    fn curate_parses_all_flags() {
        let config = CurateConfig::parse(strings([
            "curate",
            "--user",
            "ada",
            "--session",
            "s-1",
            "--log",
            "/tmp/x.jsonl",
            "--branch",
            "main",
        ]))
        .expect("parse should succeed");
        assert_eq!(config.user_id, "ada");
        assert_eq!(config.session_id.as_deref(), Some("s-1"));
        assert_eq!(config.log_path, Some(PathBuf::from("/tmp/x.jsonl")));
        assert_eq!(config.branch.as_deref(), Some("main"));
    }

    #[test]
    fn curate_defaults_to_local_user_and_all_branches() {
        let config = CurateConfig::parse(strings(["curate"])).expect("parse should succeed");
        assert_eq!(config.user_id, DEFAULT_USER);
        assert!(config.session_id.is_none());
        assert!(config.log_path.is_none());
        assert!(config.branch.is_none());
    }

    #[test]
    fn curate_rejects_unknown_flag() {
        let error = CurateConfig::parse(strings(["curate", "--nope"])).unwrap_err();
        assert!(matches!(error, QuizdomError::Usage(_)));
    }

    #[test]
    fn curate_explicit_log_path_wins() {
        let config = CurateConfig::parse(strings(["curate", "--log", "/tmp/only.jsonl"]))
            .expect("parse should succeed");
        assert_eq!(
            config.log_paths().expect("paths"),
            vec![PathBuf::from("/tmp/only.jsonl")]
        );
    }

    #[test]
    fn curate_reweights_logged_questions_and_summarizes() {
        let log = temp_log(SAMPLE_LOG);
        let config = CurateConfig {
            user_id: DEFAULT_USER.to_string(),
            session_id: None,
            log_path: Some(log.clone()),
            branch: Some("main".to_string()),
        };
        let mut questions = BTreeMap::new();
        questions.insert("Q-1".to_string(), question("Q-1", 50));
        questions.insert("Q-2".to_string(), question("Q-2", 50));
        let bank = FakeBank { questions };
        let reweighter = RecordingReweighter::default();

        let mut output = Vec::new();
        curate(&config, &bank, &reweighter, &mut output).expect("curate should succeed");
        std::fs::remove_file(&log).ok();

        // The command drove the re-weighting engine over exactly the
        // non-neutral questions in the log.
        assert_eq!(
            *reweighter.applied.borrow(),
            vec![
                ("Q-1".to_string(), QualitySignal::Unhelpful),
                ("Q-2".to_string(), QualitySignal::Insightful),
            ]
        );

        let rendered = String::from_utf8(output).expect("utf8");
        assert!(rendered.contains("Re-weighted 2 question(s):"));
        assert!(rendered.contains("Q-1 [unhelpful] -> weight 38"));
        assert!(rendered.contains("Q-2 [insightful] -> weight 62"));
    }

    #[test]
    fn curate_reports_when_nothing_changed() {
        let log = temp_log(""); // empty log -> no signals -> no re-weights
        let config = CurateConfig {
            user_id: DEFAULT_USER.to_string(),
            session_id: None,
            log_path: Some(log.clone()),
            branch: None,
        };
        let bank = FakeBank {
            questions: BTreeMap::new(),
        };
        let reweighter = RecordingReweighter::default();

        let mut output = Vec::new();
        curate(&config, &bank, &reweighter, &mut output).expect("curate should succeed");
        std::fs::remove_file(&log).ok();

        assert!(reweighter.applied.borrow().is_empty());
        let rendered = String::from_utf8(output).expect("utf8");
        assert!(rendered.contains("Nothing to curate"));
    }

    #[test]
    fn run_curate_on_unknown_user_reports_nothing() {
        // End-to-end through the real default bank + reweighter: a user with no
        // session logs yields no outcomes, so neither the bank nor the
        // reweighter ever shells out to dolt.
        let mut output = Vec::new();
        run_curate(
            strings(["curate", "--user", "no-such-user-xyz"]),
            &mut output,
        )
        .expect("run_curate should succeed");
        let rendered = String::from_utf8(output).expect("utf8");
        assert!(rendered.contains("Nothing to curate"));
    }
}
