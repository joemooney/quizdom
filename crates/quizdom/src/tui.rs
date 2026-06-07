// trace:STORY-169 | ai:claude
//! The ratatui TUI FRONT-END — the second implementation of the [`FrontEnd`]
//! seam (STORY-168 / ADR-166 / EPIC-167).
//!
//! ADR-166 reverses the EPIC-162 incremental crossterm overlay (STORY-163): that
//! palette only opened AFTER Enter and line-printed itself down the page because
//! rustyline owned the line and there was no alternate screen / cursor control.
//! STORY-169 adopts a REAL TUI: the [`TuiFrontEnd`] owns the event loop, so `/`
//! pops a LIVE palette on the keystroke, everything redraws IN PLACE, and the
//! screen has a proper layout.
//!
//! ## Where it sits
//!
//! The session ENGINE is unchanged — it still renders through [`FrontEnd::out`]
//! and requests input/control through [`FrontEnd::read_answer`] /
//! [`FrontEnd::read_line`]. The line front-end ([`crate::frontend::LineFrontEnd`])
//! writes those render intents straight to a byte sink; the TUI front-end instead
//! BUFFERS them (with color disabled, so the engine emits plain text) and, on each
//! input request, flushes the buffered text into a scrollable TRANSCRIPT pane,
//! draws the full-screen layout (transcript · input box · status bar), and runs an
//! event loop that gathers the next answer/line — opening the live `/` palette
//! overlay on the keystroke. The engine never knows it is talking to a TUI.
//!
//! ## What is testable without a terminal
//!
//! The interactive look is human-reviewed later (STORY-169 acceptance), but the
//! mechanics are unit-tested here:
//!
//! - [`select_front_end`] — the headless-vs-TUI selection decision (pure).
//! - [`layout`] — the three-pane layout math (pure over a [`Rect`]).
//! - [`TranscriptPane`] — the wrap + scroll model (pure).
//! - [`StatusLine::render`] — the goal/breadcrumb/roundedness/mode line (pure).
//! - the live `/` palette reuses [`crate::palette::PaletteState`], already pure.

use crate::error::{QuizdomError, Result};
use crate::frontend::FrontEnd;
use crate::input::{
    goal_command_text, help_command_text, is_add_command, is_back_command, is_end_command,
    is_forward_command, is_observe_command, is_rest_command, is_synopsis_command,
    is_terminate_command, is_verdict_command, mode_command_text, normalize_answer,
    tutor_command_text, AnswerInput, InputContext,
};
use crate::model::{Answer, AnswerKind};
use crate::palette::{command_registry, PaletteState};
use crate::style::theme;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Terminal;
use std::io::{self, BufRead, Stdout, Write};

/// Which front-end to build for a session, decided once at the engine boundary.
///
/// Belief-neutral plumbing: this only chooses HOW input/output flow, never WHAT
/// is asked. The TUI is the default for an interactive TTY; everything else (a
/// non-TTY stream, `--no-tui`, the non-interactive standalone commands) gets the
/// headless line front-end so the ~336 piped/byte tests and scripted runs are
/// untouched.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum FrontEndChoice {
    /// The ratatui full-screen TUI (interactive TTY, not `--no-tui`).
    Tui,
    /// The headless line front-end (non-TTY, `--no-tui`, tests, standalone).
    Headless,
}

/// Decide which front-end a session should use.
///
/// The TUI is selected ONLY when every condition holds: the session is
/// interactive (`interactive` — true for `start`/`resume`/`fork`), the user did
/// not pass `--no-tui`, and BOTH stdin and stdout are real terminals. A failure
/// of any condition falls back to [`FrontEndChoice::Headless`], so a piped
/// stdin, a redirected stdout, a `--no-tui` flag, or a non-interactive command
/// all keep today's line behavior. Pure, so the policy is unit-testable without a
/// real terminal.
pub(crate) fn select_front_end(
    interactive: bool,
    no_tui: bool,
    stdin_is_tty: bool,
    stdout_is_tty: bool,
) -> FrontEndChoice {
    if interactive && !no_tui && stdin_is_tty && stdout_is_tty {
        FrontEndChoice::Tui
    } else {
        FrontEndChoice::Headless
    }
}

/// The three stacked panes of the TUI, in screen order.
///
/// A scrollable TRANSCRIPT pane on top (the Q&A + meta-channel dialogue), a
/// single-line INPUT box, and a STATUS bar (goal · breadcrumb · roundedness ·
/// mode). Returned as a struct of [`Rect`]s so the layout math is testable
/// without drawing.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct TuiLayout {
    pub(crate) transcript: Rect,
    pub(crate) input: Rect,
    pub(crate) status: Rect,
}

/// Split the terminal area into the transcript / input / status panes.
///
/// The status bar and the input box are fixed-height (3 rows each: one content
/// row plus the border), and the transcript pane takes the rest — so it grows
/// with the window and never starves the input or status. Pure over `area`.
pub(crate) fn layout(area: Rect) -> TuiLayout {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),    // transcript (grows)
            Constraint::Length(3), // input box (1 row + borders)
            Constraint::Length(3), // status bar (1 row + borders)
        ])
        .split(area);
    TuiLayout {
        transcript: chunks[0],
        input: chunks[1],
        status: chunks[2],
    }
}

/// The scrollable transcript model: the wrapped dialogue lines plus the current
/// scroll offset (top visible line). Pure — the ratatui draw call reads
/// [`visible_offset`] and the lines; everything that decides WHAT is visible is
/// here so it can be unit-tested without a terminal.
#[derive(Debug, Default, Clone)]
pub(crate) struct TranscriptPane {
    /// Every line of dialogue accumulated so far (newest at the bottom). These
    /// are the plain-text render intents the engine wrote, split on newlines.
    lines: Vec<String>,
    /// The first visible line (top of the viewport). `0` is the oldest line.
    scroll: usize,
    /// Whether the pane is pinned to the bottom (follow mode). New output keeps
    /// the newest lines in view until the user scrolls up.
    follow: bool,
}

impl TranscriptPane {
    pub(crate) fn new() -> Self {
        Self {
            lines: Vec::new(),
            scroll: 0,
            follow: true,
        }
    }

    /// Append a block of (possibly multi-line) plain text to the transcript,
    /// splitting on newlines. Trailing partial lines are kept whole. Re-pins to
    /// the bottom when in follow mode so the freshest output stays in view.
    pub(crate) fn push_block(&mut self, text: &str) {
        for line in text.split('\n') {
            self.lines.push(line.to_string());
        }
        // Collapse a trailing empty line produced by a final '\n' so the pane
        // does not accumulate blank rows after every flush.
        if self.lines.last().map(String::is_empty).unwrap_or(false) {
            self.lines.pop();
        }
    }

    /// The total number of lines held.
    pub(crate) fn len(&self) -> usize {
        self.lines.len()
    }

    /// Borrow the lines (for rendering / tests).
    pub(crate) fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Clamp + return the scroll offset that should sit at the top of a viewport
    /// `height` rows tall. In follow mode this is the bottom of the buffer (so
    /// the newest `height` lines show); otherwise it is the user's scroll
    /// position, clamped so the viewport never runs past the end.
    pub(crate) fn visible_offset(&self, height: usize) -> usize {
        let max_top = self.lines.len().saturating_sub(height.max(1));
        if self.follow {
            max_top
        } else {
            self.scroll.min(max_top)
        }
    }

    /// Scroll up `n` lines, leaving follow mode (the user is now reading back).
    pub(crate) fn scroll_up(&mut self, n: usize, height: usize) {
        // Read the current top WHILE still following (so we start from the
        // bottom-pinned position), THEN drop follow mode and move up.
        let current = self.visible_offset(height);
        self.follow = false;
        self.scroll = current.saturating_sub(n);
    }

    /// Scroll down `n` lines. Reaching the bottom re-enters follow mode so new
    /// output resumes auto-scrolling.
    pub(crate) fn scroll_down(&mut self, n: usize, height: usize) {
        let max_top = self.lines.len().saturating_sub(height.max(1));
        let current = self.visible_offset(height);
        let next = (current + n).min(max_top);
        self.scroll = next;
        if next >= max_top {
            self.follow = true;
        }
    }
}

/// The status bar contents: the live session orientation the engine tracks
/// (goal · breadcrumb topic/depth/branch · roundedness · mode). Rendered as a
/// single compact line. The fields are filled from the last breadcrumb/goal the
/// engine emitted (parsed out of the transcript), so the bar stays in sync
/// without the engine knowing about the TUI.
#[derive(Debug, Default, Clone)]
pub(crate) struct StatusLine {
    pub(crate) breadcrumb: Option<String>,
    pub(crate) mode: Option<String>,
}

impl StatusLine {
    /// Render the status line as plain text (segments joined by `·`). Pure so the
    /// composition is testable; the draw call wraps it in a styled paragraph.
    pub(crate) fn render(&self) -> String {
        let mut segments: Vec<String> = Vec::new();
        if let Some(breadcrumb) = self
            .breadcrumb
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            segments.push(breadcrumb.to_string());
        }
        if let Some(mode) = self
            .mode
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            segments.push(format!("mode: {mode}"));
        }
        if segments.is_empty() {
            "quizdom — / for commands · ↑/↓ scroll · Enter to answer".to_string()
        } else {
            segments.join("  ·  ")
        }
    }

    /// Update the status line from a freshly-emitted transcript block: pick up the
    /// most recent breadcrumb line (`[topic: … | depth: … | branch: …]`) so the
    /// bar tracks the engine's orientation. Belief-neutral: it only mirrors what
    /// the engine already printed.
    pub(crate) fn observe_block(&mut self, text: &str) {
        for line in text.split('\n') {
            let trimmed = line.trim();
            if trimmed.starts_with("[topic:") && trimmed.ends_with(']') {
                self.breadcrumb = Some(trimmed.trim_matches(['[', ']']).to_string());
            }
            // A `/mode` confirmation echoes "Mode set: <mode>"; mirror it.
            if let Some(rest) = trimmed.strip_prefix("Mode set: ") {
                self.mode = Some(rest.split('\n').next().unwrap_or(rest).trim().to_string());
            }
            if let Some(rest) = trimmed.strip_prefix("Current mode: ") {
                self.mode = Some(rest.trim().to_string());
            }
        }
    }
}

/// RAII guard that owns the alternate screen + raw mode for the TUI's lifetime
/// and restores the terminal on Drop — on a clean return OR an unwind. Paired
/// with a panic hook ([`install_panic_hook`]) so a panic inside the event loop
/// still leaves the user's terminal usable.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().map_err(QuizdomError::Io)?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).map_err(QuizdomError::Io)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort restore; never panic in Drop. Mirror the panic hook so the
        // terminal is usable whether we exit cleanly or unwind.
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Install a panic hook that restores the terminal (raw mode + alternate screen)
/// BEFORE the default hook prints the panic, so a panic mid-session never leaves
/// the user staring at a wedged, no-echo terminal. Idempotent in effect — it
/// chains the previous hook so the panic message still surfaces.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        previous(info);
    }));
}

/// The ratatui TUI front-end: the engine's [`FrontEnd`] talking to a full-screen
/// terminal instead of a line stream.
///
/// Render intents written through [`FrontEnd::out`] are buffered in `pending`
/// (the engine runs with color disabled, so they are plain text). On each input
/// request the buffer is flushed into the [`TranscriptPane`], the screen is
/// redrawn, and an event loop gathers the next answer/line — opening the live
/// `/` palette overlay on the keystroke. `R: BufRead` is the fallback line
/// source the nested headless quick-add reads from ([`FrontEnd::author_io`]); in
/// real use it is empty (the TUI does not script the quick-add), but keeping it
/// satisfies the seam so the authoring core stays unchanged.
pub(crate) struct TuiFrontEnd<R: BufRead> {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    _guard: TerminalGuard,
    transcript: TranscriptPane,
    status: StatusLine,
    /// Bytes the engine has written via `out()` but not yet flushed to the pane.
    pending: Vec<u8>,
    /// The fallback line source + sink for the nested headless quick-add.
    author_input: R,
    author_output: Vec<u8>,
}

impl<R: BufRead> TuiFrontEnd<R> {
    /// Enter the alternate screen and build the TUI front-end. Installs the panic
    /// hook and disables engine-side color (the TUI owns visual styling, so the
    /// engine must emit plain text into the transcript buffer).
    pub(crate) fn new(author_input: R) -> Result<Self> {
        install_panic_hook();
        // The engine paints ANSI when color is enabled; the transcript pane wants
        // plain text it can re-style, so force color off for the TUI session.
        crate::style::set_enabled(false);
        let guard = TerminalGuard::enter()?;
        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::new(backend).map_err(QuizdomError::Io)?;
        let mut tui = Self {
            terminal,
            _guard: guard,
            transcript: TranscriptPane::new(),
            status: StatusLine::default(),
            pending: Vec::new(),
            author_input,
            author_output: Vec::new(),
        };
        tui.transcript.push_block(
            "quizdom — interactive session. Type your answer and press Enter. Press / for the \
             command palette, ↑/↓ to scroll the transcript.",
        );
        Ok(tui)
    }

    /// Move everything written via `out()` since the last flush into the
    /// transcript pane (and update the status bar from it).
    fn flush_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.status.observe_block(&text);
        self.transcript.push_block(&text);
        self.pending.clear();
    }

    /// Draw the three panes (and, when open, the palette overlay) for the current
    /// state. `editing` is the text in the input box; `palette` is `Some` while
    /// the `/` overlay is open.
    fn draw(&mut self, editing: &str, palette: Option<(&PaletteState, bool)>) -> Result<()> {
        let transcript = &self.transcript;
        let status_text = self.status.render();
        self.terminal
            .draw(|frame| {
                let panes = layout(frame.area());

                // ----- transcript pane -----
                let inner_height = panes.transcript.height.saturating_sub(2) as usize;
                let offset = transcript.visible_offset(inner_height);
                // Per-role colors + quote-attribution: each visible row is
                // classified to a voice and split into themed spans.
                // trace:STORY-171 | ai:claude
                let body: Vec<Line> = transcript
                    .lines()
                    .iter()
                    .skip(offset)
                    .map(|line| styled_transcript_line(line))
                    .collect();
                let follow_hint = if offset + inner_height < transcript.len() {
                    " (scrolled — ↓ to follow) "
                } else {
                    " transcript "
                };
                let transcript_widget = Paragraph::new(body)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(theme::border())
                            .title(follow_hint),
                    )
                    .wrap(Wrap { trim: false });
                frame.render_widget(transcript_widget, panes.transcript);

                // ----- input box -----
                // A GOLD cursor marker; the typed answer reads in the user color.
                // trace:STORY-171 | ai:claude
                let input_widget = Paragraph::new(Line::from(vec![
                    Span::styled("> ", theme::input_marker()),
                    Span::styled(editing.to_string(), theme::role_style(theme::Role::User)),
                ]))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(theme::border())
                        .title(" your answer "),
                );
                frame.render_widget(input_widget, panes.input);
                // Park the cursor at the end of the input text.
                let cursor_x = panes.input.x + 1 + 2 + editing.chars().count() as u16;
                let cursor_y = panes.input.y + 1;
                frame.set_cursor_position((
                    cursor_x.min(panes.input.right().saturating_sub(1)),
                    cursor_y,
                ));

                // ----- status bar -----
                // Colorized segments (goal/breadcrumb/roundedness/mode) distinct
                // from the transcript palette. trace:STORY-171 | ai:claude
                let status_widget = Paragraph::new(styled_status_line(&status_text)).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(theme::border())
                        .title(" status "),
                );
                frame.render_widget(status_widget, panes.status);

                // ----- palette overlay (drawn in place, on top) -----
                if let Some((state, show_detail)) = palette {
                    let overlay = palette_rect(frame.area());
                    frame.render_widget(Clear, overlay);
                    let text = crate::palette::render_to_string(state, show_detail);
                    let body: Vec<Line> = text.lines().map(|l| Line::from(l.to_string())).collect();
                    let widget = Paragraph::new(body)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(theme::border())
                                .title(" command palette "),
                        )
                        .wrap(Wrap { trim: false });
                    frame.render_widget(widget, overlay);
                }
            })
            .map_err(QuizdomError::Io)?;
        Ok(())
    }

    /// Run the live `/` palette overlay starting from the bare `/` already typed.
    /// Filters as the user types, arrow-navigates, Enter runs the highlighted
    /// command (returning its canonical typed form), `?` toggles per-command
    /// detail, Esc / backspacing past the `/` cancels. Redrawn IN PLACE each
    /// keystroke. `editing` is the input-box text to keep showing behind it.
    fn run_palette(&mut self, editing: &str) -> Result<Option<String>> {
        let mut state = PaletteState::new(command_registry());
        let mut show_detail = false;
        loop {
            self.draw(editing, Some((&state, show_detail)))?;
            let Event::Key(key) = event::read().map_err(QuizdomError::Io)? else {
                continue;
            };
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
            match key.code {
                KeyCode::Esc => return Ok(None),
                KeyCode::Enter => {
                    if let Some(command) = state.highlighted() {
                        return Ok(Some(command.command.to_string()));
                    }
                }
                KeyCode::Up => {
                    show_detail = false;
                    state.move_up();
                }
                KeyCode::Down => {
                    show_detail = false;
                    state.move_down();
                }
                KeyCode::Char('?') => show_detail = !show_detail,
                KeyCode::Backspace => {
                    show_detail = false;
                    if !state.pop_filter() {
                        // Backspacing past the `/` closes the overlay.
                        return Ok(None);
                    }
                }
                KeyCode::Char(c) => {
                    show_detail = false;
                    state.push_filter(c);
                }
                _ => {}
            }
        }
    }

    /// The shared input loop: flush pending output, draw, then gather one line of
    /// text from the keyboard. Handles editing (chars / Backspace), transcript
    /// scrolling (↑/↓ / PageUp / PageDown), the live `/` palette (on a bare `/`
    /// at the start of an empty line), Enter (submit), and Ctrl-D / Ctrl-C (EOF).
    /// Returns `None` on EOF so the engine winds down gracefully, mirroring the
    /// line front-end's non-TTY EOF.
    fn read_text_line(&mut self, prompt: Option<&str>) -> Result<Option<String>> {
        self.flush_pending();
        if let Some(prompt) = prompt.map(str::trim).filter(|p| !p.is_empty()) {
            self.transcript.push_block(prompt);
        }
        let mut editing = String::new();
        loop {
            self.draw(&editing, None)?;
            let viewport = self.viewport_height();
            let Event::Key(key) = event::read().map_err(QuizdomError::Io)? else {
                continue;
            };
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
            // Ctrl-C / Ctrl-D end input (EOF), like the line front-end.
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
            {
                return Ok(None);
            }
            match key.code {
                KeyCode::Enter => {
                    let line = editing.trim().to_string();
                    self.transcript.push_block(&format!("> {line}"));
                    return Ok(Some(line));
                }
                KeyCode::Backspace => {
                    editing.pop();
                }
                KeyCode::Up => self.transcript.scroll_up(1, viewport),
                KeyCode::Down => self.transcript.scroll_down(1, viewport),
                KeyCode::PageUp => self.transcript.scroll_up(viewport.max(1), viewport),
                KeyCode::PageDown => self.transcript.scroll_down(viewport.max(1), viewport),
                KeyCode::Char('/') if editing.is_empty() => {
                    // A bare `/` at the start of the line opens the LIVE palette
                    // on the keystroke (the EPIC-167 fix). A selected command is
                    // returned as its canonical typed form and submitted, routing
                    // through the SAME recognizers as the typed form.
                    if let Some(command) = self.run_palette(&editing)? {
                        self.transcript.push_block(&format!("> {command}"));
                        return Ok(Some(command));
                    }
                    // Cancelled — fall back to the prompt with an empty line.
                }
                KeyCode::Char(c) => editing.push(c),
                _ => {}
            }
        }
    }

    /// The transcript viewport height in rows for the CURRENT terminal size,
    /// used for scroll math between draws.
    fn viewport_height(&self) -> usize {
        let area = self
            .terminal
            .size()
            .map(|s| Rect::new(0, 0, s.width, s.height));
        match area {
            Ok(area) => layout(area).transcript.height.saturating_sub(2) as usize,
            Err(_) => 1,
        }
    }
}

impl<R: BufRead> FrontEnd for TuiFrontEnd<R> {
    fn out(&mut self) -> &mut dyn Write {
        &mut self.pending
    }

    fn read_answer(&mut self, kind: &AnswerKind, context: InputContext) -> Result<AnswerInput> {
        // Re-present the question until a recognized answer/control arrives. The
        // engine already rendered the question text through `out()`, so we only
        // gather + parse here. Parsing reuses the SAME recognizers as the line
        // front-end (input.rs), so a typed answer and a palette selection route
        // identically — the acceptance guarantee carried over from STORY-163.
        loop {
            let raw = match self.read_text_line(None)? {
                Some(raw) => raw,
                None => return Ok(AnswerInput::End),
            };
            if let Some(parsed) = parse_control(&raw, context) {
                return Ok(parsed);
            }
            if let Some(normalized) = normalize_answer(kind, &raw) {
                return Ok(AnswerInput::Answer(Answer { raw, normalized }));
            }
            self.transcript
                .push_block("Please enter a valid answer or /quit.");
        }
    }

    fn read_line(&mut self, prompt: &str) -> Result<Option<String>> {
        self.read_text_line(Some(prompt))
    }

    fn read_raw_line(&mut self) -> Result<Option<String>> {
        // The TUI gathers trimmed lines; hand back a newline-terminated form so
        // callers that expect `BufRead::read_line` semantics (the term-honing
        // confirmation) see a consistent shape.
        Ok(self.read_text_line(None)?.map(|line| format!("{line}\n")))
    }

    fn author_io(&mut self) -> (&mut dyn BufRead, &mut dyn Write) {
        // The nested headless quick-add core reads many prompts straight off a
        // line stream. In the TUI we feed it from `author_input` (empty in real
        // use — the quick-add UI is a STORY-170 concern) and capture its bytes;
        // this keeps the authoring core unchanged and the seam honest.
        (&mut self.author_input, &mut self.author_output)
    }
}

/// The centered overlay rectangle for the palette, sized as a fraction of the
/// screen and clamped to a sensible minimum. Pure over the full area so the
/// placement is testable.
fn palette_rect(area: Rect) -> Rect {
    let width = area.width.saturating_mul(3) / 4;
    let width = width.clamp(20.min(area.width), area.width);
    let height = (area.height.saturating_mul(3) / 4).clamp(6.min(area.height), area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

/// Parse a raw input line into a control [`AnswerInput`], or `None` when it is an
/// ordinary answer. Mirrors the recognizer order in
/// [`crate::input::read_answer_or_end`] EXACTLY so the TUI and the line front-end
/// route identical commands to identical actions (the front-end-agnostic-engine
/// contract). Context-sensitive controls (`/add` frontier-only, `/forward`
/// review-only) honor the same context gates.
fn parse_control(raw: &str, context: InputContext) -> Option<AnswerInput> {
    if is_end_command(raw) {
        return Some(AnswerInput::End);
    }
    if is_back_command(raw) {
        return Some(AnswerInput::Back);
    }
    if is_observe_command(raw) {
        return Some(AnswerInput::Observe);
    }
    if is_synopsis_command(raw) {
        return Some(AnswerInput::Synopsis);
    }
    if let Some(goal) = goal_command_text(raw) {
        return Some(AnswerInput::Goal(goal));
    }
    if let Some(mode) = mode_command_text(raw) {
        return Some(AnswerInput::Mode(mode));
    }
    if is_rest_command(raw) {
        return Some(AnswerInput::Rest);
    }
    if is_verdict_command(raw) {
        return Some(AnswerInput::Verdict);
    }
    if is_terminate_command(raw) {
        return Some(AnswerInput::Terminate);
    }
    if let Some(question) = help_command_text(raw) {
        return Some(AnswerInput::Help(question));
    }
    if let Some(text) = tutor_command_text(raw) {
        return Some(AnswerInput::Tutor(text));
    }
    if context == InputContext::Frontier && is_add_command(raw) {
        return Some(AnswerInput::Add);
    }
    if context == InputContext::Review && is_forward_command(raw) {
        return Some(AnswerInput::Forward);
    }
    None
}

// trace:STORY-171 | ai:claude
/// Build a styled ratatui [`Line`] for one transcript row: attribute the row to
/// a voice ([`theme::classify_line`]) and split it into colored spans
/// ([`theme::line_fragments`]) — applying SYMMETRIC QUOTE ATTRIBUTION across the
/// interrogator<->user pair (a quoted span renders in the OPPOSING role's color,
/// since each party quotes the other). Pure over the plain text the engine
/// emitted, so the per-role coloring is testable without a terminal.
fn styled_transcript_line(text: &str) -> Line<'static> {
    let role = theme::classify_line(text);
    let spans: Vec<Span<'static>> = theme::line_fragments(role, text)
        .into_iter()
        .map(|fragment| Span::styled(fragment.text, fragment.style))
        .collect();
    Line::from(spans)
}

// trace:STORY-171 | ai:claude
/// Colorize the status bar: split the rendered status text into `·`-separated
/// segments and paint each `label: value` pair with the theme's label/value
/// colors (a bare segment — e.g. the default hint — stays dim). Distinct from
/// the transcript palette so the bar reads as chrome, not dialogue. Pure over
/// the already-composed status string.
fn styled_status_line(status_text: &str) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let segments: Vec<&str> = status_text.split('·').collect();
    for (i, segment) in segments.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                "·".to_string(),
                Style::default().fg(theme::STATUS_DIM),
            ));
        }
        let seg = *segment;
        match seg.split_once(':') {
            Some((label, value)) if !value.trim().is_empty() => {
                spans.push(Span::styled(
                    format!("{label}:"),
                    Style::default().fg(theme::STATUS_LABEL),
                ));
                spans.push(Span::styled(
                    value.to_string(),
                    Style::default().fg(theme::STATUS_VALUE),
                ));
            }
            _ => spans.push(Span::styled(
                seg.to_string(),
                Style::default().fg(theme::STATUS_DIM),
            )),
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- front-end selection ------------------------------------------------

    // trace:STORY-169 | ai:claude
    #[test]
    fn tui_only_for_interactive_tty_without_no_tui() {
        // The one combination that yields the TUI: interactive command, no
        // --no-tui, and both std streams are TTYs.
        assert_eq!(
            select_front_end(true, false, true, true),
            FrontEndChoice::Tui
        );
    }

    // trace:STORY-169 | ai:claude
    #[test]
    fn everything_else_falls_back_to_headless() {
        // Any single failing condition selects the headless line front-end, so
        // the ~336 piped/byte tests, --no-tui, and non-interactive commands keep
        // today's behavior.
        assert_eq!(
            select_front_end(false, false, true, true),
            FrontEndChoice::Headless,
            "non-interactive command -> headless"
        );
        assert_eq!(
            select_front_end(true, true, true, true),
            FrontEndChoice::Headless,
            "--no-tui -> headless"
        );
        assert_eq!(
            select_front_end(true, false, false, true),
            FrontEndChoice::Headless,
            "piped stdin -> headless"
        );
        assert_eq!(
            select_front_end(true, false, true, false),
            FrontEndChoice::Headless,
            "redirected stdout -> headless"
        );
    }

    // ---- layout math --------------------------------------------------------

    // trace:STORY-169 | ai:claude
    #[test]
    fn layout_stacks_transcript_input_status_and_fills_the_area() {
        let area = Rect::new(0, 0, 80, 24);
        let panes = layout(area);
        // Fixed 3-row input + 3-row status; transcript takes the rest.
        assert_eq!(panes.input.height, 3);
        assert_eq!(panes.status.height, 3);
        assert_eq!(panes.transcript.height, 24 - 3 - 3);
        // Stacked top-to-bottom, contiguous, full width, no overlap.
        assert_eq!(panes.transcript.y, 0);
        assert_eq!(panes.input.y, panes.transcript.bottom());
        assert_eq!(panes.status.y, panes.input.bottom());
        assert_eq!(panes.status.bottom(), 24);
        for pane in [panes.transcript, panes.input, panes.status] {
            assert_eq!(pane.width, 80);
        }
    }

    // trace:STORY-169 | ai:claude
    #[test]
    fn palette_overlay_is_centered_and_fits_inside_the_screen() {
        let area = Rect::new(0, 0, 80, 24);
        let overlay = palette_rect(area);
        assert!(overlay.width <= area.width && overlay.height <= area.height);
        assert!(overlay.x >= area.x && overlay.right() <= area.right());
        assert!(overlay.y >= area.y && overlay.bottom() <= area.bottom());
        // Roughly centered (within a row/col of the geometric center).
        let center_x = area.width / 2;
        let overlay_center_x = overlay.x + overlay.width / 2;
        assert!((overlay_center_x as i32 - center_x as i32).abs() <= 1);
    }

    // ---- transcript scroll model -------------------------------------------

    // trace:STORY-169 | ai:claude
    #[test]
    fn transcript_follows_the_bottom_by_default() {
        let mut pane = TranscriptPane::new();
        for i in 0..20 {
            pane.push_block(&format!("line {i}"));
        }
        // A 5-row viewport in follow mode shows the LAST 5 lines.
        assert_eq!(pane.visible_offset(5), 20 - 5);
    }

    // trace:STORY-169 | ai:claude
    #[test]
    fn scrolling_up_leaves_follow_and_down_to_bottom_re_enters_it() {
        let mut pane = TranscriptPane::new();
        for i in 0..20 {
            pane.push_block(&format!("line {i}"));
        }
        pane.scroll_up(3, 5);
        // Was pinned at offset 15; up 3 -> 12, no longer following.
        assert_eq!(pane.visible_offset(5), 12);
        // New output does NOT yank the view while scrolled up.
        pane.push_block("line 20");
        assert_eq!(pane.visible_offset(5), 12);
        // Scrolling back to the bottom re-enters follow mode.
        pane.scroll_down(100, 5);
        assert_eq!(pane.visible_offset(5), pane.len() - 5);
        pane.push_block("line 21");
        assert_eq!(pane.visible_offset(5), pane.len() - 5, "follow resumed");
    }

    // trace:STORY-169 | ai:claude
    #[test]
    fn push_block_splits_lines_and_drops_a_trailing_blank() {
        let mut pane = TranscriptPane::new();
        pane.push_block("a\nb\nc\n");
        assert_eq!(pane.lines(), &["a", "b", "c"]);
    }

    // ---- status line --------------------------------------------------------

    // trace:STORY-169 | ai:claude
    #[test]
    fn status_line_mirrors_the_breadcrumb_and_mode() {
        let mut status = StatusLine::default();
        status.observe_block("[topic: free will | depth: 2 | branch: main | goal: is it real?]\n");
        status.observe_block("Mode set: debate\n(some note)");
        let rendered = status.render();
        assert!(rendered.contains("topic: free will"));
        assert!(rendered.contains("goal: is it real?"));
        assert!(rendered.contains("mode: debate"));
    }

    // trace:STORY-169 | ai:claude
    #[test]
    fn status_line_has_a_helpful_default() {
        let status = StatusLine::default();
        let rendered = status.render();
        assert!(rendered.contains('/'), "default hints the palette");
    }

    // ---- control parsing routes like the line front-end --------------------

    // trace:STORY-169 | ai:claude
    #[test]
    fn parse_control_routes_every_command_like_the_line_front_end() {
        // The TUI parses the SAME canonical command strings the palette returns
        // (and the user can type) into the SAME AnswerInput variants the line
        // front-end's read_answer_or_end produces — the front-end-agnostic-engine
        // contract: a command routes to one action regardless of front-end.
        assert!(matches!(
            parse_control("/quit", InputContext::Frontier),
            Some(AnswerInput::End)
        ));
        assert!(matches!(
            parse_control("/observe", InputContext::Frontier),
            Some(AnswerInput::Observe)
        ));
        assert!(matches!(
            parse_control("/synopsis", InputContext::Frontier),
            Some(AnswerInput::Synopsis)
        ));
        assert!(matches!(
            parse_control("/back", InputContext::Frontier),
            Some(AnswerInput::Back)
        ));
        assert!(matches!(
            parse_control("/rest", InputContext::Frontier),
            Some(AnswerInput::Rest)
        ));
        assert!(matches!(
            parse_control("/goal free will", InputContext::Frontier),
            Some(AnswerInput::Goal(_))
        ));
        assert!(matches!(
            parse_control("/mode debate", InputContext::Frontier),
            Some(AnswerInput::Mode(_))
        ));
        assert!(matches!(
            parse_control("/help how?", InputContext::Frontier),
            Some(AnswerInput::Help(_))
        ));
        assert!(matches!(
            parse_control("/tutor x", InputContext::Frontier),
            Some(AnswerInput::Tutor(_))
        ));
    }

    // trace:STORY-169 | ai:claude
    #[test]
    fn parse_control_honors_the_frontier_review_context_gates() {
        // /add is frontier-only; /forward is review-only — same gates the line
        // front-end applies.
        assert!(matches!(
            parse_control("/add", InputContext::Frontier),
            Some(AnswerInput::Add)
        ));
        assert!(parse_control("/add", InputContext::Review).is_none());
        assert!(matches!(
            parse_control("/forward", InputContext::Review),
            Some(AnswerInput::Forward)
        ));
        assert!(parse_control("/forward", InputContext::Frontier).is_none());
    }

    // trace:STORY-169 | ai:claude
    #[test]
    fn parse_control_leaves_ordinary_answers_alone() {
        // A plain answer is NOT a control, so it falls through to normalize_answer.
        assert!(parse_control("yes", InputContext::Frontier).is_none());
        assert!(parse_control("I think free will is real", InputContext::Frontier).is_none());
    }

    // ---- STORY-171: themed transcript + status spans -----------------------

    // trace:STORY-171 | ai:claude
    #[test]
    fn styled_transcript_line_colors_by_role() {
        // An interrogator line is one cyan span.
        let line = styled_transcript_line("Is your will free?");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].style.fg, Some(theme::INTERROGATOR));

        // The user's echoed answer is green.
        let line = styled_transcript_line("> free will is an illusion");
        assert_eq!(line.spans[0].style.fg, Some(theme::USER));

        // The META voice keeps the bright-blue italic styling.
        let line = styled_transcript_line("META (observer) — a reading:");
        assert_eq!(line.spans[0].style.fg, Some(theme::META));

        // The challenger is magenta.
        let line = styled_transcript_line("Challenger (closing) — objection:");
        assert_eq!(line.spans[0].style.fg, Some(theme::CHALLENGER));
    }

    // trace:STORY-171 | ai:claude
    #[test]
    fn styled_transcript_line_attributes_a_quote_to_the_interrogator() {
        // A quoted span inside the user's answer renders in the interrogator's
        // color; the surrounding answer stays the user color.
        let line = styled_transcript_line(r#"> you said "it is free" but I disagree"#);
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[0].style.fg, Some(theme::USER));
        assert_eq!(line.spans[1].style.fg, Some(theme::INTERROGATOR));
        assert_eq!(line.spans[1].content, r#""it is free""#);
        assert_eq!(line.spans[2].style.fg, Some(theme::USER));
    }

    // trace:BUG-172 | ai:claude
    #[test]
    fn styled_transcript_line_attributes_an_interrogator_quote_to_the_user() {
        // SYMMETRIC complement: a quoted span inside the INTERROGATOR's line
        // renders in the user's color (the interrogator is quoting the user);
        // the surrounding framing stays the interrogator color.
        let line = styled_transcript_line(r#"You said "it is free" — really?"#);
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[0].style.fg, Some(theme::INTERROGATOR));
        assert_eq!(line.spans[1].style.fg, Some(theme::USER));
        assert_eq!(line.spans[1].content, r#""it is free""#);
        assert_eq!(line.spans[2].style.fg, Some(theme::INTERROGATOR));
    }

    // trace:STORY-171 | ai:claude
    #[test]
    fn styled_status_line_colors_label_value_segments() {
        let line = styled_status_line("topic: free will  ·  mode: debate");
        // label + value spans for each segment, plus a dim separator.
        let labels: Vec<_> = line
            .spans
            .iter()
            .filter(|s| s.style.fg == Some(theme::STATUS_LABEL))
            .map(|s| s.content.to_string())
            .collect();
        assert!(labels.iter().any(|l| l.trim_start().starts_with("topic:")));
        assert!(labels.iter().any(|l| l.trim_start().starts_with("mode:")));
        assert!(line
            .spans
            .iter()
            .any(|s| s.style.fg == Some(theme::STATUS_VALUE)));
    }

    // trace:STORY-171 | ai:claude
    #[test]
    fn styled_status_line_default_hint_stays_dim() {
        // The bare default hint has no `label: value`, so it stays dim chrome.
        let line = styled_status_line("quizdom — / for commands");
        assert!(line
            .spans
            .iter()
            .all(|s| s.style.fg == Some(theme::STATUS_DIM)));
    }
}
