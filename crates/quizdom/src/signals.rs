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

use crate::bank::QuestionBank;
use crate::error::{QuizdomError, Result};
use crate::model::Question;
use crate::persist::QuestionReweighter;
use crate::strategy::QualitySignal;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read};

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
// trace:STORY-68 | ai:claude
pub fn apply_log_signals(
    reader: impl Read,
    branch: Option<&str>,
    bank: &dyn QuestionBank,
    reweighter: &dyn QuestionReweighter,
) -> Result<Vec<ReweightOutcome>> {
    let stats = analyze_session_log(reader, branch)?;
    let mut outcomes = Vec::new();
    for stat in stats {
        let signal = stat.signal();
        if signal == QualitySignal::Neutral {
            continue;
        }
        let question = bank.load_question(&stat.question_ref)?;
        let question = reweighter.reweight_question(&question, signal)?;
        outcomes.push(ReweightOutcome {
            question_ref: stat.question_ref,
            signal,
            question,
        });
    }
    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bank::rewrite_weight_and_quality_tags;
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
            updated.tags = rewrite_weight_and_quality_tags(&question.tags, new_weight, signal);
            updated.weight = new_weight;
            Ok(updated)
        }
    }

    fn question(id: &str, weight: u32) -> Question {
        Question {
            id: id.to_string(),
            title: format!("question {id}"),
            tags: vec![format!("weight:{weight}"), "answer:yes-no".to_string()],
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
}
