use crate::error::{QuizdomError, Result};
use crate::model::{answer_kind_from_tags, Question, QuestionRef, TermDefinition, TermRef};
use std::process::Command;

pub trait QuestionBank {
    fn load_question(&self, id: &str) -> Result<Question>;
    fn begets(&self, id: &str) -> Result<Vec<QuestionRef>>;
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
