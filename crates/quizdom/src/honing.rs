use crate::bank::QuestionBank;
use crate::error::Result;
use crate::input::FreeTextInput;
use crate::model::{Question, TermDefinition, TermMappingProposal};
use crate::persist::UserSpecificTermPersister;
use crate::strategy::NextQuestionStrategy;
use std::io::{BufRead, Write};

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct SettledTermDefinition {
    pub(crate) term_label: String,
    pub(crate) raw_meaning: String,
    pub(crate) term: TermDefinition,
}

pub(crate) fn prompt_for_term_meaning(
    definitions: &[TermDefinition],
    strategy: &dyn NextQuestionStrategy,
    term_persister: &dyn UserSpecificTermPersister,
    input: &mut impl BufRead,
    free_text_input: &mut FreeTextInput,
    output: &mut impl Write,
) -> Result<Option<SettledTermDefinition>> {
    if definitions.len() < 2 {
        return Ok(None);
    }
    // trace:STORY-42 | ai:codex
    let term_label = term_label(definitions);
    writeln!(output, "\nWhat do you mean by {term_label}?")?;
    let Some(raw) = free_text_input.read_line(input, output, "> ")? else {
        return Ok(None);
    };
    let meaning = raw.trim();
    if meaning.is_empty() || meaning == "/end" {
        return Ok(None);
    }
    if let Some(proposal) = strategy
        .map_term_meaning(&term_label, meaning, definitions)
        .unwrap_or(None)
    {
        render_term_mapping_proposal(&proposal, output)?;
        write!(output, "> ")?;
        output.flush()?;
        let mut confirmation = String::new();
        if input.read_line(&mut confirmation)? == 0 {
            return Ok(None);
        }
        if is_confirmation_yes(&confirmation) {
            writeln!(output, "Adopted {}.", proposal.term_title)?;
            let Some(term) = definitions
                .iter()
                .find(|definition| definition.id == proposal.term_id)
                .cloned()
            else {
                return Ok(None);
            };
            return Ok(Some(SettledTermDefinition {
                term_label,
                raw_meaning: meaning.to_string(),
                term,
            }));
        }
        writeln!(output, "What would make the shared definition fit better?")?;
        let Some(refinement) = free_text_input.read_line(input, output, "> ")? else {
            return Ok(None);
        };
        let refinement = refinement.trim();
        if refinement.is_empty() || refinement == "/end" {
            return Ok(None);
        }
        match term_persister.persist_user_specific_term(&term_label, refinement, definitions) {
            Ok(term) => {
                writeln!(
                    output,
                    "Recorded a user-specific definition: {} ({})",
                    term.title, term.id
                )?;
                return Ok(Some(SettledTermDefinition {
                    term_label,
                    raw_meaning: refinement.to_string(),
                    term,
                }));
            }
            Err(_) => writeln!(
                output,
                "No shared definition was adopted; user-specific persistence is unavailable."
            )?,
        }
    }
    Ok(None)
}

pub(crate) fn is_confirmation_yes(input: &str) -> bool {
    matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "yes" | "y" | "yeah" | "yep"
    )
}

pub(crate) fn term_label(definitions: &[TermDefinition]) -> String {
    definitions
        .iter()
        .find_map(|definition| {
            definition
                .tags
                .iter()
                .find_map(|tag| tag.strip_prefix("topic:"))
        })
        .map(|topic| topic.replace('-', " "))
        .unwrap_or_else(|| {
            definitions
                .first()
                .map(|definition| normalize_loaded_term(&definition.title))
                .filter(|term| !term.is_empty())
                .unwrap_or_else(|| "this term".to_string())
        })
}

pub(crate) fn render_term_mapping_proposal(
    proposal: &TermMappingProposal,
    output: &mut impl Write,
) -> Result<()> {
    writeln!(
        output,
        "That sounds closest to {}: {} Does this capture it?",
        proposal.term_title, proposal.definition
    )?;
    if !proposal.rationale.trim().is_empty() {
        writeln!(output, "Reason: {}", proposal.rationale.trim())?;
    }
    Ok(())
}

pub(crate) fn load_probed_terms(
    bank: &dyn QuestionBank,
    current: &Question,
) -> Vec<TermDefinition> {
    // trace:STORY-41 | ai:codex
    bank.probes(&current.id)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|term_ref| bank.load_term(&term_ref.id).ok())
        .collect()
}

pub(crate) fn definitions_for_loaded_terms(
    definitions: &[TermDefinition],
    loaded_terms: &[String],
) -> Vec<TermDefinition> {
    if loaded_terms.is_empty() {
        return Vec::new();
    }
    definitions
        .iter()
        .filter(|definition| {
            let title = normalize_loaded_term(&definition.title);
            loaded_terms.iter().any(|term| {
                let term = normalize_loaded_term(term);
                !term.is_empty() && (title.contains(&term) || term.contains(&title))
            })
        })
        .cloned()
        .collect()
}

pub(crate) fn normalize_loaded_term(term: &str) -> String {
    term.trim()
        .to_ascii_lowercase()
        .split(['/', ':', '('])
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn render_term_definitions(
    definitions: &[TermDefinition],
    output: &mut impl Write,
) -> Result<()> {
    if definitions.is_empty() {
        return Ok(());
    }
    writeln!(output, "\nTerms to distinguish:")?;
    for definition in definitions {
        let definition_kind = definition
            .tags
            .iter()
            .find_map(|tag| tag.strip_prefix("definition:"))
            .unwrap_or("definition");
        writeln!(
            output,
            "- {} ({definition_kind}): {}",
            definition.title, definition.definition
        )?;
    }
    Ok(())
}

pub(crate) fn render_settled_term_definition(
    settled: &SettledTermDefinition,
    output: &mut impl Write,
) -> Result<()> {
    // trace:STORY-44 | ai:codex
    writeln!(output, "\nSettled meaning for {}:", settled.term_label)?;
    writeln!(
        output,
        "- {}: {}",
        settled.term.title, settled.term.definition
    )?;
    Ok(())
}
