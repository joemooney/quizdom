use crate::error::{QuizdomError, Result};
use crate::model::{Answer, AnswerKind, Question};
use rustyline::{Config as RustylineConfig, DefaultEditor, EditMode};
use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

pub(crate) fn render_question(question: &Question, output: &mut impl Write) -> Result<()> {
    writeln!(output, "\n{}", question.title)?;
    match &question.answer_kind {
        AnswerKind::YesNo => writeln!(output, "Answer yes or no, or /end to end this session.")?,
        AnswerKind::Choice(options) => {
            for (index, option) in options.iter().enumerate() {
                writeln!(output, "{}. {}", index + 1, option)?;
            }
            writeln!(output, "Enter a choice, or /end to end this session.")?;
        }
        AnswerKind::FreeText => writeln!(
            output,
            "Answer in your own words, or /end to end this session."
        )?,
    }
    write!(output, "> ")?;
    output.flush()?;
    Ok(())
}

pub(crate) enum AnswerInput {
    Answer(Answer),
    End,
}

pub(crate) enum FreeTextInput {
    Plain,
    Interactive(Box<DefaultEditor>),
}

impl FreeTextInput {
    pub(crate) fn from_stdin() -> Result<Self> {
        if io::stdin().is_terminal() {
            Self::interactive()
        } else {
            Ok(Self::Plain)
        }
    }

    fn interactive() -> Result<Self> {
        // trace:STORY-55 | ai:codex
        let config = RustylineConfig::builder()
            .edit_mode(editor_edit_mode())
            .build();
        let editor = DefaultEditor::with_config(config)
            .map_err(|error| QuizdomError::Io(io::Error::new(io::ErrorKind::Other, error)))?;
        Ok(Self::Interactive(Box::new(editor)))
    }

    pub(crate) fn read_line(
        &mut self,
        input: &mut impl BufRead,
        output: &mut impl Write,
        prompt: &str,
    ) -> Result<Option<String>> {
        match self {
            Self::Plain => {
                write!(output, "{prompt}")?;
                output.flush()?;
                let mut raw = String::new();
                if input.read_line(&mut raw)? == 0 {
                    Ok(None)
                } else {
                    Ok(Some(raw.trim().to_string()))
                }
            }
            Self::Interactive(editor) => match editor.readline(prompt) {
                Ok(line) => {
                    if !line.trim().is_empty() {
                        let _ = editor.add_history_entry(line.as_str());
                    }
                    Ok(Some(line.trim().to_string()))
                }
                Err(rustyline::error::ReadlineError::Interrupted)
                | Err(rustyline::error::ReadlineError::Eof) => Ok(None),
                Err(error) => Err(QuizdomError::Io(io::Error::new(
                    io::ErrorKind::Other,
                    error,
                ))),
            },
        }
    }
}

fn editor_edit_mode() -> EditMode {
    let editor = env::var("EDITOR")
        .ok()
        .or_else(|| env::var("VISUAL").ok())
        .unwrap_or_default();
    edit_mode_from_editor(&editor)
}

pub(crate) fn edit_mode_from_editor(editor: &str) -> EditMode {
    let editor_name = Path::new(&editor)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(editor)
        .to_ascii_lowercase();
    match editor_name.as_str() {
        "vi" | "vim" | "nvim" => EditMode::Vi,
        _ => EditMode::Emacs,
    }
}

pub(crate) fn read_answer_or_end(
    kind: &AnswerKind,
    input: &mut impl BufRead,
    free_text_input: &mut FreeTextInput,
    output: &mut impl Write,
) -> Result<AnswerInput> {
    loop {
        let raw = match kind {
            AnswerKind::FreeText => free_text_input
                .read_line(input, output, "")?
                .ok_or_else(|| QuizdomError::Parse("no answer provided".to_string()))?,
            _ => {
                let mut raw = String::new();
                if input.read_line(&mut raw)? == 0 {
                    return Err(QuizdomError::Parse("no answer provided".to_string()));
                }
                raw.trim().to_string()
            }
        };
        if raw == "/end" {
            return Ok(AnswerInput::End);
        }
        if let Some(normalized) = normalize_answer(kind, &raw) {
            return Ok(AnswerInput::Answer(Answer { raw, normalized }));
        }
        write!(output, "Please enter a valid answer or /end: ")?;
        output.flush()?;
    }
}

pub(crate) fn normalize_answer(kind: &AnswerKind, raw: &str) -> Option<String> {
    match kind {
        AnswerKind::YesNo => match raw.trim().to_ascii_lowercase().as_str() {
            "yes" | "y" => Some("yes".to_string()),
            "no" | "n" => Some("no".to_string()),
            _ => None,
        },
        AnswerKind::Choice(options) => {
            let trimmed = raw.trim();
            if let Ok(index) = trimmed.parse::<usize>() {
                return options.get(index.checked_sub(1)?).cloned();
            }
            options
                .iter()
                .find(|option| option.eq_ignore_ascii_case(trimmed))
                .cloned()
        }
        AnswerKind::FreeText => (!raw.trim().is_empty()).then(|| raw.trim().to_string()),
    }
}
