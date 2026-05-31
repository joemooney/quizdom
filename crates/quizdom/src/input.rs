use crate::error::{QuizdomError, Result};
use crate::model::{Answer, AnswerKind, Question};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use rustyline::{Config as RustylineConfig, DefaultEditor, EditMode};
use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

// trace:STORY-69 | ai:codex
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum InputContext {
    Frontier,
    Review,
}

pub(crate) fn render_question(question: &Question, output: &mut impl Write) -> Result<()> {
    render_question_for(question, InputContext::Frontier, output)
}

pub(crate) fn render_question_for(
    question: &Question,
    context: InputContext,
    output: &mut impl Write,
) -> Result<()> {
    writeln!(output, "\n{}", question.title)?;
    match &question.answer_kind {
        AnswerKind::YesNo => writeln!(output, "{}", control_prompt("[Y] Yes  [N] No", context))?,
        AnswerKind::Choice(options) => {
            for (index, option) in options.iter().enumerate() {
                writeln!(output, "{}. {}", index + 1, option)?;
            }
            writeln!(
                output,
                "{}",
                control_prompt(&format!("[1-{}] Choose", options.len()), context)
            )?;
        }
        AnswerKind::FreeText => writeln!(
            output,
            "Answer in your own words, or Q/Quit to end this session."
        )?,
    }
    write!(output, "> ")?;
    output.flush()?;
    Ok(())
}

fn control_prompt(prefix: &str, context: InputContext) -> String {
    match context {
        InputContext::Frontier => format!("{prefix}  [X] eXplore  [P] Punt  [B] Back  [Q] Quit"),
        InputContext::Review => {
            format!("{prefix}  [X] eXplore  [P] Punt  [B] Back  [F] Forward  [Q] Quit")
        }
    }
}

pub(crate) enum AnswerInput {
    Answer(Answer),
    Back,
    Forward,
    End,
}

pub(crate) enum FreeTextInput {
    Plain,
    Interactive(Box<DefaultEditor>),
}

struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
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
    context: InputContext,
    input: &mut impl BufRead,
    free_text_input: &mut FreeTextInput,
    output: &mut impl Write,
) -> Result<AnswerInput> {
    loop {
        let raw = match kind {
            AnswerKind::FreeText => free_text_input
                .read_line(input, output, "")?
                .ok_or_else(|| QuizdomError::Parse("no answer provided".to_string()))?,
            _ => read_control_answer_or_line(input, output, kind, context)?,
        };
        if is_end_command(&raw) {
            return Ok(AnswerInput::End);
        }
        if is_back_command(&raw) {
            return Ok(AnswerInput::Back);
        }
        if context == InputContext::Review && is_forward_command(&raw) {
            return Ok(AnswerInput::Forward);
        }
        if let Some(normalized) = normalize_answer(kind, &raw) {
            return Ok(AnswerInput::Answer(Answer { raw, normalized }));
        }
        write!(output, "Please enter a valid answer or /end: ")?;
        output.flush()?;
    }
}

fn read_control_answer_or_line(
    input: &mut impl BufRead,
    output: &mut impl Write,
    kind: &AnswerKind,
    context: InputContext,
) -> Result<String> {
    // trace:STORY-51 | ai:codex
    if io::stdin().is_terminal() {
        if let Some(raw) = read_single_key_answer(output, kind, context)? {
            return Ok(raw);
        }
    }
    let mut raw = String::new();
    if input.read_line(&mut raw)? == 0 {
        return Err(QuizdomError::Parse("no answer provided".to_string()));
    }
    Ok(raw.trim().to_string())
}

fn read_single_key_answer(
    output: &mut impl Write,
    kind: &AnswerKind,
    context: InputContext,
) -> Result<Option<String>> {
    let Ok(_raw_mode) = RawModeGuard::enter() else {
        return Ok(None);
    };
    loop {
        let event = event::read()
            .map_err(|error| QuizdomError::Io(io::Error::new(io::ErrorKind::Other, error)))?;
        let Event::Key(key) = event else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        let raw = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') if matches!(kind, AnswerKind::YesNo) => "y",
            KeyCode::Char('n') | KeyCode::Char('N') if matches!(kind, AnswerKind::YesNo) => "n",
            KeyCode::Char('x') | KeyCode::Char('X') => "x",
            KeyCode::Char('p') | KeyCode::Char('P') => "p",
            KeyCode::Char('b') | KeyCode::Char('B') => "b",
            KeyCode::Char('f') | KeyCode::Char('F') if context == InputContext::Review => "f",
            KeyCode::Char('q') | KeyCode::Char('Q') => "/end",
            KeyCode::Char(character) if matches!(kind, AnswerKind::Choice(_)) => {
                if character.is_ascii_digit() {
                    write!(output, "{character}\n")?;
                    output.flush()?;
                    return Ok(Some(character.to_string()));
                }
                continue;
            }
            KeyCode::Esc => "/end",
            _ => continue,
        };
        writeln!(output, "{raw}")?;
        output.flush()?;
        return Ok(Some(raw.to_string()));
    }
}

pub(crate) fn is_end_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "/end" | "q" | "quit"
    )
}

pub(crate) fn is_back_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "b" | "/b" | "back"
    )
}

pub(crate) fn is_forward_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "f" | "/f" | "forward"
    )
}

pub(crate) fn normalize_answer(kind: &AnswerKind, raw: &str) -> Option<String> {
    match kind {
        AnswerKind::YesNo => match raw.trim().to_ascii_lowercase().as_str() {
            "yes" | "y" => Some("yes".to_string()),
            "no" | "n" => Some("no".to_string()),
            "x" | "/x" | "explore" => Some("explore".to_string()),
            "p" | "/p" | "punt" => Some("punt".to_string()),
            _ => None,
        },
        AnswerKind::Choice(options) => {
            let trimmed = raw.trim();
            match trimmed.to_ascii_lowercase().as_str() {
                "x" | "/x" | "explore" => return Some("explore".to_string()),
                "p" | "/p" | "punt" => return Some("punt".to_string()),
                _ => {}
            }
            if let Ok(index) = trimmed.parse::<usize>() {
                return options.get(index.checked_sub(1)?).cloned();
            }
            options
                .iter()
                .find(|option| option.eq_ignore_ascii_case(trimmed))
                .cloned()
        }
        AnswerKind::FreeText => match raw.trim() {
            "x" | "/x" => Some("explore".to_string()),
            "p" | "/p" => Some("punt".to_string()),
            other => (!other.is_empty()).then(|| other.to_string()),
        },
    }
}
