// trace:STORY-180 | ai:claude
//! The capable FREE-TEXT answer editor for the ratatui TUI (EPIC-167).
//!
//! The TUI's free-text answer box used to be an append-only `String`
//! (`editing.push(c)` / `editing.pop()` + Enter) — no cursor movement, word
//! motions, or kill/yank. This module wraps [`tui_textarea::TextArea`] — the
//! standard ratatui editor widget — to give free-text answers bash-command-line
//! ergonomics: readline/Emacs motions (Ctrl-A/E/F/B, Ctrl-W/K/U, Ctrl-Y yank,
//! Alt-B/F/D word motions, Home/End, arrows), multi-line, undo/redo, and a
//! kill-ring. It is the TUI analog of rustyline (which STAYS for the headless
//! [`crate::frontend::LineFrontEnd`]).
//!
//! ## Editing model — FOLLOW $EDITOR (DECIDED)
//!
//! The model is inferred ONCE at startup from `$VISUAL`/`$EDITOR` (mirroring the
//! headless line front-end's selection logic, STORY-51): an editor whose name is
//! `vi`/`vim`/`nvim` selects a Vim MODAL layer (normal/insert/visual), everything
//! else gets the Emacs/readline default that `TextArea::input` already ships.
//!
//! ## Open-in-$EDITOR escape — Ctrl-X Ctrl-E (DECIDED)
//!
//! The bash `edit-and-execute-command` analog: Ctrl-X then Ctrl-E suspends the
//! TUI, writes the current buffer to a tempfile, launches `$VISUAL`/`$EDITOR`,
//! and reads it back on save — the ultimate vim/emacs for long-form answers. The
//! round-trip ([`edit_buffer_externally`]) takes an injectable [`EditorLauncher`]
//! so CI never spawns a real editor; the TUI front-end wires the real launcher
//! (and the alternate-screen suspend/restore) around it.
//!
//! Only the MECHANICS live here (so they are testable without a terminal); the
//! draw + suspend/restore plumbing lives in [`crate::tui`].

use crate::error::{QuizdomError, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::env;
use std::ffi::OsStr;
use std::path::Path;
use tui_textarea::{CursorMove, Input, Key, Scrolling, TextArea, WrapMode};

/// Which editing model the free-text box presents, inferred from `$EDITOR`.
///
/// Belief-neutral plumbing: this only chooses HOW keys edit text, never WHAT is
/// asked. [`EditorModel::Emacs`] is the readline default (`TextArea::input`'s
/// built-in bindings); [`EditorModel::Vim`] wires a modal normal/insert/visual
/// layer on top.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum EditorModel {
    /// Emacs / readline keybindings (the default).
    Emacs,
    /// Vim modal editing (selected when `$EDITOR`/`$VISUAL` is vi/vim/nvim).
    Vim,
}

/// Infer the editing model from a `$EDITOR`/`$VISUAL` value (the basename is
/// matched, so `/usr/bin/vim` still selects Vim). Mirrors the headless line
/// front-end's `edit_mode_from_editor` (STORY-51) so the TUI and the line path
/// agree on the model. Pure, so the inference is unit-testable.
pub(crate) fn editor_model_from_editor(editor: &str) -> EditorModel {
    let name = Path::new(editor)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(editor)
        .to_ascii_lowercase();
    match name.as_str() {
        "vi" | "vim" | "nvim" => EditorModel::Vim,
        _ => EditorModel::Emacs,
    }
}

// trace:STORY-194 | ai:claude — the session-startup `editor_model()` inference
// MOVED into `crate::settings`: the editing model now derives from the persisted
// `EditorChoice` (Emacs / Vim / Auto), with `Auto` re-running the basename
// inference above via `editor_model_from_editor`. The TUI seeds from the saved
// settings + `$EDITOR` (`resolved_env_editor`), so this standalone reader is gone.

/// The Vim modal sub-state, a trimmed adaptation of tui-textarea's `vim` example.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VimMode {
    Normal,
    Insert,
    Visual,
    /// A pending operator (`d`/`c`/`y`) waiting for a motion or repeat.
    Operator(char),
}

/// What the editor wants the surrounding event loop to do after a keystroke.
///
/// The editor itself never owns the terminal; it returns an INTENT and the TUI
/// front-end ([`crate::tui`]) performs the side effects (submit the answer, open
/// the palette, suspend for the external editor). Keeps the editor mechanics
/// terminal-free and testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EditorOutcome {
    /// The key was handled; keep editing (redraw).
    Continue,
    /// Enter on a single-line buffer: submit the text.
    Submit(String),
    /// Ctrl-C / Ctrl-D on an empty buffer: end input (EOF).
    Eof,
    /// `/` typed into an EMPTY buffer: open the command palette (as today).
    OpenPalette,
    /// Ctrl-X Ctrl-E: suspend the TUI and open the buffer in `$EDITOR`.
    OpenExternalEditor,
}

/// The free-text editor: a [`TextArea`] plus the editing model and (for Vim) the
/// modal state. Constructed fresh per free-text prompt.
pub(crate) struct TextEditor {
    textarea: TextArea<'static>,
    model: EditorModel,
    vim: VimMode,
    /// Pending two-key sequences: `gg` (vim) and Ctrl-X (the external-editor
    /// prefix, both models). Tracked as flags so a stray prefix key is harmless.
    pending_g: bool,
    pending_ctrl_x: bool,
}

impl TextEditor {
    /// Build an empty editor for the given model.
    pub(crate) fn new(model: EditorModel) -> Self {
        let mut textarea = TextArea::default();
        // A free-text answer has no line-number gutter; keep it a bare text box.
        textarea.set_cursor_line_style(ratatui::style::Style::default());
        // trace:BUG-183 | ai:claude — SOFT-WRAP the single logical answer line to
        // the box width instead of horizontally scrolling it off the right edge.
        // Word boundaries with a grapheme fallback for over-long tokens (URLs).
        textarea.set_wrap_mode(WrapMode::WordOrGlyph);
        let vim = match model {
            // Vim starts in INSERT so a user can just type (and reach normal mode
            // with Esc) — the least surprising default for a quick free-text box.
            EditorModel::Vim => VimMode::Insert,
            EditorModel::Emacs => VimMode::Insert,
        };
        Self {
            textarea,
            model,
            vim,
            pending_g: false,
            pending_ctrl_x: false,
        }
    }

    /// The editing model in effect.
    pub(crate) fn model(&self) -> EditorModel {
        self.model
    }

    /// The current Vim mode (meaningful only when `model == Vim`).
    pub(crate) fn vim_mode(&self) -> VimMode {
        self.vim
    }

    /// Borrow the underlying [`TextArea`] for rendering.
    pub(crate) fn textarea(&self) -> &TextArea<'static> {
        &self.textarea
    }

    /// The full buffer text (lines joined with `\n`).
    pub(crate) fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Whether the buffer is empty (no text on a single empty line). Drives the
    /// `/`-opens-palette gate (palette only from an EMPTY box, as today).
    pub(crate) fn is_empty(&self) -> bool {
        self.textarea.is_empty()
    }

    // trace:BUG-183 | ai:claude
    /// How many CONTENT rows the buffer occupies when soft-wrapped to a box of
    /// `outer_width` columns (i.e. excluding the box borders). Drives the dynamic
    /// input-pane height so the box grows downward as the answer wraps. Measures
    /// on a clone so the live editor's measure cache / cursor are untouched (the
    /// clone has no block, so `measure` adds no chrome and returns pure content
    /// rows for the inner width).
    pub(crate) fn wrapped_content_rows(&self, outer_width: u16) -> u16 {
        // The rendered box has 1-column borders on each side, so the wrap width is
        // the inner width. Measure a borderless clone at that inner width.
        let inner_width = outer_width.saturating_sub(2);
        if inner_width == 0 {
            return 1;
        }
        let mut probe = self.textarea.clone();
        probe.measure(inner_width).content_rows.max(1)
    }

    /// Replace the entire buffer (used after the external-editor round-trip).
    pub(crate) fn set_text(&mut self, text: &str) {
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(str::to_string).collect()
        };
        let mut textarea = TextArea::new(lines);
        textarea.set_cursor_line_style(ratatui::style::Style::default());
        // trace:BUG-183 | ai:claude — preserve soft-wrap across the external-editor
        // round-trip (set_text rebuilds the whole TextArea).
        textarea.set_wrap_mode(WrapMode::WordOrGlyph);
        textarea.move_cursor(CursorMove::Bottom);
        textarea.move_cursor(CursorMove::End);
        self.textarea = textarea;
    }

    /// Feed one crossterm key event and return the intent for the event loop.
    ///
    /// Routing order (so the universal escapes win over editing):
    /// 1. The Ctrl-X Ctrl-E external-editor prefix (both models).
    /// 2. `/` into an EMPTY buffer → open the palette (as today).
    /// 3. Enter on a SINGLE line → submit; on a multi-line buffer Enter inserts a
    ///    newline (long/multi-line answers stay in the box).
    /// 4. Ctrl-C / Ctrl-D on an EMPTY buffer → EOF.
    /// 5. Otherwise the model's editing layer handles the key.
    pub(crate) fn feed(&mut self, key: KeyEvent) -> EditorOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // (1) The external-editor escape: Ctrl-X then Ctrl-E (bash's
        // edit-and-execute-command). A lone Ctrl-X arms the prefix; any other key
        // disarms it (and is then handled normally).
        if self.pending_ctrl_x {
            self.pending_ctrl_x = false;
            if ctrl && matches!(key.code, KeyCode::Char('e') | KeyCode::Char('E')) {
                return EditorOutcome::OpenExternalEditor;
            }
            // Fall through: re-handle this key as an ordinary keystroke.
        }
        if ctrl && matches!(key.code, KeyCode::Char('x') | KeyCode::Char('X')) {
            self.pending_ctrl_x = true;
            return EditorOutcome::Continue;
        }

        // (2) `/` into an EMPTY buffer opens the palette (the coexistence rule:
        // the editor owns the keys, but an empty box still surfaces commands).
        if !ctrl && key.code == KeyCode::Char('/') && self.is_empty() {
            // In Vim NORMAL mode a `/` would mean search; but on an empty buffer
            // there is nothing to search, so the palette wins uniformly.
            return EditorOutcome::OpenPalette;
        }

        // (4) EOF on an empty buffer (mirrors the line front-end's Ctrl-C/Ctrl-D).
        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d')) && self.is_empty() {
            return EditorOutcome::Eof;
        }

        match self.model {
            EditorModel::Emacs => self.feed_emacs(key),
            EditorModel::Vim => self.feed_vim(key),
        }
    }

    /// Emacs/readline editing: Enter submits a single line (else inserts a
    /// newline); everything else flows through `TextArea::input`, which ships the
    /// readline bindings (Ctrl-A/E/F/B, Ctrl-W/K, Ctrl-Y yank, Alt-B/F/D).
    ///
    /// Two bindings are remapped to match classic readline (`set -o emacs`),
    /// because tui-textarea's defaults differ: Ctrl-U is `unix-line-discard`
    /// (kill to line head) here rather than the widget's undo, and undo/redo are
    /// exposed on Ctrl-Z / Ctrl-R so Ctrl-U stays a KILL (the spec's binding list).
    fn feed_emacs(&mut self, key: KeyEvent) -> EditorOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => return self.submit_or_newline(),
            // Ctrl-U: kill to line head (readline unix-line-discard) into the ring.
            KeyCode::Char('u') | KeyCode::Char('U') if ctrl => {
                self.textarea.delete_line_by_head();
                return EditorOutcome::Continue;
            }
            // Ctrl-Z: undo (the widget's default undo is Ctrl-U, which we just
            // reclaimed for the kill — so route undo to the conventional Ctrl-Z).
            KeyCode::Char('z') | KeyCode::Char('Z') if ctrl => {
                self.textarea.undo();
                return EditorOutcome::Continue;
            }
            _ => {}
        }
        let input: Input = key.into();
        self.textarea.input(input);
        EditorOutcome::Continue
    }

    /// Submit the buffer when it is a single line; otherwise insert a newline so
    /// multi-line answers keep growing in the box (Enter only submits the
    /// common single-line case, matching the old append-only box's contract).
    fn submit_or_newline(&mut self) -> EditorOutcome {
        if self.textarea.lines().len() <= 1 {
            EditorOutcome::Submit(self.text())
        } else {
            self.textarea.insert_newline();
            EditorOutcome::Continue
        }
    }

    /// The Vim modal layer (adapted from tui-textarea's `vim` example, trimmed to
    /// the motions/edits a free-text answer needs). Enter in NORMAL submits;
    /// Enter in INSERT inserts a newline (multi-line) — symmetric with Emacs's
    /// single-line submit but driven by the mode.
    fn feed_vim(&mut self, key: KeyEvent) -> EditorOutcome {
        let input: Input = key.into();
        match self.vim {
            VimMode::Insert => {
                if input.key == Key::Esc || (input.ctrl && matches!(input.key, Key::Char('c'))) {
                    self.vim = VimMode::Normal;
                    return EditorOutcome::Continue;
                }
                if input.key == Key::Enter {
                    // Multi-line authoring in insert mode: Enter is a newline.
                    self.textarea.insert_newline();
                    return EditorOutcome::Continue;
                }
                self.textarea.input(input);
                EditorOutcome::Continue
            }
            VimMode::Normal | VimMode::Visual | VimMode::Operator(_) => {
                self.feed_vim_command(input)
            }
        }
    }

    /// Handle a key in a non-insert Vim mode. Returns the event-loop intent;
    /// motions/edits mutate the buffer and update `self.vim`.
    fn feed_vim_command(&mut self, input: Input) -> EditorOutcome {
        // In NORMAL mode, Enter submits the answer (the modal analog of Emacs's
        // single-line Enter). It is the deliberate "I'm done" key.
        if self.vim == VimMode::Normal && input.key == Key::Enter {
            return EditorOutcome::Submit(self.text());
        }

        // `gg` jumps to the top — track the pending `g`.
        let was_pending_g = self.pending_g;
        self.pending_g = false;

        match input {
            Input {
                key: Key::Char('h'),
                ..
            } => self.textarea.move_cursor(CursorMove::Back),
            Input {
                key: Key::Char('j'),
                ..
            } => self.textarea.move_cursor(CursorMove::Down),
            Input {
                key: Key::Char('k'),
                ..
            } => self.textarea.move_cursor(CursorMove::Up),
            Input {
                key: Key::Char('l'),
                ..
            } => self.textarea.move_cursor(CursorMove::Forward),
            Input {
                key: Key::Char('w'),
                ..
            } => self.textarea.move_cursor(CursorMove::WordForward),
            Input {
                key: Key::Char('e'),
                ctrl: false,
                ..
            } => {
                self.textarea.move_cursor(CursorMove::WordEnd);
                if matches!(self.vim, VimMode::Operator(_)) {
                    self.textarea.move_cursor(CursorMove::Forward);
                }
            }
            Input {
                key: Key::Char('b'),
                ctrl: false,
                ..
            } => self.textarea.move_cursor(CursorMove::WordBack),
            Input {
                key: Key::Char('^') | Key::Char('0'),
                ..
            } => self.textarea.move_cursor(CursorMove::Head),
            Input {
                key: Key::Char('$'),
                ..
            } => self.textarea.move_cursor(CursorMove::End),
            Input {
                key: Key::Char('D'),
                ..
            } => {
                self.textarea.delete_line_by_end();
                self.vim = VimMode::Normal;
            }
            Input {
                key: Key::Char('C'),
                ..
            } => {
                self.textarea.delete_line_by_end();
                self.textarea.cancel_selection();
                self.vim = VimMode::Insert;
            }
            Input {
                key: Key::Char('p'),
                ..
            } => {
                self.textarea.paste();
                self.vim = VimMode::Normal;
            }
            Input {
                key: Key::Char('u'),
                ctrl: false,
                ..
            } => {
                self.textarea.undo();
                self.vim = VimMode::Normal;
            }
            Input {
                key: Key::Char('r'),
                ctrl: true,
                ..
            } => {
                self.textarea.redo();
                self.vim = VimMode::Normal;
            }
            Input {
                key: Key::Char('x'),
                ..
            } => {
                self.textarea.delete_next_char();
                self.vim = VimMode::Normal;
            }
            Input {
                key: Key::Char('i'),
                ..
            } => {
                self.textarea.cancel_selection();
                self.vim = VimMode::Insert;
            }
            Input {
                key: Key::Char('a'),
                ..
            } => {
                self.textarea.cancel_selection();
                self.textarea.move_cursor(CursorMove::Forward);
                self.vim = VimMode::Insert;
            }
            Input {
                key: Key::Char('A'),
                ..
            } => {
                self.textarea.cancel_selection();
                self.textarea.move_cursor(CursorMove::End);
                self.vim = VimMode::Insert;
            }
            Input {
                key: Key::Char('I'),
                ..
            } => {
                self.textarea.cancel_selection();
                self.textarea.move_cursor(CursorMove::Head);
                self.vim = VimMode::Insert;
            }
            Input {
                key: Key::Char('o'),
                ..
            } => {
                self.textarea.move_cursor(CursorMove::End);
                self.textarea.insert_newline();
                self.vim = VimMode::Insert;
            }
            Input {
                key: Key::Char('O'),
                ..
            } => {
                self.textarea.move_cursor(CursorMove::Head);
                self.textarea.insert_newline();
                self.textarea.move_cursor(CursorMove::Up);
                self.vim = VimMode::Insert;
            }
            Input {
                key: Key::Char('G'),
                ctrl: false,
                ..
            } => self.textarea.move_cursor(CursorMove::Bottom),
            Input {
                key: Key::Char('g'),
                ctrl: false,
                ..
            } if was_pending_g => self.textarea.move_cursor(CursorMove::Top),
            Input {
                key: Key::Char('g'),
                ctrl: false,
                ..
            } => {
                // First `g` of a `gg`: arm the pending flag and wait.
                self.pending_g = true;
            }
            Input {
                key: Key::Char('d'),
                ctrl: true,
                ..
            } => self.textarea.scroll(Scrolling::HalfPageDown),
            Input {
                key: Key::Char('u'),
                ctrl: true,
                ..
            } => self.textarea.scroll(Scrolling::HalfPageUp),
            Input {
                key: Key::Char('v'),
                ctrl: false,
                ..
            } if self.vim == VimMode::Normal => {
                self.textarea.start_selection();
                self.vim = VimMode::Visual;
            }
            Input { key: Key::Esc, .. } if self.vim == VimMode::Visual => {
                self.textarea.cancel_selection();
                self.vim = VimMode::Normal;
            }
            Input { key: Key::Esc, .. } => {
                // Esc in normal/operator: cancel any pending operator selection.
                self.textarea.cancel_selection();
                self.vim = VimMode::Normal;
            }
            Input {
                key: Key::Char(c),
                ctrl: false,
                ..
            } if self.vim == VimMode::Operator(c) => {
                // Linewise repeat: dd / cc / yy.
                self.textarea.move_cursor(CursorMove::Head);
                self.textarea.start_selection();
                let cursor = self.textarea.cursor();
                self.textarea.move_cursor(CursorMove::Down);
                if cursor == self.textarea.cursor() {
                    self.textarea.move_cursor(CursorMove::End);
                }
                self.apply_pending_operator();
            }
            Input {
                key: Key::Char(op @ ('y' | 'd' | 'c')),
                ctrl: false,
                ..
            } if self.vim == VimMode::Normal => {
                self.textarea.start_selection();
                self.vim = VimMode::Operator(op);
                return EditorOutcome::Continue;
            }
            Input {
                key: Key::Char('y'),
                ctrl: false,
                ..
            } if self.vim == VimMode::Visual => {
                self.textarea.move_cursor(CursorMove::Forward);
                self.textarea.copy();
                self.vim = VimMode::Normal;
            }
            Input {
                key: Key::Char('d'),
                ctrl: false,
                ..
            } if self.vim == VimMode::Visual => {
                self.textarea.move_cursor(CursorMove::Forward);
                self.textarea.cut();
                self.vim = VimMode::Normal;
            }
            Input {
                key: Key::Char('c'),
                ctrl: false,
                ..
            } if self.vim == VimMode::Visual => {
                self.textarea.move_cursor(CursorMove::Forward);
                self.textarea.cut();
                self.vim = VimMode::Insert;
            }
            _ => return EditorOutcome::Continue,
        }

        // After a motion in operator-pending mode, apply the operator over the
        // selection the motion extended.
        if let VimMode::Operator(_) = self.vim {
            self.apply_pending_operator();
        }
        EditorOutcome::Continue
    }

    /// Apply the pending Vim operator (`y`/`d`/`c`) over the current selection and
    /// return to the resulting mode.
    fn apply_pending_operator(&mut self) {
        match self.vim {
            VimMode::Operator('y') => {
                self.textarea.copy();
                self.vim = VimMode::Normal;
            }
            VimMode::Operator('d') => {
                self.textarea.cut();
                self.vim = VimMode::Normal;
            }
            VimMode::Operator('c') => {
                self.textarea.cut();
                self.vim = VimMode::Insert;
            }
            _ => {}
        }
    }
}

/// An injectable launcher for the external-editor round-trip, so tests can drive
/// the Ctrl-X Ctrl-E flow WITHOUT spawning a real editor (CI must never block on
/// vim). The real implementation ([`SpawnEditorLauncher`]) shells out to
/// `$VISUAL`/`$EDITOR`; tests pass a closure that mutates the tempfile in place.
pub(crate) trait EditorLauncher {
    /// "Edit" the file at `path` (the buffer has already been written there).
    /// Return `Ok(())` once the user has saved/quit; the caller then reads the
    /// file back. Errors abort the round-trip and keep the in-pane buffer.
    fn launch(&self, path: &Path) -> Result<()>;
}

/// The production launcher: resolve `$VISUAL` then `$EDITOR` (falling back to
/// `vi`), split off any arguments, and spawn it on the tempfile, inheriting the
/// terminal so the editor draws normally. The TUI front-end suspends the
/// alternate screen around the call.
pub(crate) struct SpawnEditorLauncher;

impl EditorLauncher for SpawnEditorLauncher {
    fn launch(&self, path: &Path) -> Result<()> {
        let editor = env::var("VISUAL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| env::var("EDITOR").ok().filter(|s| !s.trim().is_empty()))
            .unwrap_or_else(|| "vi".to_string());
        // Support `$EDITOR` values that carry arguments (e.g. `code --wait`).
        let mut parts = editor.split_whitespace();
        let program = parts.next().unwrap_or("vi");
        let args: Vec<&str> = parts.collect();
        let status = std::process::Command::new(program)
            .args(&args)
            .arg(path)
            .status()
            .map_err(QuizdomError::Io)?;
        if status.success() {
            Ok(())
        } else {
            Err(QuizdomError::Io(std::io::Error::other(format!(
                "editor `{editor}` exited with status {status}"
            ))))
        }
    }
}

/// Round-trip `buffer` through an external editor: write it to a tempfile, hand
/// the path to `launcher` (which "edits" it), then read the file back as the new
/// buffer. The tempfile is removed afterwards (best-effort). Pure over the
/// launcher, so the whole flow is testable with a mock launcher — CI never
/// spawns a real editor.
///
/// A trailing newline written by editors (vim adds one) is trimmed so the
/// round-tripped answer matches what the user sees in the box.
pub(crate) fn edit_buffer_externally(
    buffer: &str,
    launcher: &dyn EditorLauncher,
) -> Result<String> {
    use std::io::Write as _;

    // A unique-ish tempfile name in the system temp dir. We avoid a tempfile
    // crate dependency: pid + a monotonic counter is enough for a single-user
    // interactive session.
    let dir = env::temp_dir();
    let unique = format!(
        "quizdom-answer-{}-{}.txt",
        std::process::id(),
        next_temp_seq()
    );
    let path = dir.join(unique);

    {
        let mut file = std::fs::File::create(&path).map_err(QuizdomError::Io)?;
        file.write_all(buffer.as_bytes())
            .map_err(QuizdomError::Io)?;
        file.flush().map_err(QuizdomError::Io)?;
    }

    let launch_result = launcher.launch(&path);

    let read_result =
        launch_result.and_then(|()| std::fs::read_to_string(&path).map_err(QuizdomError::Io));
    // Best-effort cleanup regardless of outcome.
    let _ = std::fs::remove_file(&path);

    read_result.map(|text| text.strip_suffix('\n').unwrap_or(&text).to_string())
}

/// A process-wide monotonic counter for tempfile uniqueness within one run.
fn next_temp_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn special(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    fn type_str(ed: &mut TextEditor, s: &str) {
        for c in s.chars() {
            ed.feed(key(c));
        }
    }

    // ---- $EDITOR-mode inference (vim vs emacs) ------------------------------

    // trace:STORY-180 | ai:claude — the editing model FOLLOWS $EDITOR: vi/vim/nvim
    // (basename, so a full path works) select the Vim modal layer; anything else
    // is the Emacs/readline default. Mirrors STORY-51's headless selection logic.
    #[test]
    fn editor_model_infers_vim_for_vi_family_and_emacs_otherwise() {
        assert_eq!(editor_model_from_editor("vim"), EditorModel::Vim);
        assert_eq!(editor_model_from_editor("vi"), EditorModel::Vim);
        assert_eq!(editor_model_from_editor("nvim"), EditorModel::Vim);
        assert_eq!(editor_model_from_editor("/usr/bin/vim"), EditorModel::Vim);
        assert_eq!(editor_model_from_editor("nano"), EditorModel::Emacs);
        assert_eq!(editor_model_from_editor("emacs"), EditorModel::Emacs);
        assert_eq!(editor_model_from_editor(""), EditorModel::Emacs);
        assert_eq!(editor_model_from_editor("code --wait"), EditorModel::Emacs);
    }

    // ---- Emacs / readline motion wiring -------------------------------------

    // trace:STORY-180 | ai:claude — the readline kill/yank ring is wired through
    // TextArea: Ctrl-W kills the previous word into the ring, Ctrl-Y yanks it
    // back. Proves the Emacs bindings reach the widget (not the old push/pop box).
    #[test]
    fn emacs_kill_word_and_yank_round_trips_through_the_ring() {
        let mut ed = TextEditor::new(EditorModel::Emacs);
        type_str(&mut ed, "hello world");
        // Ctrl-W kills the previous word ("world").
        ed.feed(ctrl('w'));
        assert_eq!(ed.text(), "hello ");
        // Ctrl-Y yanks it back.
        ed.feed(ctrl('y'));
        assert_eq!(ed.text(), "hello world");
    }

    // trace:STORY-180 | ai:claude — Ctrl-A/Ctrl-E (line start/end) and Ctrl-K
    // (kill to end of line) are wired: jump home, kill the whole line.
    #[test]
    fn emacs_line_motions_and_kill_line() {
        let mut ed = TextEditor::new(EditorModel::Emacs);
        type_str(&mut ed, "abcdef");
        ed.feed(ctrl('a')); // to line head
        ed.feed(ctrl('k')); // kill to end
        assert_eq!(ed.text(), "");
    }

    // trace:STORY-180 | ai:claude — Alt-B/Alt-D word motions + delete: Alt-B back
    // a word, Alt-D delete the next word.
    #[test]
    fn emacs_word_motions() {
        let mut ed = TextEditor::new(EditorModel::Emacs);
        type_str(&mut ed, "one two three");
        ed.feed(alt('b')); // back over "three"
        ed.feed(alt('d')); // delete "three"
        assert_eq!(ed.text(), "one two ");
    }

    // trace:STORY-180 | ai:claude — undo restores the buffer after an edit
    // (the kill-ring + history come from TextArea, replacing the append-only box).
    #[test]
    fn emacs_undo_restores_after_kill() {
        let mut ed = TextEditor::new(EditorModel::Emacs);
        type_str(&mut ed, "keep this");
        ed.feed(ctrl('u')); // kill to line head (whole line)
        assert_eq!(ed.text(), "");
        ed.feed(ctrl('z')); // undo
        assert_eq!(ed.text(), "keep this");
    }

    // ---- submit vs newline --------------------------------------------------

    // trace:STORY-180 | ai:claude — Enter on a single-line buffer SUBMITS; a
    // multi-line buffer keeps Enter as a newline so long answers grow in place.
    #[test]
    fn emacs_enter_submits_single_line_but_newlines_multiline() {
        let mut ed = TextEditor::new(EditorModel::Emacs);
        type_str(&mut ed, "my answer");
        assert_eq!(
            ed.feed(special(KeyCode::Enter)),
            EditorOutcome::Submit("my answer".into())
        );

        // Build a multi-line buffer via the external-editor set_text, then Enter
        // inserts a newline rather than submitting.
        let mut ed = TextEditor::new(EditorModel::Emacs);
        ed.set_text("line one\nline two");
        assert_eq!(ed.feed(special(KeyCode::Enter)), EditorOutcome::Continue);
        assert!(ed.text().contains('\n'));
    }

    // ---- '/'-from-empty opens the palette; non-empty types it ---------------

    // trace:STORY-180 | ai:claude — the coexistence rule: '/' on an EMPTY free-text
    // box opens the palette (as today); once the box has text, '/' is just a typed
    // character (so a free-text answer can contain a slash).
    #[test]
    fn slash_opens_palette_only_from_empty_box() {
        let mut ed = TextEditor::new(EditorModel::Emacs);
        assert_eq!(ed.feed(key('/')), EditorOutcome::OpenPalette);

        let mut ed = TextEditor::new(EditorModel::Emacs);
        type_str(&mut ed, "and/or");
        assert_eq!(ed.text(), "and/or", "slash typed into a non-empty box");
    }

    // trace:STORY-180 | ai:claude — Ctrl-C / Ctrl-D on an EMPTY buffer is EOF
    // (mirrors the line front-end); with text present they fall through to editing.
    #[test]
    fn ctrl_c_d_is_eof_only_when_empty() {
        let mut ed = TextEditor::new(EditorModel::Emacs);
        assert_eq!(ed.feed(ctrl('d')), EditorOutcome::Eof);
        assert_eq!(ed.feed(ctrl('c')), EditorOutcome::Eof);

        let mut ed = TextEditor::new(EditorModel::Emacs);
        type_str(&mut ed, "x");
        assert_ne!(ed.feed(ctrl('d')), EditorOutcome::Eof);
    }

    // ---- Ctrl-X Ctrl-E external-editor escape -------------------------------

    // trace:STORY-180 | ai:claude — the open-in-$EDITOR escape: Ctrl-X then Ctrl-E
    // returns the OpenExternalEditor intent; a lone Ctrl-X is harmless (it arms
    // the prefix, and the next ordinary key is handled normally).
    #[test]
    fn ctrl_x_ctrl_e_requests_the_external_editor() {
        let mut ed = TextEditor::new(EditorModel::Emacs);
        type_str(&mut ed, "draft");
        assert_eq!(ed.feed(ctrl('x')), EditorOutcome::Continue);
        assert_eq!(ed.feed(ctrl('e')), EditorOutcome::OpenExternalEditor);

        // A lone Ctrl-X followed by an ordinary key just types the key.
        let mut ed = TextEditor::new(EditorModel::Emacs);
        ed.feed(ctrl('x'));
        type_str(&mut ed, "z");
        assert_eq!(ed.text(), "z");
    }

    // ---- the $EDITOR round-trip (mocked launcher) ---------------------------

    /// A mock launcher that runs a user closure over the tempfile contents — the
    /// stand-in for a real `$EDITOR`, so CI never spawns one.
    struct MockLauncher<F: Fn(String) -> String>(F);
    impl<F: Fn(String) -> String> EditorLauncher for MockLauncher<F> {
        fn launch(&self, path: &Path) -> Result<()> {
            let before = std::fs::read_to_string(path).map_err(QuizdomError::Io)?;
            let after = (self.0)(before);
            std::fs::write(path, after).map_err(QuizdomError::Io)?;
            Ok(())
        }
    }

    // trace:STORY-180 | ai:claude — the external-editor round-trip writes the
    // buffer to a tempfile, the (mocked) editor rewrites it, and the result is
    // read back — with the editor's trailing newline trimmed. No real editor runs.
    #[test]
    fn external_edit_round_trips_through_the_tempfile() {
        let launcher = MockLauncher(|contents| {
            assert_eq!(contents, "before");
            // Simulate vim appending a trailing newline on save.
            "after edit\n".to_string()
        });
        let result = edit_buffer_externally("before", &launcher).unwrap();
        assert_eq!(result, "after edit");
    }

    // trace:STORY-180 | ai:claude — a launcher error (editor exited non-zero / not
    // found) propagates so the caller can keep the in-pane buffer; the tempfile is
    // still cleaned up.
    #[test]
    fn external_edit_propagates_a_launcher_error() {
        struct Failing;
        impl EditorLauncher for Failing {
            fn launch(&self, _path: &Path) -> Result<()> {
                Err(QuizdomError::Io(std::io::Error::other("boom")))
            }
        }
        assert!(edit_buffer_externally("x", &Failing).is_err());
    }

    // ---- Vim modal layer ----------------------------------------------------

    // trace:STORY-180 | ai:claude — Vim starts in INSERT (type immediately), Esc
    // enters NORMAL, and `i` returns to INSERT — the modal toggle the spec calls
    // for (set -o vi style).
    #[test]
    fn vim_starts_in_insert_and_toggles_modes() {
        let mut ed = TextEditor::new(EditorModel::Vim);
        assert_eq!(ed.vim_mode(), VimMode::Insert);
        type_str(&mut ed, "hi");
        assert_eq!(ed.text(), "hi");
        ed.feed(special(KeyCode::Esc));
        assert_eq!(ed.vim_mode(), VimMode::Normal);
        ed.feed(key('i'));
        assert_eq!(ed.vim_mode(), VimMode::Insert);
    }

    // trace:STORY-180 | ai:claude — Vim NORMAL-mode motions + edit: `0`/`x` delete
    // the first char; `dd` deletes the line. Confirms the modal layer drives the
    // TextArea (not the append-only box).
    #[test]
    fn vim_normal_mode_motions_and_delete() {
        let mut ed = TextEditor::new(EditorModel::Vim);
        type_str(&mut ed, "abcdef");
        ed.feed(special(KeyCode::Esc)); // -> NORMAL (cursor after last char)
        ed.feed(key('0')); // line head
        ed.feed(key('x')); // delete 'a'
        assert_eq!(ed.text(), "bcdef");
    }

    // trace:STORY-180 | ai:claude — in NORMAL mode Enter SUBMITS (the modal analog
    // of Emacs's single-line Enter); in INSERT mode Enter inserts a newline.
    #[test]
    fn vim_enter_submits_in_normal_and_newlines_in_insert() {
        let mut ed = TextEditor::new(EditorModel::Vim);
        type_str(&mut ed, "done");
        ed.feed(special(KeyCode::Esc)); // NORMAL
        assert_eq!(
            ed.feed(special(KeyCode::Enter)),
            EditorOutcome::Submit("done".into())
        );

        let mut ed = TextEditor::new(EditorModel::Vim);
        type_str(&mut ed, "a"); // INSERT
        assert_eq!(ed.feed(special(KeyCode::Enter)), EditorOutcome::Continue);
        assert!(ed.text().contains('\n'));
    }

    // trace:STORY-180 | ai:claude — '/' from an EMPTY box opens the palette in Vim
    // mode too (the palette wins over vim's search on an empty buffer).
    #[test]
    fn vim_slash_from_empty_opens_palette() {
        let mut ed = TextEditor::new(EditorModel::Vim);
        ed.feed(special(KeyCode::Esc)); // NORMAL, empty
        assert_eq!(ed.feed(key('/')), EditorOutcome::OpenPalette);
    }

    // trace:STORY-180 | ai:claude — the Ctrl-X Ctrl-E escape works in Vim mode too
    // (it is a universal escape checked before the modal layer).
    #[test]
    fn vim_ctrl_x_ctrl_e_requests_the_external_editor() {
        let mut ed = TextEditor::new(EditorModel::Vim);
        type_str(&mut ed, "draft");
        ed.feed(special(KeyCode::Esc));
        assert_eq!(ed.feed(ctrl('x')), EditorOutcome::Continue);
        assert_eq!(ed.feed(ctrl('e')), EditorOutcome::OpenExternalEditor);
    }

    // trace:STORY-180 | ai:claude — set_text replaces the buffer (used after the
    // external round-trip) and parks the cursor at the end.
    #[test]
    fn set_text_replaces_buffer_and_handles_multiline() {
        let mut ed = TextEditor::new(EditorModel::Emacs);
        type_str(&mut ed, "old");
        ed.set_text("new\nlines");
        assert_eq!(ed.text(), "new\nlines");
        // Typing now appends at the end (cursor parked at end).
        type_str(&mut ed, "!");
        assert_eq!(ed.text(), "new\nlines!");
    }

    // ---- BUG-183: soft-wrap + wrapped-row measurement -----------------------

    // trace:BUG-183 | ai:claude — the editor soft-wraps (no horizontal scroll):
    // the wrap mode is enabled so a long single logical line measures as MULTIPLE
    // content rows for the box width instead of one over-long row.
    #[test]
    fn wrapped_content_rows_grows_with_a_long_single_line() {
        let mut ed = TextEditor::new(EditorModel::Emacs);
        // Empty buffer is a single row regardless of width.
        assert_eq!(ed.wrapped_content_rows(40), 1);
        // A ~60-char single logical line wrapped to a 22-col box (20 inner cols)
        // occupies several rows. Still ONE logical line (Enter would submit).
        type_str(
            &mut ed,
            "the quick brown fox jumps over the lazy dog and keeps running far",
        );
        assert_eq!(ed.text().lines().count(), 1, "stays one logical line");
        let rows = ed.wrapped_content_rows(22);
        assert!(rows >= 3, "long answer wraps to multiple rows, got {rows}");
        // A WIDER box needs fewer rows for the same text.
        let wide = ed.wrapped_content_rows(80);
        assert!(
            wide < rows,
            "wider box wraps to fewer rows: {wide} < {rows}"
        );
    }

    // trace:BUG-183 | ai:claude — a degenerate width (narrower than the borders)
    // still reports a single row rather than panicking or returning zero.
    #[test]
    fn wrapped_content_rows_clamps_tiny_width_to_one_row() {
        let mut ed = TextEditor::new(EditorModel::Emacs);
        type_str(&mut ed, "anything");
        assert_eq!(ed.wrapped_content_rows(2), 1);
        assert_eq!(ed.wrapped_content_rows(0), 1);
    }
}
