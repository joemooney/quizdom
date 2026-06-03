use crate::error::{QuizdomError, Result};
use crate::model::{Answer, AnswerKind, Question};
use crate::style;
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

// trace:STORY-78 | ai:claude
/// Render the in-session orientation breadcrumb shown each frontier turn:
/// the current topic, exploration depth, and active branch. Keeping it on one
/// compact dimmed line keeps the user oriented in a long session without
/// crowding the question itself. The breadcrumb funnels through the same
/// styling gate as everything else, so non-TTY / `NO_COLOR` / test output stays
/// plain text.
pub(crate) fn render_breadcrumb(
    question: &Question,
    depth: usize,
    branch_id: &str,
    output: &mut impl Write,
) -> Result<()> {
    let line = breadcrumb_line(question, depth, branch_id);
    writeln!(output, "{}", style::paint(style::breadcrumb(), &line))?;
    Ok(())
}

// trace:STORY-78 | ai:claude
/// Pure formatter behind [`render_breadcrumb`], split out so the breadcrumb's
/// content is unit-testable without a buffer or the styling global.
pub(crate) fn breadcrumb_line(question: &Question, depth: usize, branch_id: &str) -> String {
    format!(
        "[topic: {} | depth: {} | branch: {}]",
        breadcrumb_topic(question),
        depth,
        branch_id
    )
}

// trace:STORY-78 | ai:claude
/// The human-facing topic for the breadcrumb, read from the question's
/// `topic:<slug>` tag (dashes rendered as spaces). Untagged questions — e.g. a
/// runtime-minted contradiction prompt — fall back to a stable placeholder so
/// the breadcrumb never disappears mid-session.
fn breadcrumb_topic(question: &Question) -> String {
    question
        .tags
        .iter()
        .find_map(|tag| tag.strip_prefix("topic:"))
        .map(str::trim)
        .filter(|topic| !topic.is_empty())
        .map(|topic| topic.replace('-', " "))
        .unwrap_or_else(|| "(general)".to_string())
}

pub(crate) fn render_question_for(
    question: &Question,
    context: InputContext,
    output: &mut impl Write,
) -> Result<()> {
    // trace:STORY-76 | ai:claude — a surfaced contradiction reuses this
    // renderer; style its prompt distinctly so the tension reads as a flag,
    // not just another question.
    let title_style = if question
        .tags
        .iter()
        .any(|tag| tag == "runtime:contradiction")
    {
        style::contradiction()
    } else {
        style::question()
    };
    writeln!(output, "\n{}", style::paint(title_style, &question.title))?;
    match &question.answer_kind {
        AnswerKind::YesNo => writeln!(
            output,
            "{}",
            style::paint(
                style::control(),
                &control_prompt("[Y] Yes  [N] No", context)
            )
        )?,
        AnswerKind::Choice(options) => {
            for (index, option) in options.iter().enumerate() {
                writeln!(
                    output,
                    "{} {}",
                    style::paint(style::option(), &format!("{}.", index + 1)),
                    option
                )?;
            }
            writeln!(
                output,
                "{}",
                style::paint(
                    style::control(),
                    &control_prompt(&format!("[1-{}] Choose", options.len()), context)
                )
            )?;
        }
        // trace:BUG-98 | ai:claude — free-text is rustyline line-mode
        // (STORY-55), so single keys can't be intercepted mid-edit. Display the
        // same control set as the single-key prompt, but as slash-commands the
        // line parser recognizes, so navigation is consistent across all kinds.
        AnswerKind::FreeText => writeln!(
            output,
            "Answer in your own words, or {}",
            free_text_controls(context)
        )?,
    }
    write!(output, "> ")?;
    output.flush()?;
    Ok(())
}

fn control_prompt(prefix: &str, context: InputContext) -> String {
    match context {
        // trace:STORY-88 | ai:claude — the quick-add control is offered only at
        // the frontier, where "add a question from here" is meaningful; the
        // review pane is for revising the saved path, not authoring.
        // trace:STORY-127 | ai:claude — the observer control ('?') is offered in
        // both contexts: it is non-destructive, so reading the exchange and
        // returning to the same prompt is always safe.
        InputContext::Frontier => {
            format!("{prefix}  [?] Observe  [X] eXplore  [A] Add  [P] Punt  [B] Back  [Q] Quit")
        }
        InputContext::Review => {
            format!("{prefix}  [?] Observe  [X] eXplore  [P] Punt  [B] Back  [F] Forward  [Q] Quit")
        }
    }
}

// trace:BUG-98 | ai:claude
/// The free-text prompt's control suffix, expressed as slash-commands so a
/// user editing a line can still navigate. Mirrors the single-key
/// [`control_prompt`] set for each context: the frontier offers `/add` (author
/// a question), review offers `/forward` (re-walk the saved path) instead.
fn free_text_controls(context: InputContext) -> String {
    // trace:STORY-127 | ai:claude — `/observe` (or `?`) mirrors the single-key
    // observer control for the free-text line-mode prompt.
    match context {
        InputContext::Frontier => {
            "/observe /explore /add /punt /back /quit to navigate.".to_string()
        }
        InputContext::Review => {
            "/observe /explore /punt /back /forward /quit to navigate.".to_string()
        }
    }
}

pub(crate) enum AnswerInput {
    Answer(Answer),
    Back,
    Forward,
    // trace:STORY-88 | ai:claude
    // The user pressed the in-session quick-add control to author + link a new
    // question from the current node mid-exploration.
    Add,
    // trace:STORY-127 | ai:claude
    // The user pressed the in-session observer control ('?') to get a
    // belief-neutral reading of the current exchange. Non-destructive: the
    // session shows the reading, then re-presents the SAME question.
    Observe,
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
        // trace:STORY-127 | ai:claude — the observer control is non-destructive,
        // so it is recognized in every context before any answer parsing.
        if is_observe_command(&raw) {
            return Ok(AnswerInput::Observe);
        }
        // trace:STORY-88 | ai:claude — quick-add is a frontier-only control.
        if context == InputContext::Frontier && is_add_command(&raw) {
            return Ok(AnswerInput::Add);
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
            // trace:STORY-127 | ai:claude — the observer key. Non-destructive in
            // every context, so it is accepted regardless of answer kind.
            KeyCode::Char('?') => "?",
            // trace:STORY-88 | ai:claude — frontier-only quick-add key.
            KeyCode::Char('a') | KeyCode::Char('A') if context == InputContext::Frontier => "/add",
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

// trace:BUG-98 | ai:claude — `/quit` joins the recognized end aliases so the
// free-text slash-command form matches the prompt the user is shown.
pub(crate) fn is_end_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "/end" | "/quit" | "q" | "quit"
    )
}

// trace:BUG-98 | ai:claude — `/back` joins the recognized aliases so the
// free-text slash-command form maps to the same Back action.
pub(crate) fn is_back_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "b" | "/b" | "/back" | "back"
    )
}

// trace:BUG-98 | ai:claude — `/forward` joins the recognized aliases so the
// free-text slash-command form maps to the same Forward action.
pub(crate) fn is_forward_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "f" | "/f" | "/forward" | "forward"
    )
}

// trace:STORY-127 | ai:claude
/// The in-session observer control: surface a belief-neutral reading of the
/// current exchange without disturbing it. Recognised as a bare `?`, `/?`,
/// `/observe`, or the word `observe`.
pub(crate) fn is_observe_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "?" | "/?" | "/observe" | "observe"
    )
}

// trace:STORY-88 | ai:claude
/// The in-session quick-add control: author + link a new question from the
/// current node. Recognised as a bare `a`, `/a`, `/add`, or the word `add`.
pub(crate) fn is_add_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "a" | "/a" | "/add" | "add"
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
        // trace:BUG-98 | ai:claude — recognize the full `/explore` / `/punt`
        // slash-commands (the form the free-text prompt advertises) alongside
        // the short `/x` / `/p` aliases. Bare words like `explore` stay a
        // legitimate free-text answer; only the leading-slash form is a command.
        AnswerKind::FreeText => match raw.trim().to_ascii_lowercase().as_str() {
            "x" | "/x" | "/explore" => Some("explore".to_string()),
            "p" | "/p" | "/punt" => Some("punt".to_string()),
            _ => {
                let other = raw.trim();
                (!other.is_empty()).then(|| other.to_string())
            }
        },
    }
}
