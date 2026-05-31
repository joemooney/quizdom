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
