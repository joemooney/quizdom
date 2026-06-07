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
    is_forward_command, is_judge_command, is_observe_command, is_request_goal_command,
    is_resolved_command, is_rest_command, is_score_command, is_synopsis_command,
    is_terminate_command, is_verdict_command, mode_command_text, normalize_answer,
    objection_command_text, tutor_command_text, AnswerInput, InputContext,
};
// trace:STORY-180 | ai:claude — the capable free-text editor (tui-textarea) and
// the open-in-$EDITOR escape.
use crate::editor::{
    edit_buffer_externally, editor_model, EditorLauncher, EditorModel, EditorOutcome,
    SpawnEditorLauncher, TextEditor, VimMode,
};
// trace:STORY-176 | ai:claude — the single keymap registry drives BOTH the key
// dispatcher (here) and the cheat-sheet overlay, so they can never drift.
use crate::keymap::{self, KeyAction};
use crate::model::{Answer, AnswerKind};
// trace:STORY-190 | ai:claude — PaletteContext threads the availability snapshot.
use crate::palette::{command_registry, PaletteContext, PaletteState};
use crate::style::theme;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Terminal;
use std::io::{self, BufRead, Stdout, Write};

// trace:BUG-184 | ai:claude — the TUI is now GENERIC over the ratatui [`Backend`]
// (so a `TestBackend` can drive the model in unit tests). Backend draw/clear
// errors are `B::Error`, not `io::Error`, so funnel them through one mapper into
// a `QuizdomError::Io` (the production `CrosstermBackend::Error` IS `io::Error`;
// the `TestBackend::Error` is `Infallible`, so this never fires under test).
fn map_backend_err<E: core::error::Error>(error: E) -> QuizdomError {
    QuizdomError::Io(io::Error::other(error.to_string()))
}

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

/// The default input-box height: one content row plus the top/bottom border.
pub(crate) const INPUT_MIN_HEIGHT: u16 = 3;

/// Split the terminal area into the transcript / input / status panes with a
/// FIXED 3-row input box (single-line input paths).
///
/// The status bar and the input box are fixed-height (3 rows each: one content
/// row plus the border), and the transcript pane takes the rest — so it grows
/// with the window and never starves the input or status. Pure over `area`.
pub(crate) fn layout(area: Rect) -> TuiLayout {
    layout_with_input(area, INPUT_MIN_HEIGHT)
}

// trace:BUG-183 | ai:claude
/// Split the area with a DYNAMIC input-box height (`input_height`, borders
/// included). The status bar stays fixed at 3 rows; the input box takes
/// `input_height`; the transcript pane takes the rest and shrinks as the input
/// box grows. `input_height` is clamped so the status bar and at least one
/// transcript row always survive — pure over `area` so the geometry is unit
/// testable without a terminal.
pub(crate) fn layout_with_input(area: Rect, input_height: u16) -> TuiLayout {
    // Reserve the status bar (3) plus one transcript row; the input box may take
    // the remaining height but never less than its single-row minimum.
    let max_input = area.height.saturating_sub(INPUT_MIN_HEIGHT + 1);
    let input_height = input_height.clamp(INPUT_MIN_HEIGHT, max_input.max(INPUT_MIN_HEIGHT));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),               // transcript (grows / shrinks)
            Constraint::Length(input_height), // input box (dynamic)
            Constraint::Length(3),            // status bar (1 row + borders)
        ])
        .split(area);
    TuiLayout {
        transcript: chunks[0],
        input: chunks[1],
        status: chunks[2],
    }
}

// trace:BUG-183 | ai:claude
/// Compute the DYNAMIC input-box height (borders included) for a free-text
/// answer of `content_rows` wrapped rows inside a screen `screen_height` tall.
///
/// The box grows with the wrapped content (`content_rows + 2` for the borders),
/// starting from the single-row minimum and clamped to a maximum of ~1/3 of the
/// screen. Beyond the clamp the box scrolls internally (tui-textarea keeps the
/// cursor visible) rather than eating the transcript. Pure — unit tested
/// directly; the live layout feeds it `content_rows` from the wrapped measure.
pub(crate) fn input_pane_height(content_rows: u16, screen_height: u16) -> u16 {
    // Borders add two rows. Clamp the OUTER height to one third of the screen
    // (at least the single-row minimum, so tiny terminals still get a box).
    let max_height = (screen_height / 3).max(INPUT_MIN_HEIGHT);
    let desired = content_rows.saturating_add(2);
    desired.clamp(INPUT_MIN_HEIGHT, max_height)
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
    // trace:STORY-176 | ai:claude — the re-read HIGHLIGHT cursor: a line index the
    // user moves with Ctrl-←/→ to re-read earlier exchanges WITHOUT changing any
    // answer (scroll-to-view only; 'B'/back stays the only way to revise). `None`
    // until the user first navigates; clamped to `[0, len-1]` at the first/last
    // exchange. Moving it scrolls the line into view (leaving follow mode).
    highlight: Option<usize>,
}

impl TranscriptPane {
    pub(crate) fn new() -> Self {
        Self {
            lines: Vec::new(),
            scroll: 0,
            follow: true,
            highlight: None,
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

    // trace:STORY-176 | ai:claude
    /// The current re-read HIGHLIGHT line index, or `None` until the user first
    /// navigates with Ctrl-←/→. Read by the draw call to mark the highlighted row.
    pub(crate) fn highlight(&self) -> Option<usize> {
        self.highlight
    }

    // trace:STORY-176 | ai:claude
    /// The indices of the transcript's EXCHANGE ANCHORS — the non-empty lines a
    /// user re-reads (questions, echoed answers, meta readings). Blank spacer rows
    /// are skipped so Ctrl-←/→ jumps between content, not whitespace. Pure so the
    /// clamp behavior is testable without a terminal.
    fn anchors(&self) -> Vec<usize> {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, _)| index)
            .collect()
    }

    // trace:STORY-176 | ai:claude
    /// Move the re-read highlight to the PREVIOUS exchange anchor (Ctrl-←),
    /// CLAMPED at the first anchor — it never moves before the first exchange.
    /// Scroll-to-view ONLY: it scrolls the highlighted line into view and leaves
    /// follow mode, but changes no answer ('B'/back is the only way to revise).
    /// Returns the new highlight index (or `None` when the transcript is empty).
    pub(crate) fn highlight_prev(&mut self, height: usize) -> Option<usize> {
        let anchors = self.anchors();
        if anchors.is_empty() {
            self.highlight = None;
            return None;
        }
        let next = match self.highlight {
            // First navigation starts from the last anchor (the newest exchange).
            None => *anchors.last().unwrap(),
            Some(current) => {
                // The largest anchor strictly less than the current line, clamped
                // at the first anchor (cannot go before the first exchange).
                anchors
                    .iter()
                    .rev()
                    .find(|&&index| index < current)
                    .copied()
                    .unwrap_or_else(|| *anchors.first().unwrap())
            }
        };
        self.highlight = Some(next);
        self.scroll_into_view(next, height);
        Some(next)
    }

    // trace:STORY-176 | ai:claude
    /// Move the re-read highlight to the NEXT exchange anchor (Ctrl-→), CLAMPED at
    /// the last anchor — it never moves past the last exchange. Scroll-to-view
    /// only, like [`highlight_prev`]. Returns the new highlight index.
    pub(crate) fn highlight_next(&mut self, height: usize) -> Option<usize> {
        let anchors = self.anchors();
        if anchors.is_empty() {
            self.highlight = None;
            return None;
        }
        let next = match self.highlight {
            None => *anchors.last().unwrap(),
            Some(current) => anchors
                .iter()
                .find(|&&index| index > current)
                .copied()
                .unwrap_or_else(|| *anchors.last().unwrap()),
        };
        self.highlight = Some(next);
        self.scroll_into_view(next, height);
        Some(next)
    }

    // trace:STORY-176 | ai:claude
    /// Scroll so `line` is visible in a `height`-row viewport, leaving follow mode
    /// (the user is now reading back). Clamps the top so the viewport never runs
    /// past the buffer ends. Used by the highlight navigation to keep the
    /// re-read line on screen.
    fn scroll_into_view(&mut self, line: usize, height: usize) {
        let height = height.max(1);
        let max_top = self.lines.len().saturating_sub(height);
        self.follow = false;
        if line < self.scroll {
            self.scroll = line;
        } else if line >= self.scroll + height {
            self.scroll = (line + 1).saturating_sub(height);
        }
        self.scroll = self.scroll.min(max_top);
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
    // trace:STORY-174 | ai:claude — the persistent `/score` gauge segment, mirrored
    // from the `[score: …]` line the engine emits when the gauge is ON. `None`
    // until `/score` turns it on; cleared again when `/score` turns it off.
    pub(crate) score: Option<String>,
    // trace:STORY-175 | ai:claude — the open-objection GAVEL segment, mirrored from
    // the `[objection: …]` line the engine emits when a `/objection` pins the
    // exchange. `None` until one is raised; cleared when `/resolved` or `/judge`
    // emits `[objection: clear]`. Belief-neutral chrome: it marks a contested point.
    pub(crate) objection: Option<String>,
    // trace:BUG-184 | ai:claude — the post-submit THINKING flag. Set the instant a
    // free-text / single-key answer is parsed (before control returns to the engine
    // for its blocking multi-second LLM call) so the status bar shows a working
    // indicator instead of the screen appearing frozen on the filled answer box.
    // Cleared on the next `flush_pending`, so the engine's next output (the new
    // question / rebuttal) replaces it.
    pub(crate) thinking: bool,
}

impl StatusLine {
    /// Render the status line as plain text (segments joined by `·`). Pure so the
    /// composition is testable; the draw call wraps it in a styled paragraph.
    pub(crate) fn render(&self) -> String {
        let mut segments: Vec<String> = Vec::new();
        // trace:BUG-184 | ai:claude — the THINKING indicator leads every segment
        // (it is the most salient state: the system is working on a blocking call).
        // A static `thinking…` segment via the status model — no background thread
        // (the alternate screen owns the frame; spinner.rs writes stderr, invisible
        // under the TUI), and it is replaced by the next question on the next flush.
        if self.thinking {
            segments.push("thinking…".to_string());
        }
        if let Some(breadcrumb) = self
            .breadcrumb
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            segments.push(breadcrumb.to_string());
        }
        // trace:STORY-175 | ai:claude — the open-objection GAVEL segment leads the
        // metrics (it is the most salient: the exchange is pinned) when an objection
        // is open. Belief-neutral chrome: it marks a contested point, never a belief.
        if let Some(objection) = self
            .objection
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            segments.push(format!(
                "{} objection: {}",
                crate::style::OBJECTION_GAVEL,
                objection
            ));
        }
        // trace:STORY-174 | ai:claude — the score gauge segment sits beside the
        // mode segment when the gauge is on; it is already a `score: …` pair.
        if let Some(score) = self
            .score
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            segments.push(score.to_string());
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
            // trace:STORY-174 | ai:claude — the engine emits a `[score: …]` line
            // when the gauge is ON (the same `score: …` segment the headless
            // footer shows). Mirror it into the status bar. A `[score: off]` line
            // (emitted when `/score` toggles the gauge off) CLEARS the segment.
            if let Some(inner) = trimmed
                .strip_prefix("[score: ")
                .and_then(|rest| rest.strip_suffix(']'))
            {
                if inner.trim() == "off" {
                    self.score = None;
                } else {
                    self.score = Some(format!("score: {}", inner.trim()));
                }
            }
            // trace:STORY-175 | ai:claude — the engine emits `[objection: <text> (<party>)]`
            // when a `/objection` PINS the exchange, and `[objection: clear]` when it is
            // `/resolved` or `/judge`-d. Mirror it into the gavel segment; a clear drops it.
            if let Some(inner) = trimmed
                .strip_prefix("[objection: ")
                .and_then(|rest| rest.strip_suffix(']'))
            {
                if inner.trim() == "clear" {
                    self.objection = None;
                } else {
                    self.objection = Some(inner.trim().to_string());
                }
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

    // trace:STORY-180 | ai:claude — the open-in-$EDITOR escape (Ctrl-X Ctrl-E)
    /// SUSPEND the TUI: leave the alternate screen and raw mode so an external
    /// `$EDITOR` (vim/emacs) gets a normal cooked terminal. Paired with [`resume`].
    fn suspend(&self) -> Result<()> {
        disable_raw_mode().map_err(QuizdomError::Io)?;
        execute!(io::stdout(), LeaveAlternateScreen).map_err(QuizdomError::Io)?;
        Ok(())
    }

    // trace:STORY-180 | ai:claude
    /// RESUME the TUI after the external editor exits: re-enter the alternate
    /// screen + raw mode so the session redraws cleanly where it left off.
    fn resume(&self) -> Result<()> {
        enable_raw_mode().map_err(QuizdomError::Io)?;
        execute!(io::stdout(), EnterAlternateScreen).map_err(QuizdomError::Io)?;
        Ok(())
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
pub(crate) struct TuiFrontEnd<R: BufRead, B: Backend = CrosstermBackend<Stdout>> {
    terminal: Terminal<B>,
    // trace:BUG-184 | ai:claude — the terminal guard is OPTIONAL so the model can be
    // driven over an in-memory `TestBackend` (no alternate screen / raw mode) in
    // unit tests. Production always carries `Some(guard)`; tests carry `None`.
    _guard: Option<TerminalGuard>,
    transcript: TranscriptPane,
    status: StatusLine,
    /// Bytes the engine has written via `out()` but not yet flushed to the pane.
    pending: Vec<u8>,
    /// The fallback line source + sink for the nested headless quick-add.
    author_input: R,
    author_output: Vec<u8>,
    // trace:STORY-180 | ai:claude — the editing model (Emacs/readline vs Vim
    // modal) for the free-text box, inferred ONCE from $EDITOR/$VISUAL at startup.
    editor_model: EditorModel,
    // trace:STORY-180 | ai:claude — the open-in-$EDITOR launcher. Boxed + injectable
    // so the Ctrl-X Ctrl-E round-trip can be driven by a mock in tests (CI never
    // spawns a real editor); production wires [`SpawnEditorLauncher`].
    launcher: Box<dyn EditorLauncher>,
    // trace:STORY-190 | ai:claude — the live availability snapshot for the `/`
    // palette, refreshed at the top of each `read_answer` from the engine-supplied
    // context. The line/closing-ritual prompts leave it at the default (every
    // command enabled) since no answer context applies there.
    palette_ctx: PaletteContext,
}

impl<R: BufRead> TuiFrontEnd<R, CrosstermBackend<Stdout>> {
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
            _guard: Some(guard),
            transcript: TranscriptPane::new(),
            status: StatusLine::default(),
            pending: Vec::new(),
            author_input,
            author_output: Vec::new(),
            // trace:STORY-180 | ai:claude — infer the editing model from $EDITOR once.
            editor_model: editor_model(),
            launcher: Box::new(SpawnEditorLauncher),
            // trace:STORY-190 | ai:claude
            palette_ctx: PaletteContext::default(),
        };
        tui.transcript.push_block(
            "quizdom — interactive session. Type your answer and press Enter. Press / for the \
             command palette, ↑/↓ to scroll the transcript.",
        );
        Ok(tui)
    }
}

// trace:BUG-184 | ai:claude — a test-only constructor over an in-memory ratatui
// `TestBackend`: no alternate screen / raw mode (the guard is `None`), so the
// rendering + status model can be unit-tested without a real terminal.
#[cfg(test)]
impl<R: BufRead> TuiFrontEnd<R, ratatui::backend::TestBackend> {
    fn with_test_backend(author_input: R, width: u16, height: u16) -> Self {
        crate::style::set_enabled(false);
        let backend = ratatui::backend::TestBackend::new(width, height);
        let terminal = Terminal::new(backend).expect("test terminal");
        Self {
            terminal,
            _guard: None,
            transcript: TranscriptPane::new(),
            status: StatusLine::default(),
            pending: Vec::new(),
            author_input,
            author_output: Vec::new(),
            editor_model: EditorModel::Emacs,
            launcher: Box::new(SpawnEditorLauncher),
            // trace:STORY-190 | ai:claude
            palette_ctx: PaletteContext::default(),
        }
    }

    /// The current TestBackend buffer flattened to a single string (cells joined,
    /// rows newline-separated) so a draw's visible content is assertable.
    fn rendered_text(&self) -> String {
        let buffer = self.terminal.backend().buffer();
        let area = *buffer.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }
}

// trace:BUG-184 | ai:claude — the rendering + input loop is GENERIC over the
// ratatui [`Backend`] so the model can be driven over an in-memory `TestBackend`
// in unit tests (the production path uses `CrosstermBackend<Stdout>` built in
// `new`). Only the terminal-owning constructor is backend-specific.
impl<R: BufRead, B: Backend> TuiFrontEnd<R, B> {
    /// Move everything written via `out()` since the last flush into the
    /// transcript pane (and update the status bar from it).
    fn flush_pending(&mut self) {
        // trace:BUG-184 | ai:claude — clear the post-submit THINKING indicator as the
        // next input request begins: by now the engine's blocking call has returned
        // and its output (the new question / rebuttal) is about to render, so the
        // working state is over. Done before the empty-pending early-return so a
        // control command that produced no output still drops the indicator.
        self.status.thinking = false;
        if self.pending.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.status.observe_block(&text);
        self.transcript.push_block(&text);
        self.pending.clear();
    }

    // trace:STORY-191 | ai:claude
    /// Hydrate a RESUMED session's prior conversation into the transcript pane as
    /// the CLEAN STYLED transcript — NOT the `[turn N]/question_ref:` debug replay
    /// the headless front-end emits. Each prior turn is pushed as the same plain
    /// text the LIVE loop emits (the question title, then the `> answer` echo), so
    /// the per-line role coloring + markdown render (STORY-179) apply at draw time
    /// IDENTICALLY to a freshly-asked exchange. A compact `resumed — N turns`
    /// marker tops the backlog. The pane stays in FOLLOW mode (set at construction)
    /// so the newest exchange shows on resume with the full history scrollable
    /// above (STORY-176 scroll/re-read now span the whole hydrated buffer).
    ///
    /// The lines land in the SAME `lines: Vec<String>` the live loop appends to, so
    /// scroll offsets / anchors / the re-read highlight all index the entire
    /// transcript back to turn 1. Only the VISIBLE window is markdown-parsed per
    /// frame (the draw call skips to the scroll offset and bounds the row count),
    /// so a 150+ turn backlog stays smooth on every keystroke redraw.
    fn hydrate_transcript(&mut self, turns: &[(String, String)]) {
        if turns.is_empty() {
            return;
        }
        // A compact backlog marker (the debug "Replaying previous session path…"
        // dump is intentionally NOT shown in the TUI).
        self.transcript
            .push_block(&format!("resumed — {} turns", turns.len()));
        for (question_text, raw_answer) in turns {
            // Blank spacer + the question title mirror `render_question` (which
            // emits a leading newline before the title); the `> answer` echo
            // mirrors the live single-key / free-text answer echo. Role coloring
            // + markdown are applied at draw via `styled_transcript_line`.
            self.transcript.push_block("");
            self.transcript.push_block(question_text);
            self.transcript.push_block(&format!("> {raw_answer}"));
        }
    }

    // trace:BUG-184 | ai:claude
    /// Draw ONE post-submit "thinking" frame before control returns to the engine
    /// for its blocking LLM call: turn ON the status `thinking…` indicator and draw
    /// the panes with an EMPTY (collapsed) input box. The echoed `> answer` is
    /// already in the transcript, so this frame shows the answer landed + the system
    /// working, instead of the screen freezing on the filled answer box. The
    /// indicator is cleared by the next [`flush_pending`], so the engine's next
    /// question / rebuttal replaces it.
    fn show_thinking_frame(&mut self) -> Result<()> {
        self.status.thinking = true;
        self.draw("", None)
    }

    /// Draw the three panes (and, when open, the palette overlay) for the current
    /// state. `editing` is the text in the input box; `palette` is `Some` while
    /// the `/` overlay is open.
    // trace:STORY-190 | ai:claude — the overlay tuple grew a `show_reason` flag so
    // Enter's no-op on a greyed command can surface its reason; `draw_palette` is
    // the thin wrapper the palette loop calls.
    fn draw_palette(
        &mut self,
        editing: &str,
        state: &PaletteState,
        show_detail: bool,
        show_reason: bool,
    ) -> Result<()> {
        self.draw(editing, Some((state, show_detail, show_reason)))
    }

    fn draw(&mut self, editing: &str, palette: Option<(&PaletteState, bool, bool)>) -> Result<()> {
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
                // trace:STORY-176 | ai:claude — the re-read HIGHLIGHT line (moved by
                // Ctrl-←/→) renders on a subtle highlight background so the user can
                // see which earlier exchange they are re-reading.
                let highlight = transcript.highlight();
                // trace:STORY-191 | ai:claude — render only the visible window
                // through the markdown renderer (see `transcript_body`).
                let body = transcript_body(transcript, offset, inner_height, highlight);
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
                if let Some((state, show_detail, show_reason)) = palette {
                    let overlay = palette_rect(frame.area());
                    frame.render_widget(Clear, overlay);
                    // trace:STORY-190 | ai:claude — render with the availability
                    // layer: greyed rows carry their reason, and Enter's no-op note
                    // shows when `show_reason`.
                    let text = crate::palette::render_to_string_with_reason(
                        state,
                        show_detail,
                        show_reason,
                    );
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
            .map_err(map_backend_err)?;
        Ok(())
    }

    // trace:STORY-177 | ai:claude — the backspace/mode-flip semantics live in
    // [`PaletteState`]; this driver just feeds keys and re-renders.
    /// Run the live `/` palette overlay starting from the bare `/` already typed.
    /// Filters as the user types, arrow-navigates, Enter runs the highlighted
    /// command (returning its canonical typed form), `?` toggles per-command
    /// detail, Esc cancels. Redrawn IN PLACE each keystroke. `editing` is the
    /// input-box text to keep showing behind it.
    ///
    /// MATCH MODE switches live on the buffer (STORY-177): with the leading `/`
    /// it PREFIX-matches command names; backspacing the `/` away FLIPS to a
    /// name+description SUBSTRING search WITHOUT closing (the palette stays open
    /// on an empty buffer, showing all). Only Backspace on a truly EMPTY buffer
    /// — i.e. [`PaletteState::pop_filter`] returning `false` — cancels.
    fn run_palette(&mut self, editing: &str) -> Result<Option<String>> {
        // trace:STORY-190 | ai:claude — open the palette over the live availability
        // snapshot so inapplicable commands render greyed; Enter on a greyed
        // command is a NO-OP (surfacing its reason) and never returns it.
        let mut state = PaletteState::new(command_registry(), self.palette_ctx);
        let mut show_detail = false;
        let mut show_reason = false;
        loop {
            self.draw_palette(editing, &state, show_detail, show_reason)?;
            let Event::Key(key) = event::read().map_err(QuizdomError::Io)? else {
                continue;
            };
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
            match key.code {
                KeyCode::Esc => return Ok(None),
                KeyCode::Enter => {
                    // trace:STORY-190 | ai:claude — only an ENABLED command returns;
                    // a disabled (greyed) one surfaces its reason and keeps the menu
                    // up — it never executes / is returned.
                    if let Some(command) = state.selection() {
                        return Ok(Some(command));
                    }
                    if let Some((_, availability)) = state.highlighted_with_availability() {
                        if !availability.is_enabled() {
                            show_reason = true;
                            show_detail = false;
                        }
                    }
                }
                KeyCode::Up => {
                    show_detail = false;
                    show_reason = false;
                    state.move_up();
                }
                KeyCode::Down => {
                    show_detail = false;
                    show_reason = false;
                    state.move_down();
                }
                KeyCode::Char('?') => {
                    show_reason = false;
                    show_detail = !show_detail;
                }
                KeyCode::Backspace => {
                    show_detail = false;
                    show_reason = false;
                    // trace:STORY-177 | ai:claude — `pop_filter` returns false
                    // ONLY on a truly empty buffer; backspacing the leading `/`
                    // succeeds (flips to search) and keeps the overlay open.
                    if !state.pop_filter() {
                        // Backspacing an EMPTY buffer closes the overlay.
                        return Ok(None);
                    }
                }
                KeyCode::Char(c) => {
                    show_detail = false;
                    show_reason = false;
                    state.push_filter(c);
                }
                _ => {}
            }
        }
    }

    // trace:STORY-176 | ai:claude
    /// Open the keyboard CHEAT-SHEET overlay (the `?` key). Renders the cheat-sheet
    /// — GENERATED from the single keymap registry, so it can never drift from the
    /// dispatcher — centered on top of the current screen, and waits for any key
    /// (or Esc) to dismiss it. `editing` is the input-box text kept showing behind
    /// it. Non-destructive: it returns to the same prompt.
    fn show_cheat_sheet(&mut self, editing: &str) -> Result<()> {
        loop {
            self.draw_cheat_sheet(editing)?;
            let Event::Key(key) = event::read().map_err(QuizdomError::Io)? else {
                continue;
            };
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
            // Any key dismisses the cheat-sheet (it is a read-only reference).
            return Ok(());
        }
    }

    // trace:STORY-176 | ai:claude
    /// Draw the cheat-sheet overlay over the current screen: the three panes
    /// behind it (so the context stays visible) plus the centered cheat-sheet box
    /// rendered from [`keymap::render_cheat_sheet`].
    fn draw_cheat_sheet(&mut self, editing: &str) -> Result<()> {
        // First lay down the normal screen, then the overlay on top.
        self.draw(editing, None)?;
        let cheat_text = keymap::render_cheat_sheet();
        self.terminal
            .draw(|frame| {
                let overlay = palette_rect(frame.area());
                frame.render_widget(Clear, overlay);
                let body: Vec<Line> = cheat_text
                    .lines()
                    .map(|line| Line::from(line.to_string()))
                    .collect();
                let widget = Paragraph::new(body)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(theme::border())
                            .title(" keyboard cheat-sheet — any key to close "),
                    )
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, overlay);
            })
            .map_err(map_backend_err)?;
        Ok(())
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
            // trace:STORY-176 | ai:claude — consult the SINGLE keymap registry first
            // for the non-text keystrokes (navigation highlight, transcript scroll,
            // the `?` cheat-sheet). The `?` cheat-sheet only fires when the input box
            // is empty, so typing a literal `?` into an answer/free-text line still
            // works. A dispatched key is handled here and the loop continues; an
            // un-dispatched key falls through to the editing / command path below,
            // so the front-end-agnostic command routing is unchanged.
            let dispatched = keymap::dispatch(key.code, key.modifiers);
            if let Some(action) = dispatched {
                let suppress_cheat_sheet =
                    matches!(action, KeyAction::CheatSheet) && !editing.is_empty();
                if !suppress_cheat_sheet {
                    match action {
                        KeyAction::HighlightPrev => {
                            self.transcript.highlight_prev(viewport);
                        }
                        KeyAction::HighlightNext => {
                            self.transcript.highlight_next(viewport);
                        }
                        KeyAction::ScrollLineUp => self.transcript.scroll_up(1, viewport),
                        KeyAction::ScrollLineDown => self.transcript.scroll_down(1, viewport),
                        KeyAction::ScrollPageUp => {
                            self.transcript.scroll_up(viewport.max(1), viewport)
                        }
                        KeyAction::ScrollPageDown => {
                            self.transcript.scroll_down(viewport.max(1), viewport)
                        }
                        KeyAction::CheatSheet => self.show_cheat_sheet(&editing)?,
                    }
                    continue;
                }
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

    // trace:STORY-180 | ai:claude
    /// Draw the three panes with the FREE-TEXT EDITOR widget filling the input
    /// box (instead of the single-line `> text` paragraph). The editor is a
    /// [`crate::editor::TextEditor`] wrapping a `tui-textarea` widget, so it
    /// renders its own cursor + multi-line content; the box title surfaces the
    /// active editing model (and Vim mode).
    fn draw_editor(&mut self, editor: &TextEditor) -> Result<()> {
        let transcript = &self.transcript;
        let status_text = self.status.render();
        let title = editor_box_title(editor);
        self.terminal
            .draw(|frame| {
                let area = frame.area();
                // trace:BUG-183 | ai:claude — the input box GROWS VERTICALLY as the
                // soft-wrapped answer accumulates rows: measure the wrapped content
                // for the current width, then take min(max≈⅓ screen, wrapped+borders)
                // as the dynamic input height. The transcript pane shrinks to match.
                let content_rows = editor.wrapped_content_rows(area.width);
                let input_height = input_pane_height(content_rows, area.height);
                let panes = layout_with_input(area, input_height);

                // ----- transcript pane (same as draw) -----
                let inner_height = panes.transcript.height.saturating_sub(2) as usize;
                let offset = transcript.visible_offset(inner_height);
                let highlight = transcript.highlight();
                // trace:STORY-191 | ai:claude — render only the visible window
                // through the markdown renderer (see `transcript_body`).
                let body = transcript_body(transcript, offset, inner_height, highlight);
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

                // ----- input box: the tui-textarea editor widget -----
                // The cloned widget keeps the WrapMode set on the live editor, so it
                // soft-wraps to `panes.input` and scrolls internally once the box is
                // clamped at its max height (cursor stays visible).
                let mut textarea = editor.textarea().clone();
                textarea.set_block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(theme::border())
                        .title(title.clone()),
                );
                frame.render_widget(&textarea, panes.input);

                // ----- status bar (same as draw) -----
                let status_widget = Paragraph::new(styled_status_line(&status_text)).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(theme::border())
                        .title(" status "),
                );
                frame.render_widget(status_widget, panes.status);
            })
            .map_err(map_backend_err)?;
        Ok(())
    }

    // trace:STORY-180 | ai:claude
    /// Gather a FREE-TEXT answer through the capable editor (readline/Emacs or
    /// Vim modal, per `$EDITOR`). Returns the submitted text, `None` on EOF, or
    /// the canonical `/`-palette command string when the palette is opened from an
    /// EMPTY box and a command is selected (which the caller routes through the
    /// SAME recognizers as a typed command, the front-end-agnostic contract).
    ///
    /// Ctrl-X Ctrl-E suspends the TUI and opens `$VISUAL`/`$EDITOR` on the buffer,
    /// reading it back on save. The transcript scroll / cheat-sheet keymap is NOT
    /// consulted here: the editor owns every key (a free-text answer may contain
    /// any character), so the editor is a distinct focus from the transcript.
    fn read_free_text(&mut self) -> Result<Option<String>> {
        self.flush_pending();
        let mut editor = TextEditor::new(self.editor_model);
        loop {
            self.draw_editor(&editor)?;
            let Event::Key(key) = event::read().map_err(QuizdomError::Io)? else {
                continue;
            };
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
            match editor.feed(key) {
                EditorOutcome::Continue => {}
                EditorOutcome::Submit(text) => {
                    let line = text.trim().to_string();
                    self.transcript.push_block(&format!("> {line}"));
                    return Ok(Some(line));
                }
                EditorOutcome::Eof => return Ok(None),
                EditorOutcome::OpenPalette => {
                    // The `/`-from-empty palette (as today). A selected command
                    // returns as its canonical typed form and routes identically.
                    if let Some(command) = self.run_palette("")? {
                        self.transcript.push_block(&format!("> {command}"));
                        return Ok(Some(command));
                    }
                    // Cancelled — drop back into the editor (the box is still empty).
                }
                EditorOutcome::OpenExternalEditor => {
                    self.open_external_editor(&mut editor)?;
                }
            }
        }
    }

    // trace:STORY-180 | ai:claude
    /// The Ctrl-X Ctrl-E flow: SUSPEND the TUI (leave the alternate screen via the
    /// [`TerminalGuard`]), round-trip the buffer through `$VISUAL`/`$EDITOR` via
    /// the injectable launcher, then RESUME and force a full redraw. A launcher
    /// error (editor missing / non-zero exit) is non-fatal: the in-pane buffer is
    /// kept and a note is shown in the transcript.
    fn open_external_editor(&mut self, editor: &mut TextEditor) -> Result<()> {
        let buffer = editor.text();
        // trace:BUG-184 | ai:claude — the guard is optional (None under a TestBackend);
        // suspend/resume only the alternate screen when a real guard is present.
        if let Some(guard) = self._guard.as_ref() {
            guard.suspend()?;
        }
        let outcome = edit_buffer_externally(&buffer, self.launcher.as_ref());
        let resume = match self._guard.as_ref() {
            Some(guard) => guard.resume(),
            None => Ok(()),
        };
        // Clear the terminal so the alternate screen redraws fresh after the editor.
        self.terminal.clear().map_err(map_backend_err)?;
        resume?;
        match outcome {
            Ok(text) => editor.set_text(&text),
            Err(error) => {
                self.transcript
                    .push_block(&format!("[editor] could not edit externally: {error}"));
            }
        }
        Ok(())
    }

    // trace:STORY-180 | ai:claude
    /// Gather a YES/NO or MULTIPLE-CHOICE answer with SINGLE-KEY, no-Enter controls
    /// (Y/N for yes-no, digits for choice, plus the shared X/P/B and `/` palette).
    /// The rich free-text editor is NOT used here — `y` means Yes, not a typed
    /// char. Returns the canonical raw token (e.g. `"y"`, `"x"`, a digit, or a
    /// palette command string) for the caller to route through the same
    /// recognizers as the line front-end. `None` on EOF.
    fn read_single_key(
        &mut self,
        kind: &AnswerKind,
        context: InputContext,
    ) -> Result<Option<String>> {
        self.flush_pending();
        loop {
            self.draw("", None)?;
            let viewport = self.viewport_height();
            let Event::Key(key) = event::read().map_err(QuizdomError::Io)? else {
                continue;
            };
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
            {
                return Ok(None);
            }
            // Transcript scroll / re-read highlight stay live on a single-key
            // prompt (read-back is non-destructive). The cheat-sheet `?` fires
            // here too (the box is always "empty" — there is no typed buffer).
            if let Some(action) = keymap::dispatch(key.code, key.modifiers) {
                match action {
                    KeyAction::HighlightPrev => {
                        self.transcript.highlight_prev(viewport);
                    }
                    KeyAction::HighlightNext => {
                        self.transcript.highlight_next(viewport);
                    }
                    KeyAction::ScrollLineUp => self.transcript.scroll_up(1, viewport),
                    KeyAction::ScrollLineDown => self.transcript.scroll_down(1, viewport),
                    KeyAction::ScrollPageUp => self.transcript.scroll_up(viewport.max(1), viewport),
                    KeyAction::ScrollPageDown => {
                        self.transcript.scroll_down(viewport.max(1), viewport)
                    }
                    KeyAction::CheatSheet => self.show_cheat_sheet("")?,
                }
                continue;
            }
            if let Some(token) = single_key_token(key.code, kind, context) {
                if token == "/" {
                    // `/` opens the palette (as on a single-key line front-end).
                    if let Some(command) = self.run_palette("")? {
                        self.transcript.push_block(&format!("> {command}"));
                        return Ok(Some(command));
                    }
                    continue;
                }
                self.transcript.push_block(&format!("> {token}"));
                return Ok(Some(token));
            }
            // Unrecognized key: ignore and keep waiting (no-Enter single-key UI).
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

impl<R: BufRead, B: Backend> FrontEnd for TuiFrontEnd<R, B> {
    fn out(&mut self) -> &mut dyn Write {
        &mut self.pending
    }

    fn read_answer(
        &mut self,
        kind: &AnswerKind,
        context: InputContext,
        palette_ctx: PaletteContext,
    ) -> Result<AnswerInput> {
        // trace:STORY-190 | ai:claude — stash the engine-supplied availability
        // snapshot so `run_palette` (opened from this answer prompt) greys the
        // inapplicable commands for the CURRENT session state.
        self.palette_ctx = palette_ctx;
        // Re-present the question until a recognized answer/control arrives. The
        // engine already rendered the question text through `out()`, so we only
        // gather + parse here. Parsing reuses the SAME recognizers as the line
        // front-end (input.rs), so a typed answer and a palette selection route
        // identically — the acceptance guarantee carried over from STORY-163.
        //
        // trace:STORY-180 | ai:claude — route by ANSWER KIND: a FREE-TEXT question
        // gets the capable in-pane editor (readline/Emacs or Vim modal + the
        // Ctrl-X Ctrl-E $EDITOR escape); YES/NO and MULTIPLE-CHOICE keep the
        // single-key, no-Enter controls (Y/N/X/P/B / digits) — so `y` means Yes,
        // not a typed char. Both paths route their result through the SAME
        // recognizers below.
        loop {
            let raw = match kind {
                AnswerKind::FreeText => self.read_free_text()?,
                AnswerKind::YesNo | AnswerKind::Choice(_) => self.read_single_key(kind, context)?,
            };
            let raw = match raw {
                Some(raw) => raw,
                None => return Ok(AnswerInput::End),
            };
            if let Some(parsed) = parse_control(&raw, context) {
                return Ok(parsed);
            }
            if let Some(normalized) = normalize_answer(kind, &raw) {
                // trace:BUG-184 | ai:claude — an actual Answer has been parsed and is
                // about to return to the engine, which then makes a SYNCHRONOUS
                // multi-second LLM call with NO front-end draw in between. Draw ONE
                // frame NOW so the user sees their answer LAND and the system working:
                // the echoed `> answer` is already in the transcript (pushed by the
                // read_* path), the input box COLLAPSES (drawn empty), and the status
                // bar shows a `thinking…` indicator. Without this the last-drawn frame
                // is the FILLED answer box and the screen appears frozen until the
                // next prompt. Control commands return instantly above, so they never
                // reach here (a brief thinking frame would be harmless anyway, since
                // the engine's next output replaces it via flush_pending).
                self.show_thinking_frame()?;
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

    // trace:STORY-191 | ai:claude
    fn hydrate_resume(&mut self, turns: &[(String, String)]) {
        self.hydrate_transcript(turns);
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

// trace:STORY-180 | ai:claude
/// The box-title for the free-text editor, surfacing the active editing model so
/// the user knows which keymap is live (and, for Vim, the current mode). Pure
/// over the editor state so it is testable without a terminal.
fn editor_box_title(editor: &TextEditor) -> String {
    match editor.model() {
        EditorModel::Emacs => " your answer · emacs ".to_string(),
        EditorModel::Vim => {
            let mode = match editor.vim_mode() {
                VimMode::Normal => "NORMAL",
                VimMode::Insert => "INSERT",
                VimMode::Visual => "VISUAL",
                VimMode::Operator(_) => "OP-PENDING",
            };
            format!(" your answer · vim {mode} ")
        }
    }
}

// trace:STORY-180 | ai:claude
/// Map a single keystroke to its canonical answer/control TOKEN for a YES/NO or
/// MULTIPLE-CHOICE prompt (no Enter). Mirrors the headless line front-end's
/// `read_single_key_answer` EXACTLY so the TUI and the line path agree: `y`/`n`
/// for yes-no, digits for choice, the shared `x`/`p`/`b`, context-gated `a`/`f`,
/// `q`/Esc to quit, `o`/`s` for observe/synopsis, `?` cheat-sheet, and `/` opens
/// the palette (returned as the literal `"/"` for the caller to act on). Pure, so
/// the routing is unit-testable. Returns `None` for an unrecognized key.
fn single_key_token(code: KeyCode, kind: &AnswerKind, context: InputContext) -> Option<String> {
    let token = match code {
        KeyCode::Char('y') | KeyCode::Char('Y') if matches!(kind, AnswerKind::YesNo) => "y",
        KeyCode::Char('n') | KeyCode::Char('N') if matches!(kind, AnswerKind::YesNo) => "n",
        KeyCode::Char('x') | KeyCode::Char('X') => "x",
        KeyCode::Char('o') | KeyCode::Char('O') => "/observe",
        KeyCode::Char('s') | KeyCode::Char('S') => "/synopsis",
        KeyCode::Char('a') | KeyCode::Char('A') if context == InputContext::Frontier => "/add",
        KeyCode::Char('p') | KeyCode::Char('P') => "p",
        KeyCode::Char('b') | KeyCode::Char('B') => "b",
        KeyCode::Char('f') | KeyCode::Char('F') if context == InputContext::Review => "f",
        KeyCode::Char('q') | KeyCode::Char('Q') => "/end",
        KeyCode::Char('/') => "/",
        KeyCode::Char(c) if matches!(kind, AnswerKind::Choice(_)) && c.is_ascii_digit() => {
            return Some(c.to_string());
        }
        KeyCode::Esc => "/end",
        _ => return None,
    };
    Some(token.to_string())
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
    // trace:STORY-174 | ai:claude — `/score` toggles the persistent gauge; mirrors
    // the line front-end recognizer order so the TUI routes it identically.
    if is_score_command(raw) {
        return Some(AnswerInput::Score);
    }
    // trace:STORY-173 | ai:claude — `/request-goal` checked before `/goal` so the
    // on-demand alias routes to the direct-propose path (mirrors the line
    // front-end recognizer order exactly).
    if is_request_goal_command(raw) {
        return Some(AnswerInput::RequestGoal);
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
    // trace:STORY-175 | ai:claude — the court-case `/objection` controls. `/resolved`
    // and `/judge` are exact keywords checked before the objection-text recognizer,
    // mirroring the line front-end recognizer order EXACTLY so the TUI routes them
    // identically (the front-end-agnostic-engine contract).
    if is_resolved_command(raw) {
        return Some(AnswerInput::Resolved);
    }
    if is_judge_command(raw) {
        return Some(AnswerInput::Judge);
    }
    if let Some(text) = objection_command_text(raw) {
        return Some(AnswerInput::Objection(text));
    }
    if context == InputContext::Frontier && is_add_command(raw) {
        return Some(AnswerInput::Add);
    }
    if context == InputContext::Review && is_forward_command(raw) {
        return Some(AnswerInput::Forward);
    }
    None
}

// trace:STORY-179 | ai:claude
// trace:BUG-178  | ai:claude
/// Build a styled ratatui [`Line`] for one transcript row by rendering it as
/// markdown ([`crate::markdown`]): the row's voice ([`theme::classify_line`])
/// supplies the base color, inline `*emph*`/`**strong**`/`` `code` `` and the
/// per-line block constructs (lists, headings, blockquotes) are interpreted,
/// and quoted spans recolor to the role-agnostic quote-yellow (BUG-178). The
/// renderer keeps a 1:1 source-line mapping, so the transcript pane's
/// scroll/highlight indices are unaffected. Pure over the plain text the engine
/// emitted, so the styling is testable without a terminal.
// trace:STORY-191 | ai:claude
/// Build the VISIBLE transcript rows for a `height`-row viewport starting at
/// `offset`, styling ONLY the lines that can appear on screen.
///
/// The window is `height` source lines from `offset`: each source line renders
/// to exactly one `Line` (STORY-179's 1:1 mapping), and a wrapped line only
/// consumes MORE viewport rows, so `height` source lines always fill or overfill
/// the pane — never under-fill it. Bounding the slice here means the markdown
/// renderer runs over ~`height` lines per frame instead of the entire history,
/// keeping a 150+ turn (hydrated) transcript smooth on every keystroke redraw.
/// The `highlight` index (STORY-176 re-read cursor) is matched against the
/// ABSOLUTE line index so the highlight survives the windowing.
fn transcript_body(
    transcript: &TranscriptPane,
    offset: usize,
    height: usize,
    highlight: Option<usize>,
) -> Vec<Line<'static>> {
    transcript
        .lines()
        .iter()
        .enumerate()
        .skip(offset)
        .take(height.max(1))
        .map(|(index, line)| {
            let mut styled = styled_transcript_line(line);
            if Some(index) == highlight {
                styled = styled.style(theme::reread_highlight());
            }
            styled
        })
        .collect()
}

fn styled_transcript_line(text: &str) -> Line<'static> {
    // Render this single source line through the message renderer and take its
    // one produced line (multi-line fenced blocks are not expressible in a lone
    // line; every per-line construct is). `render_lines` of a one-element slice
    // gives exactly one line.
    crate::markdown::render_lines(std::slice::from_ref(&text.to_string()))
        .pop()
        .unwrap_or_else(|| Line::from(text.to_string()))
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

    // trace:BUG-183 | ai:claude — the dynamic input-box height GROWS with wrapped
    // rows (content + borders), starts at the single-row minimum, and clamps to
    // ~1/3 of the screen so a runaway answer never eats the transcript.
    #[test]
    fn input_pane_height_grows_with_wrapped_rows_then_clamps() {
        // One content row → minimum box (1 row + 2 borders).
        assert_eq!(input_pane_height(1, 24), 3);
        // Three wrapped rows → 3 + 2 borders = 5, well under the 1/3 clamp (8).
        assert_eq!(input_pane_height(3, 24), 5);
        // A huge answer clamps to floor(24/3) = 8 (scrolls internally past that).
        assert_eq!(input_pane_height(100, 24), 8);
        // Monotonic: more rows never shrink the box.
        let mut prev = 0;
        for rows in 0..50u16 {
            let h = input_pane_height(rows, 30);
            assert!(h >= prev, "height must be monotonic in rows");
            prev = h;
        }
    }

    // trace:BUG-183 | ai:claude — the layout reflects the dynamic input height:
    // the input box takes the requested rows and the transcript shrinks to match
    // (status bar stays fixed at 3).
    #[test]
    fn layout_with_input_reflects_dynamic_input_height() {
        let area = Rect::new(0, 0, 80, 24);
        let panes = layout_with_input(area, 6);
        assert_eq!(panes.input.height, 6);
        assert_eq!(panes.status.height, 3);
        // Transcript = total - input - status; complementary shrink.
        assert_eq!(panes.transcript.height, 24 - 6 - 3);
        // Still stacked contiguously, full width, no overlap.
        assert_eq!(panes.input.y, panes.transcript.bottom());
        assert_eq!(panes.status.y, panes.input.bottom());
        assert_eq!(panes.status.bottom(), 24);
        // A taller input box leaves a smaller transcript.
        let taller = layout_with_input(area, 10);
        assert!(taller.transcript.height < panes.transcript.height);
    }

    // trace:BUG-183 | ai:claude — the clamp protects the status bar + at least one
    // transcript row even when asked for an absurd input height.
    #[test]
    fn layout_with_input_clamps_to_protect_status_and_transcript() {
        let area = Rect::new(0, 0, 80, 24);
        let panes = layout_with_input(area, 100);
        assert_eq!(panes.status.height, 3, "status bar always survives");
        assert!(panes.transcript.height >= 1, "≥1 transcript row survives");
        assert_eq!(
            panes.transcript.height + panes.input.height + panes.status.height,
            24
        );
        // Never below the single-row minimum either.
        let tiny = layout_with_input(area, 0);
        assert_eq!(tiny.input.height, INPUT_MIN_HEIGHT);
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

    // ---- STORY-176: re-read highlight navigation (clamps at first/last) -----

    // trace:STORY-176 | ai:claude — Ctrl-←/→ move a re-read HIGHLIGHT through the
    // transcript's exchange anchors and CLAMP at the first / last exchange: it can
    // never move before the first or past the last. Non-destructive scroll-to-view.
    #[test]
    fn highlight_navigation_clamps_at_first_and_last_exchange() {
        let mut pane = TranscriptPane::new();
        // Five content lines, each followed by a trailing blank that push_block
        // collapses — so the anchors are the five content rows, with a genuine
        // blank spacer row interleaved to prove spacers are skipped by `anchors`.
        for i in 0..5 {
            pane.push_block(&format!("exchange {i}\n\n"));
        }
        // Lines: "exchange 0", "", "exchange 1", "", ... — anchors at the even rows.
        let anchors: Vec<usize> = (0..5).map(|i| i * 2).collect();
        for &a in &anchors {
            assert!(!pane.lines()[a].trim().is_empty(), "anchor {a} is content");
        }

        // First Ctrl-→ (or Ctrl-←) starts at the NEWEST exchange (the last anchor).
        assert_eq!(pane.highlight_prev(3), Some(*anchors.last().unwrap()));

        // Walk all the way back: it stops at the FIRST anchor and never goes before.
        for _ in 0..20 {
            pane.highlight_prev(3);
        }
        assert_eq!(
            pane.highlight(),
            Some(anchors[0]),
            "clamped at the first exchange"
        );
        // Another Ctrl-← stays put (cannot move before the first exchange).
        pane.highlight_prev(3);
        assert_eq!(pane.highlight(), Some(anchors[0]));

        // Walk all the way forward: it stops at the LAST anchor and never goes past.
        for _ in 0..20 {
            pane.highlight_next(3);
        }
        assert_eq!(
            pane.highlight(),
            Some(*anchors.last().unwrap()),
            "clamped at the last exchange"
        );
        pane.highlight_next(3);
        assert_eq!(pane.highlight(), Some(*anchors.last().unwrap()));
    }

    // trace:STORY-176 | ai:claude — moving the highlight is SCROLL-TO-VIEW only: it
    // scrolls the highlighted line into the viewport (leaving follow mode) but the
    // transcript lines are untouched (non-destructive; 'B'/back is the only revise).
    #[test]
    fn highlight_navigation_scrolls_into_view_without_mutating_lines() {
        let mut pane = TranscriptPane::new();
        for i in 0..30 {
            pane.push_block(&format!("line {i}"));
        }
        let before: Vec<String> = pane.lines().to_vec();
        // From the bottom, walk the highlight up several exchanges in a 5-row view.
        for _ in 0..10 {
            pane.highlight_prev(5);
        }
        let highlighted = pane.highlight().unwrap();
        let top = pane.visible_offset(5);
        // The highlighted line is within the visible window [top, top+5).
        assert!(
            highlighted >= top && highlighted < top + 5,
            "highlight {highlighted} must be visible in [{top}, {})",
            top + 5
        );
        // Lines are unchanged — navigation re-reads, it never revises.
        assert_eq!(pane.lines(), before.as_slice());
    }

    // trace:STORY-176 | ai:claude — an empty transcript has no anchors, so the
    // highlight stays None and navigation is a no-op (no panic on the edge).
    #[test]
    fn highlight_navigation_is_a_no_op_on_an_empty_transcript() {
        let mut pane = TranscriptPane::new();
        assert_eq!(pane.highlight_prev(5), None);
        assert_eq!(pane.highlight_next(5), None);
        assert_eq!(pane.highlight(), None);
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

    // trace:STORY-174 | ai:claude — the status bar mirrors the `[score: …]` gauge
    // line the engine emits when `/score` is on, and CLEARS it on `[score: off]`.
    #[test]
    fn status_line_mirrors_the_score_gauge_and_clears_on_off() {
        let mut status = StatusLine::default();
        status.observe_block("[topic: free will | depth: 1 | branch: main]\n");
        status.observe_block("[score: ~70% of the way to settling X (live)]\n");
        let rendered = status.render();
        assert!(
            rendered.contains("score: ~70% of the way to settling X (live)"),
            "{rendered}"
        );
        // `/score` off emits `[score: off]`, which clears the segment.
        status.observe_block("[score: off]\n");
        assert!(!status.render().contains("score:"), "{}", status.render());
    }

    // trace:STORY-175 | ai:claude — the open-objection GAVEL motif mirrors the
    // engine's `[objection: …]` line into the status bar, and `[objection: clear]`
    // (emitted on /resolved or /judge) drops it.
    #[test]
    fn status_line_mirrors_the_open_objection_and_clears_on_resolve() {
        let mut status = StatusLine::default();
        status.observe_block("[topic: free will | depth: 1 | branch: main]\n");
        status.observe_block("[objection: you never defined free (user)]\n");
        let rendered = status.render();
        assert!(
            rendered.contains(crate::style::OBJECTION_GAVEL),
            "{rendered}"
        );
        assert!(
            rendered.contains("objection: you never defined free (user)"),
            "{rendered}"
        );
        // /resolved or /judge emits `[objection: clear]`, which drops the segment.
        status.observe_block("[objection: clear]\n");
        assert!(
            !status.render().contains("objection:"),
            "{}",
            status.render()
        );
    }

    // trace:STORY-174 | ai:claude — the gauge routes to the same AnswerInput::Score
    // through the TUI parser as the line front-end, the front-end-agnostic contract.
    #[test]
    fn parse_control_routes_score_toggle() {
        assert!(matches!(
            parse_control("/score", InputContext::Frontier),
            Some(AnswerInput::Score)
        ));
        assert!(matches!(
            parse_control("/score", InputContext::Review),
            Some(AnswerInput::Score)
        ));
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
        // trace:STORY-175 | ai:claude — the court-case objection controls route to
        // the same AnswerInput variants as the line front-end's recognizers.
        assert!(matches!(
            parse_control("/objection you never defined free", InputContext::Frontier),
            Some(AnswerInput::Objection(_))
        ));
        assert!(matches!(
            parse_control("/resolved", InputContext::Frontier),
            Some(AnswerInput::Resolved)
        ));
        assert!(matches!(
            parse_control("/judge", InputContext::Frontier),
            Some(AnswerInput::Judge)
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

    // trace:BUG-178 | ai:claude — quote coloring is now role-AGNOSTIC: a quoted
    // span recolors to the single QUOTE yellow regardless of which voice the
    // line belongs to (retiring the BUG-172 opposing-role attribution).
    #[test]
    fn styled_transcript_line_colors_a_user_quote_in_quote_yellow() {
        // A quoted span inside the user's answer recolors to the quote-yellow;
        // the surrounding answer (and the `> ` echo marker) stay the user color.
        let line = styled_transcript_line(r#"> you said "it is free" but I disagree"#);
        let quoted = line
            .spans
            .iter()
            .find(|s| s.content.contains("it is free"))
            .expect("quoted span");
        assert_eq!(quoted.style.fg, Some(theme::QUOTE));
        assert_eq!(quoted.content, r#""it is free""#);
        // The `> ` echo marker and the surrounding answer stay user-green.
        assert!(line
            .spans
            .iter()
            .any(|s| s.content.contains("but I disagree") && s.style.fg == Some(theme::USER)));
        assert_eq!(line.spans[0].content, "> ");
        assert_eq!(line.spans[0].style.fg, Some(theme::USER));
    }

    // trace:BUG-178 | ai:claude
    #[test]
    fn styled_transcript_line_colors_an_interrogator_quote_in_quote_yellow() {
        // A quoted span inside the INTERROGATOR's line ALSO recolors to the
        // quote-yellow (role-agnostic); the surrounding framing stays cyan.
        let line = styled_transcript_line(r#"You said "it is free" — really?"#);
        let quoted = line
            .spans
            .iter()
            .find(|s| s.content.contains("it is free"))
            .expect("quoted span");
        assert_eq!(quoted.style.fg, Some(theme::QUOTE));
        assert_eq!(quoted.content, r#""it is free""#);
        assert!(line
            .spans
            .iter()
            .any(|s| s.content.contains("You said") && s.style.fg == Some(theme::INTERROGATOR)));
    }

    // trace:BUG-178 | ai:claude — the OBSERVED META example now colorizes both
    // single-quoted spans (previously uncovered: META wasn't in the pair, and
    // single quotes weren't detected).
    #[test]
    fn styled_transcript_line_colors_a_meta_single_quote() {
        let line = styled_transcript_line(
            "META — not 'I pronounce my life well lived' but 'I hope that verdict is within reach'",
        );
        let colored: Vec<_> = line
            .spans
            .iter()
            .filter(|s| s.style.fg == Some(theme::QUOTE))
            .collect();
        assert_eq!(
            colored.len(),
            2,
            "both single-quoted spans colorize on a META line"
        );
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

    // ---- STORY-180: free-text editor routing + single-key token mapping -----

    // trace:STORY-180 | ai:claude — YES/NO routes the single keys to the same
    // canonical tokens the headless `read_single_key_answer` produces, so a typed
    // and a TUI single-key answer route identically (front-end-agnostic contract).
    #[test]
    fn single_key_token_maps_yes_no_controls() {
        let k = &AnswerKind::YesNo;
        assert_eq!(
            single_key_token(KeyCode::Char('y'), k, InputContext::Frontier).as_deref(),
            Some("y")
        );
        assert_eq!(
            single_key_token(KeyCode::Char('N'), k, InputContext::Frontier).as_deref(),
            Some("n")
        );
        assert_eq!(
            single_key_token(KeyCode::Char('x'), k, InputContext::Frontier).as_deref(),
            Some("x")
        );
        assert_eq!(
            single_key_token(KeyCode::Char('p'), k, InputContext::Frontier).as_deref(),
            Some("p")
        );
        assert_eq!(
            single_key_token(KeyCode::Char('b'), k, InputContext::Frontier).as_deref(),
            Some("b")
        );
        // `/` returns the bare slash so the loop opens the palette.
        assert_eq!(
            single_key_token(KeyCode::Char('/'), k, InputContext::Frontier).as_deref(),
            Some("/")
        );
        // q / Esc end the session.
        assert_eq!(
            single_key_token(KeyCode::Char('q'), k, InputContext::Frontier).as_deref(),
            Some("/end")
        );
        assert_eq!(
            single_key_token(KeyCode::Esc, k, InputContext::Frontier).as_deref(),
            Some("/end")
        );
    }

    // trace:STORY-180 | ai:claude — MULTIPLE-CHOICE routes DIGIT keys to the option
    // index token; `y`/`n` are NOT yes-no controls here (they fall through, so a
    // stray letter is ignored on a choice prompt).
    #[test]
    fn single_key_token_maps_choice_digits_and_context_gates() {
        let choice = AnswerKind::Choice(vec!["a".into(), "b".into()]);
        assert_eq!(
            single_key_token(KeyCode::Char('2'), &choice, InputContext::Frontier).as_deref(),
            Some("2")
        );
        // 'y' is not a yes-no control on a choice prompt -> unrecognized.
        assert!(single_key_token(KeyCode::Char('y'), &choice, InputContext::Frontier).is_none());
        // `/add` is frontier-only; `/forward` is review-only (same gates as headless).
        assert_eq!(
            single_key_token(KeyCode::Char('a'), &choice, InputContext::Frontier).as_deref(),
            Some("/add")
        );
        assert!(single_key_token(KeyCode::Char('a'), &choice, InputContext::Review).is_none());
        assert_eq!(
            single_key_token(KeyCode::Char('f'), &choice, InputContext::Review).as_deref(),
            Some("f")
        );
        assert!(single_key_token(KeyCode::Char('f'), &choice, InputContext::Frontier).is_none());
    }

    // trace:STORY-180 | ai:claude — the free-text editor box title surfaces the
    // active editing model (and, for Vim, the live mode) so the user can see which
    // keymap is in effect.
    #[test]
    fn editor_box_title_shows_the_active_model_and_vim_mode() {
        let emacs = TextEditor::new(EditorModel::Emacs);
        assert!(editor_box_title(&emacs).contains("emacs"));

        let vim = TextEditor::new(EditorModel::Vim);
        // Vim starts in INSERT.
        assert!(editor_box_title(&vim).contains("vim"));
        assert!(editor_box_title(&vim).contains("INSERT"));
    }

    // ---- BUG-184: post-submit thinking state + redraw ----------------------

    use std::io::BufReader;

    fn test_tui(
        width: u16,
        height: u16,
    ) -> TuiFrontEnd<BufReader<std::io::Empty>, ratatui::backend::TestBackend> {
        TuiFrontEnd::with_test_backend(BufReader::new(std::io::empty()), width, height)
    }

    // trace:BUG-184 | ai:claude — the status model gains a `thinking…` segment that
    // LEADS the bar (most salient) and clears on the next flush.
    #[test]
    fn status_line_shows_thinking_segment_leading_and_clears_on_flush() {
        let mut status = StatusLine::default();
        status.observe_block("[topic: free will | depth: 1 | branch: main]\n");
        status.thinking = true;
        let rendered = status.render();
        assert!(rendered.contains("thinking…"), "{rendered}");
        // It leads the segments (it is the first thing the bar shows).
        assert!(rendered.starts_with("thinking…"), "{rendered}");
        // Turning it off drops the segment but keeps the breadcrumb.
        status.thinking = false;
        let rendered = status.render();
        assert!(!rendered.contains("thinking"), "{rendered}");
        assert!(rendered.contains("topic: free will"), "{rendered}");
    }

    // trace:BUG-184 | ai:claude — `show_thinking_frame` draws ONE frame with the
    // echoed answer already in the transcript, the input box COLLAPSED (empty), and
    // a `thinking…` status — the post-submit state the engine's blocking LLM call
    // would otherwise leave looking frozen. Driven over a TestBackend (no terminal).
    #[test]
    fn show_thinking_frame_draws_thinking_status_with_collapsed_box() {
        let mut tui = test_tui(60, 24);
        // The read_* path pushes the echoed answer before read_answer returns.
        tui.transcript
            .push_block("> free will is an illusion of choice");
        tui.show_thinking_frame().expect("draw thinking frame");

        // The status MODEL reflects thinking.
        assert!(tui.status.thinking, "status model is in the thinking state");
        let frame = tui.rendered_text();
        // The status bar shows the working indicator.
        assert!(
            frame.contains("thinking…"),
            "frame had no thinking status:\n{frame}"
        );
        // The echoed answer is visible in the transcript pane.
        assert!(
            frame.contains("free will is an illusion of choice"),
            "frame missing echoed answer:\n{frame}"
        );

        // The INPUT box is COLLAPSED: the content row under the input border shows
        // only the `>` marker, NOT the just-typed answer (no frozen filled box).
        let buffer = tui.terminal.backend().buffer();
        let input_content_y = 24 - 6 + 1; // transcript Min, input Length(3): border+content
        let mut input_row = String::new();
        for x in 0..60u16 {
            input_row.push_str(buffer[(x, input_content_y)].symbol());
        }
        assert!(
            !input_row.contains("illusion"),
            "input box must be collapsed, not frozen with the answer: {input_row:?}"
        );
        // The collapsed box still shows its `>` marker (just no typed text after it).
        assert!(
            input_row.contains('>'),
            "input box keeps its `>` marker: {input_row:?}"
        );
    }

    // trace:BUG-184 | ai:claude — flush_pending clears the thinking indicator as the
    // next input request begins, so the engine's next question/rebuttal renders
    // normally (the indicator is one-shot, replaced by the new prompt).
    #[test]
    fn flush_pending_clears_thinking_then_renders_next_question() {
        let mut tui = test_tui(60, 24);
        tui.status.thinking = true;
        // The engine writes the next question through out(), then the next read
        // calls flush_pending.
        writeln!(tui.out(), "Is the will free? (next question)").unwrap();
        tui.flush_pending();
        assert!(!tui.status.thinking, "thinking cleared on the next flush");
        assert!(
            tui.transcript
                .lines()
                .iter()
                .any(|l| l.contains("Is the will free?")),
            "the next question rendered into the transcript"
        );
        assert!(!tui.status.render().contains("thinking"));
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

    // ---- STORY-191: full styled scrollable transcript on resume ------------

    // trace:STORY-191 | ai:claude — resume HYDRATES the full prior conversation
    // into the pane as the CLEAN STYLED transcript: a compact `resumed — N turns`
    // marker plus each turn's question + `> answer`, role-colored + markdown-
    // rendered at draw. The `[turn N]/question_ref:` debug replay format never
    // appears in the TUI.
    #[test]
    fn hydrate_resume_lays_the_full_conversation_into_the_styled_transcript() {
        let mut tui = test_tui(80, 24);
        let turns = vec![
            ("Is the will *free*?".to_string(), "yes".to_string()),
            ("What is `causation`?".to_string(), "necessity".to_string()),
            ("Could it be otherwise?".to_string(), "no".to_string()),
        ];
        tui.hydrate_resume(&turns);

        // A compact resumed marker tops the backlog (NOT the debug replay dump).
        let lines = tui.transcript.lines();
        assert!(
            lines.iter().any(|l| l == "resumed — 3 turns"),
            "missing compact resumed marker: {lines:?}"
        );
        // The DEBUG replay format is absent.
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("Replaying previous session path")
                    || l.starts_with("[turn ")
                    || l.starts_with("question_ref:")),
            "debug replay format leaked into the TUI transcript: {lines:?}"
        );
        // Every turn's question + echoed answer is present, in order.
        assert!(lines.iter().any(|l| l == "Is the will *free*?"));
        assert!(lines.iter().any(|l| l == "> yes"));
        assert!(lines.iter().any(|l| l == "What is `causation`?"));
        assert!(lines.iter().any(|l| l == "> necessity"));
        assert!(lines.iter().any(|l| l == "Could it be otherwise?"));
        assert!(lines.iter().any(|l| l == "> no"));

        // The lines render as the STYLED transcript: the question is interrogator-
        // colored with its markdown emphasis interpreted (markers hidden), and the
        // echoed answer is user-colored.
        let q = styled_transcript_line("Is the will *free*?");
        assert_eq!(q.spans[0].style.fg, Some(theme::INTERROGATOR));
        assert!(
            q.spans.iter().all(|s| !s.content.contains('*')),
            "emphasis markers should be hidden: {q:?}"
        );
        let a = styled_transcript_line("> yes");
        assert_eq!(a.spans[0].style.fg, Some(theme::USER));
    }

    // trace:STORY-191 | ai:claude — follow-mode lands at the NEWEST exchange on
    // resume (the freshest turn is in view), with the full backlog scrollable
    // above it.
    #[test]
    fn hydrate_resume_follows_the_newest_exchange() {
        let mut tui = test_tui(80, 24);
        let turns: Vec<(String, String)> = (0..40)
            .map(|i| (format!("Question {i}?"), format!("answer {i}")))
            .collect();
        tui.hydrate_resume(&turns);

        // Still in follow mode: the visible window sits at the BOTTOM, so the
        // newest exchange is on screen and the older history scrolls above.
        let height = 20usize;
        let offset = tui.transcript.visible_offset(height);
        assert_eq!(
            offset,
            tui.transcript.len().saturating_sub(height),
            "follow mode pins to the newest exchange after hydration"
        );
        let frame_top = offset;
        // The newest turn's answer is within the visible window.
        let newest_idx = tui
            .transcript
            .lines()
            .iter()
            .rposition(|l| l == "> answer 39")
            .expect("newest answer present");
        assert!(
            newest_idx >= frame_top,
            "newest exchange is visible on resume"
        );
    }

    // trace:STORY-191 | ai:claude — scroll reaches turn 1: after hydration the
    // user can scroll all the way up to the FIRST question, so the whole history
    // is reachable (not just the recap).
    #[test]
    fn scroll_reaches_turn_one_after_hydration() {
        let mut tui = test_tui(80, 24);
        let turns: Vec<(String, String)> = (0..50)
            .map(|i| (format!("Question {i}?"), format!("answer {i}")))
            .collect();
        tui.hydrate_resume(&turns);

        let height = 18usize;
        // Page up many times; offset must clamp at 0 (the very top of the buffer).
        for _ in 0..200 {
            tui.transcript.scroll_up(height, height);
        }
        assert_eq!(
            tui.transcript.visible_offset(height),
            0,
            "scrolling up reaches the top of the hydrated history"
        );
        // Turn 1's question (Question 0) sits at the top of the buffer, in view.
        let first_idx = tui
            .transcript
            .lines()
            .iter()
            .position(|l| l == "Question 0?")
            .expect("first question present");
        assert!(
            first_idx < height,
            "the first hydrated question is reachable in the top viewport"
        );
    }

    // trace:STORY-191 | ai:claude — PERFORMANCE: only the VISIBLE window is parsed
    // / rendered per frame. `transcript_body` slices to at most `height` source
    // lines from the scroll offset, so a 150+ turn backlog never re-parses the
    // whole history on a keystroke redraw.
    #[test]
    fn transcript_body_renders_only_the_visible_window() {
        let mut pane = TranscriptPane::new();
        // A long (200-line) transcript, far more than any viewport.
        for i in 0..200 {
            pane.push_block(&format!("line {i}"));
        }
        let height = 25usize;
        let offset = pane.visible_offset(height);

        let body = transcript_body(&pane, offset, height, None);
        // Exactly the viewport's worth of lines is parsed/rendered — NOT all 200.
        assert_eq!(
            body.len(),
            height,
            "only the viewport ({height} rows) is rendered, not the full history"
        );

        // Scrolled to the TOP of a 200-line buffer, still only `height` lines are
        // rendered (the regression the windowing fixes: a naive `.skip(offset)`
        // with no bound would render all 200 here).
        let body_top = transcript_body(&pane, 0, height, None);
        assert_eq!(body_top.len(), height);
    }

    // trace:STORY-191 | ai:claude — the STORY-176 re-read highlight survives the
    // windowing: a highlighted ABSOLUTE line index, when inside the visible
    // window, still renders on the highlight background.
    #[test]
    fn transcript_body_keeps_the_reread_highlight_in_the_window() {
        let mut pane = TranscriptPane::new();
        for i in 0..40 {
            pane.push_block(&format!("line {i}"));
        }
        let height = 10usize;
        // Highlight a line and render a window that contains it.
        let highlight = 5usize;
        let body = transcript_body(&pane, 0, height, Some(highlight));
        // The highlighted row carries the re-read style; the others do not.
        let reread = theme::reread_highlight();
        assert_eq!(
            body[highlight].style, reread,
            "the highlighted absolute index renders with the re-read style"
        );
        assert_ne!(body[highlight - 1].style, reread);
    }

    // trace:STORY-191 | ai:claude — an EMPTY resume (no prior turns) hydrates
    // nothing extra: the pane keeps only its intro line, no resumed marker.
    #[test]
    fn hydrate_resume_with_no_turns_is_a_noop() {
        let mut tui = test_tui(80, 24);
        let before = tui.transcript.len();
        tui.hydrate_resume(&[]);
        assert_eq!(tui.transcript.len(), before, "no turns => no hydration");
        assert!(!tui
            .transcript
            .lines()
            .iter()
            .any(|l| l.starts_with("resumed —")));
    }
}
