#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Question {
    pub id: String,
    pub title: String,
    pub tags: Vec<String>,
    pub answer_kind: AnswerKind,
    pub weight: u32,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum AnswerKind {
    YesNo,
    Choice(Vec<String>),
    FreeText,
}

impl AnswerKind {
    pub fn mode(&self) -> String {
        match self {
            Self::YesNo => "yes-no".to_string(),
            Self::Choice(options) => format!("choice[{}]", options.join(",")),
            Self::FreeText => "free-text".to_string(),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Answer {
    pub raw: String,
    pub normalized: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct QuestionRef {
    pub id: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TermRef {
    pub id: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TermDefinition {
    pub id: String,
    pub title: String,
    pub tags: Vec<String>,
    pub definition: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TermMappingProposal {
    pub term_id: String,
    pub term_title: String,
    pub definition: String,
    pub rationale: String,
}

// trace:STORY-86 | ai:claude
/// An LLM-proposed improvement to a user-authored question, presented for the
/// user to approve before the question is persisted.
///
/// The user either approves the proposal (the refined wording / answer shape
/// is adopted) or keeps their own phrasing verbatim. `weak_socratic` flags a
/// question the LLM judged a poor Socratic prompt (e.g. leading, factual, or
/// answerable with a single fact) so the UI can warn before persisting.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RefinementProposal {
    /// The LLM's improved phrasing of the question.
    pub refined_title: String,
    /// The answer shape the LLM suggests for the refined question.
    pub suggested_answer_kind: AnswerKind,
    /// True when the LLM judged the question a weak Socratic prompt.
    pub weak_socratic: bool,
    /// Short reason for the refinement / weak-Socratic flag.
    pub rationale: String,
}

// trace:STORY-48 | ai:claude
/// The triggering answer recorded on an answer-conditioned `begets` follow-on,
/// read from a `from-answer:<value>` tag. `None` for unconditional follow-ons.
pub(crate) fn from_answer_tag(tags: &[String]) -> Option<&str> {
    tags.iter()
        .find_map(|tag| tag.strip_prefix("from-answer:"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(crate) fn answer_kind_from_tags(tags: &[String]) -> Option<AnswerKind> {
    tags.iter().find_map(|tag| {
        if tag == "answer:yes-no" {
            Some(AnswerKind::YesNo)
        } else if tag == "answer:free-text" {
            Some(AnswerKind::FreeText)
        } else {
            tag.strip_prefix("answer:choice[").and_then(|rest| {
                let options = rest.strip_suffix(']')?;
                let options = options
                    .split([',', '|'])
                    .map(str::trim)
                    .filter(|option| !option.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                (!options.is_empty()).then_some(AnswerKind::Choice(options))
            })
        }
    })
}
