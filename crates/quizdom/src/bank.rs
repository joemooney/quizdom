use crate::error::{QuizdomError, Result};
use crate::model::{answer_kind_from_tags, Question, QuestionRef, TermDefinition, TermRef};
use crate::strategy::QualitySignal;
use std::collections::BTreeSet;
use std::process::Command;

pub trait QuestionBank {
    fn load_question(&self, id: &str) -> Result<Question>;
    fn begets(&self, id: &str) -> Result<Vec<QuestionRef>>;
    fn all_questions(&self) -> Result<Vec<Question>> {
        Ok(Vec::new())
    }
    fn probes(&self, _id: &str) -> Result<Vec<TermRef>> {
        Ok(Vec::new())
    }
    fn load_term(&self, id: &str) -> Result<TermDefinition> {
        Err(QuizdomError::Parse(format!("missing term {id}")))
    }
}

pub struct AidaCliQuestionBank {
    command: String,
}

impl Default for AidaCliQuestionBank {
    fn default() -> Self {
        Self {
            command: "aida".to_string(),
        }
    }
}

impl QuestionBank for AidaCliQuestionBank {
    fn load_question(&self, id: &str) -> Result<Question> {
        let output = Command::new(&self.command).args(["show", id]).output()?;
        if !output.status.success() {
            return Err(QuizdomError::Aida(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        parse_question_show(&String::from_utf8_lossy(&output.stdout))
    }

    fn begets(&self, id: &str) -> Result<Vec<QuestionRef>> {
        let output = Command::new(&self.command)
            .args(["rel", "list", id, "--type", "begets"])
            .output()?;
        if !output.status.success() {
            return Err(QuizdomError::Aida(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        Ok(parse_begets_rel_list(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }

    fn all_questions(&self) -> Result<Vec<Question>> {
        // trace:STORY-53 | ai:codex
        let output = Command::new(&self.command)
            .args(["list", "--type", "functional", "--no-scope"])
            .output()?;
        if !output.status.success() {
            return Err(QuizdomError::Aida(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        let mut questions = Vec::new();
        for id in parse_question_list_ids(&String::from_utf8_lossy(&output.stdout)) {
            if let Ok(question) = self.load_question(&id) {
                questions.push(question);
            }
        }
        Ok(questions)
    }

    fn probes(&self, id: &str) -> Result<Vec<TermRef>> {
        let output = Command::new(&self.command)
            .args(["rel", "list", id, "--type", "probes"])
            .output()?;
        if !output.status.success() {
            return Err(QuizdomError::Aida(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        Ok(parse_probes_rel_list(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }

    fn load_term(&self, id: &str) -> Result<TermDefinition> {
        let output = Command::new(&self.command).args(["show", id]).output()?;
        if !output.status.success() {
            return Err(QuizdomError::Aida(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }

        parse_term_show(&String::from_utf8_lossy(&output.stdout))
    }
}

pub fn parse_question_show(output: &str) -> Result<Question> {
    let id = prefixed_line(output, "ID:")
        .ok_or_else(|| QuizdomError::Parse("aida show output missing ID".to_string()))?;
    let title = prefixed_line(output, "Title:")
        .ok_or_else(|| QuizdomError::Parse("aida show output missing Title".to_string()))?;
    let tags = split_tags(&prefixed_line(output, "Tags:").unwrap_or_default());
    let answer_kind = answer_kind_from_tags(&tags)
        .ok_or_else(|| QuizdomError::Parse(format!("{id} missing answer:* tag")))?;
    let weight = tags
        .iter()
        .find_map(|tag| tag.strip_prefix("weight:")?.parse::<u32>().ok())
        .unwrap_or(0);

    Ok(Question {
        id,
        title,
        tags,
        answer_kind,
        weight,
    })
}

pub fn parse_term_show(output: &str) -> Result<TermDefinition> {
    let id = prefixed_line(output, "ID:")
        .ok_or_else(|| QuizdomError::Parse("aida show output missing ID".to_string()))?;
    let title = prefixed_line(output, "Title:")
        .ok_or_else(|| QuizdomError::Parse("aida show output missing Title".to_string()))?;
    let tags = split_tags(&prefixed_line(output, "Tags:").unwrap_or_default());
    let definition = parse_definition_text(output)
        .ok_or_else(|| QuizdomError::Parse(format!("{id} missing definition: line")))?;

    Ok(TermDefinition {
        id,
        title,
        tags,
        definition,
    })
}

fn parse_definition_text(output: &str) -> Option<String> {
    let mut definition = Vec::new();
    let mut in_definition = false;
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("definition:") {
            in_definition = true;
            let rest = rest.trim();
            if !rest.is_empty() {
                definition.push(rest.to_string());
            }
            continue;
        }
        if in_definition {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("scope:") {
                break;
            }
            definition.push(trimmed.to_string());
        }
    }
    (!definition.is_empty()).then(|| definition.join(" "))
}

fn split_tags(line: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let mut current = String::new();
    let mut bracket_depth = 0_u32;

    for character in line.chars() {
        match character {
            '[' => {
                bracket_depth += 1;
                current.push(character);
            }
            ']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                current.push(character);
            }
            ',' if bracket_depth == 0 => {
                let tag = current.trim();
                if !tag.is_empty() {
                    tags.push(tag.to_string());
                }
                current.clear();
            }
            _ => current.push(character),
        }
    }

    let tag = current.trim();
    if !tag.is_empty() {
        tags.push(tag.to_string());
    }

    tags
}

fn prefixed_line(output: &str, prefix: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix(prefix).map(str::trim))
        .map(str::to_string)
}

pub fn parse_begets_rel_list(output: &str) -> Vec<QuestionRef> {
    parse_rel_list(output, "begets")
        .into_iter()
        .map(|id| QuestionRef { id })
        .collect()
}

pub fn parse_probes_rel_list(output: &str) -> Vec<TermRef> {
    parse_rel_list(output, "probes")
        .into_iter()
        .map(|id| TermRef { id })
        .collect()
}

// trace:STORY-53 | ai:codex
pub fn parse_question_list_ids(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let id = line.split_whitespace().next()?;
            id.starts_with("Q-").then(|| id.to_string())
        })
        .collect()
}

fn parse_rel_list(output: &str, expected_type: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty()
                || trimmed.starts_with("FROM")
                || trimmed.starts_with("(no outgoing")
                || trimmed.ends_with("edges")
            {
                return None;
            }
            let mut columns = trimmed.split_whitespace();
            let _from = columns.next()?;
            let relationship_type = columns.next()?;
            let to = columns.next()?;
            (relationship_type == expected_type).then(|| to.to_string())
        })
        .collect()
}

// trace:STORY-86 | ai:claude
/// A bank question judged a near-duplicate of a candidate the user just
/// authored, paired with its similarity score in `[0.0, 1.0]`.
///
/// Returned by [`find_near_duplicate`] so the approve flow can offer the
/// existing question for reuse / linking instead of persisting a rephrasing.
#[derive(Debug, Clone, PartialEq)]
pub struct NearDuplicate {
    /// The existing bank question that closely matches the candidate.
    pub question: Question,
    /// Jaccard token-overlap similarity in `[0.0, 1.0]`; higher is closer.
    pub similarity: f64,
}

// trace:STORY-86 | ai:claude
/// Similarity at or above which two question titles are treated as
/// near-duplicates. Jaccard overlap of `0.6` means the two questions share at
/// least ~60% of their significant words — enough to flag a rephrasing while
/// letting genuinely distinct prompts through.
pub const DEDUP_SIMILARITY_THRESHOLD: f64 = 0.6;

// trace:STORY-86 | ai:claude
/// Search `bank` for the question most similar to `candidate_title`, returning
/// it only when the similarity is at or above `threshold`.
///
/// Pure and dependency-free: similarity is the Jaccard overlap of the two
/// titles' normalized word sets (case-folded, punctuation- and stop-word
/// stripped). An exact rephrasing — same words, reordered or re-punctuated —
/// scores `1.0`. Ties break toward the higher-weight question, then the lower
/// id, so the choice is deterministic. Returns `None` when the bank is empty or
/// nothing clears the bar.
pub fn find_near_duplicate(
    candidate_title: &str,
    bank: &[Question],
    threshold: f64,
) -> Option<NearDuplicate> {
    let candidate_tokens = significant_tokens(candidate_title);
    if candidate_tokens.is_empty() {
        return None;
    }
    let mut best: Option<NearDuplicate> = None;
    for question in bank {
        let similarity =
            jaccard_similarity(&candidate_tokens, &significant_tokens(&question.title));
        if similarity < threshold {
            continue;
        }
        let is_better = match &best {
            None => true,
            Some(current) => {
                similarity > current.similarity
                    || (similarity == current.similarity
                        && (question.weight > current.question.weight
                            || (question.weight == current.question.weight
                                && question.id < current.question.id)))
            }
        };
        if is_better {
            best = Some(NearDuplicate {
                question: question.clone(),
                similarity,
            });
        }
    }
    best
}

// trace:STORY-86 | ai:claude
/// Jaccard overlap of two token sets: `|A ∩ B| / |A ∪ B|`. `0.0` when either
/// set is empty, `1.0` when they are identical.
fn jaccard_similarity(left: &BTreeSet<String>, right: &BTreeSet<String>) -> f64 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let intersection = left.intersection(right).count();
    let union = left.union(right).count();
    intersection as f64 / union as f64
}

// trace:STORY-86 | ai:claude
/// The set of significant lowercase word tokens in a question title: split on
/// non-alphanumeric characters, case-folded, with very common function words
/// dropped so "Is the self continuous?" and "Self continuity over time" compare
/// on their content words rather than their scaffolding.
fn significant_tokens(title: &str) -> BTreeSet<String> {
    const STOP_WORDS: &[&str] = &[
        "a", "an", "and", "are", "as", "at", "be", "by", "can", "do", "does", "for", "from", "if",
        "in", "is", "it", "of", "on", "or", "over", "that", "the", "to", "we", "you", "your",
    ];
    title
        .split(|character: char| !character.is_alphanumeric())
        .map(|word| word.trim().to_ascii_lowercase())
        .filter(|word| !word.is_empty() && !STOP_WORDS.contains(&word.as_str()))
        .collect()
}

// trace:STORY-66 | ai:claude
/// Rewrite a question's tag list for a re-weighting pass.
///
/// `weight:N` and `quality:*` are single-valued tags, so every existing
/// occurrence is dropped and exactly one fresh `weight:<new_weight>` and one
/// `quality:*` (from `signal`) are appended. All other tags keep their original
/// relative order. Pure — does not touch AIDA.
pub fn rewrite_weight_and_quality_tags(
    tags: &[String],
    new_weight: u32,
    signal: QualitySignal,
) -> Vec<String> {
    let mut rewritten: Vec<String> = tags
        .iter()
        .filter(|tag| !tag.starts_with("weight:") && !tag.starts_with("quality:"))
        .cloned()
        .collect();
    rewritten.push(format!("weight:{new_weight}"));
    rewritten.push(signal.quality_tag().to_string());
    rewritten
}

// trace:STORY-86 | ai:claude
#[cfg(test)]
mod dedup_tests {
    use super::{find_near_duplicate, DEDUP_SIMILARITY_THRESHOLD};
    use crate::model::{AnswerKind, Question};

    fn question(id: &str, title: &str, weight: u32) -> Question {
        Question {
            id: id.to_string(),
            title: title.to_string(),
            tags: vec!["answer:yes-no".to_string()],
            answer_kind: AnswerKind::YesNo,
            weight,
        }
    }

    #[test]
    fn exact_rephrasing_scores_full_similarity() {
        let bank = vec![question("Q-1", "Is the self continuous over time?", 50)];
        let found = find_near_duplicate(
            "Over time, is the self continuous?",
            &bank,
            DEDUP_SIMILARITY_THRESHOLD,
        )
        .expect("reordered/repunctuated rephrasing is a near-duplicate");
        assert_eq!(found.question.id, "Q-1");
        assert!((found.similarity - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn near_duplicate_above_threshold_is_offered_for_reuse() {
        let bank = vec![question(
            "Q-7",
            "Does free will require uncaused choice?",
            60,
        )];
        let found = find_near_duplicate(
            "Does free will require an uncaused choice?",
            &bank,
            DEDUP_SIMILARITY_THRESHOLD,
        )
        .expect("a one-stop-word difference clears the bar");
        assert_eq!(found.question.id, "Q-7");
        assert!(found.similarity >= DEDUP_SIMILARITY_THRESHOLD);
    }

    #[test]
    fn distinct_question_is_not_a_duplicate() {
        let bank = vec![question("Q-1", "Is the self continuous over time?", 50)];
        assert!(find_near_duplicate(
            "Does morality depend on consequences?",
            &bank,
            DEDUP_SIMILARITY_THRESHOLD
        )
        .is_none());
    }

    #[test]
    fn empty_bank_or_blank_candidate_finds_nothing() {
        let bank = vec![question("Q-1", "Is the self continuous over time?", 50)];
        assert!(find_near_duplicate("anything", &[], DEDUP_SIMILARITY_THRESHOLD).is_none());
        assert!(find_near_duplicate("   ?  ", &bank, DEDUP_SIMILARITY_THRESHOLD).is_none());
    }

    #[test]
    fn ties_break_toward_higher_weight_then_lower_id() {
        // Two identical-similarity matches; the heavier one wins.
        let bank = vec![
            question("Q-2", "Is the self continuous over time?", 40),
            question("Q-9", "Is the self continuous over time?", 70),
        ];
        let found = find_near_duplicate(
            "Is the self continuous over time?",
            &bank,
            DEDUP_SIMILARITY_THRESHOLD,
        )
        .expect("exact match present");
        assert_eq!(found.question.id, "Q-9");

        // Equal weight -> lower id wins.
        let bank = vec![
            question("Q-5", "Is the self continuous over time?", 50),
            question("Q-3", "Is the self continuous over time?", 50),
        ];
        let found = find_near_duplicate(
            "Is the self continuous over time?",
            &bank,
            DEDUP_SIMILARITY_THRESHOLD,
        )
        .expect("exact match present");
        assert_eq!(found.question.id, "Q-3");
    }
}

// trace:STORY-66 | ai:claude
#[cfg(test)]
mod reweight_tag_tests {
    use super::rewrite_weight_and_quality_tags;
    use crate::strategy::QualitySignal;

    #[test]
    fn replaces_weight_and_quality_preserving_order() {
        let tags = vec![
            "topic:meaning".to_string(),
            "weight:50".to_string(),
            "answer:yes-no".to_string(),
            "quality:neutral".to_string(),
            "seed".to_string(),
        ];
        let result = rewrite_weight_and_quality_tags(&tags, 62, QualitySignal::Insightful);
        assert_eq!(
            result,
            vec![
                "topic:meaning".to_string(),
                "answer:yes-no".to_string(),
                "seed".to_string(),
                "weight:62".to_string(),
                "quality:insightful".to_string(),
            ]
        );
    }

    #[test]
    fn adds_tags_when_absent() {
        let tags = vec!["topic:free-will".to_string()];
        let result = rewrite_weight_and_quality_tags(&tags, 30, QualitySignal::Punted);
        assert_eq!(
            result,
            vec![
                "topic:free-will".to_string(),
                "weight:30".to_string(),
                "quality:punted".to_string(),
            ]
        );
    }

    #[test]
    fn collapses_duplicate_single_valued_tags() {
        let tags = vec![
            "weight:10".to_string(),
            "weight:20".to_string(),
            "quality:unhelpful".to_string(),
            "quality:insightful".to_string(),
        ];
        let result = rewrite_weight_and_quality_tags(&tags, 0, QualitySignal::Unhelpful);
        assert_eq!(
            result,
            vec!["weight:0".to_string(), "quality:unhelpful".to_string()]
        );
    }
}
