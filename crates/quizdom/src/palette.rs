// trace:STORY-163 | ai:claude
//! The slash-command PALETTE overlay.
//!
//! Typing `/` as the first character at a prompt opens a self-contained
//! crossterm POPUP listing the available commands, each with a one-line
//! description. Filter-as-you-type narrows the list, the arrow keys navigate,
//! Enter selects/runs the highlighted command, Esc cancels back to the prompt,
//! and `?` on the highlighted command shows its DETAILED help + rationale.
//!
//! The overlay is built directly on crossterm (already a dependency from
//! STORY-51) — NOT a ratatui rewrite. It takes over the terminal for the menu,
//! renders, handles keys, returns the choice, and restores the prompt. On a
//! non-TTY stdin (pipes, redirects, tests) the overlay does not run at all; the
//! typed slash-commands keep working exactly as before (see [`run_palette`]'s
//! TTY gate and the caller in `input.rs`).
//!
//! The module is split so the parts that matter are testable WITHOUT a live
//! terminal:
//!
//! - [`command_registry`] is the single source of truth for the command list
//!   (so `/help`, `/tutor`, `/observe`, `/synopsis`, … all appear in one place
//!   and stay in sync with what the prompt advertises).
//! - [`PaletteState`] holds the pure filter / navigation / selection logic; the
//!   crossterm driver only feeds it key events and reads back what to render.

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{self, IsTerminal, Write};

/// One entry in the slash-command palette.
///
/// `command` is the canonical TYPED form the palette returns when this entry is
/// selected — it is fed straight back into the same command-recognition path
/// the user would hit by typing it, so selecting `/observe` in the palette and
/// typing `/observe` route to the IDENTICAL action. `description` is the
/// one-liner shown in the menu; `detail` is the longer help + rationale shown
/// when `?` is pressed on the highlighted command (what it does, when to use
/// it, why it exists).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct PaletteCommand {
    /// The canonical typed form returned on selection (e.g. `/observe`).
    pub(crate) command: &'static str,
    /// The one-line description shown beside the command in the menu.
    pub(crate) description: &'static str,
    /// The detailed help + rationale shown when `?` is pressed.
    pub(crate) detail: &'static str,
}

/// The single source of truth for the palette's command list.
///
/// Driving the palette (and the per-command `?` help) from one registry keeps
/// `/help`, `/tutor`, `/observe`, `/synopsis`, … in sync with what the prompt
/// advertises — add a command here and it appears in the palette, filters, and
/// the `?` help automatically. Belief-neutral throughout: `/help` is described
/// in terms of the TOOL (process/controls), `/tutor` in terms of sharpening the
/// user's OWN point and surfacing missing nuance — neither description supplies
/// a belief or takes a side.
pub(crate) fn command_registry() -> Vec<PaletteCommand> {
    vec![
        PaletteCommand {
            command: "/observe",
            description: "Belief-neutral reading of the current exchange",
            detail: "Surfaces a belief-neutral, clarify-only reading of the exchange on screen: \
restates the challenge in plainer terms, names the precise tension, diagnoses what was asked \
vs what you answered, and lists the dimensions a precise answer must address. It never supplies \
your answer or takes a side. Use it when a follow-up feels slippery and you want to see the \
exchange more clearly. Non-destructive: returns to the same question.",
        },
        PaletteCommand {
            command: "/synopsis",
            description: "Belief-neutral reading of the whole session so far",
            detail: "The whole-session counterpart to /observe: reads the arc of the session so \
far and reflects it back belief-neutrally — the ground you have covered and where it is thin. \
Use it to get oriented in a long session. Never supplies a belief. Non-destructive: returns to \
the same question.",
        },
        PaletteCommand {
            command: "/help",
            description: "Ask how the tool/dialogue works (process, belief-neutral)",
            detail: "An out-of-band help channel for questions about HOW the tool and the \
dialogue work — the controls, the flow, what a feature does, how to rest your case. It answers \
from the tool's design (TOOL-CONTEXT), never from your belief content, so it is strictly \
belief-neutral. Use it when you are unsure what a control does or how the process works. \
Non-destructive: returns to your question.",
        },
        PaletteCommand {
            command: "/tutor",
            description: "Articulation & nuance coach (sharpens YOUR point; never supplies it)",
            detail: "A more active teaching aid than /observe: it helps you ARTICULATE the point \
you are reaching for and names the NUANCE you may be missing. It reflects your own half-formed \
view back more precisely ('you seem to be getting at X — is that it?'), teaches the relevant \
distinction, and names what you have not yet addressed — WITHOUT telling you what to believe. \
It asks 'is this what you mean?'; it never supplies the belief or takes a side. \
Non-destructive: returns to your question.",
        },
        PaletteCommand {
            command: "/explore",
            description: "Branch deeper into the current topic",
            detail: "Follows the current thread one level deeper instead of answering directly, \
branching the exploration into the question behind the question. Use it when a question opens \
a richer line you want to pursue before committing to an answer.",
        },
        PaletteCommand {
            command: "/add",
            description: "Author and link your own question from here",
            detail: "Authors a new question from the current node and links it into the graph as \
a follow-on, so your own line of inquiry becomes part of the session. Frontier-only. Use it \
when the tool has not asked the question you most want to explore.",
        },
        PaletteCommand {
            command: "/goal",
            description: "State or show the session goal/thesis",
            detail: "States the session GOAL — the question or thesis the exploration is \
orienting toward — phrased belief-neutrally as a question to settle, never a belief to adopt. \
A bare /goal shows the current goal. Once set, the goal orients the next questions and the \
breadcrumb.",
        },
        PaletteCommand {
            command: "/mode",
            description: "Toggle questioning mode (socratic | debate)",
            detail: "Switches the questioner's stance between socratic (neutral Socratic \
questioning) and debate (the questioner steelmans the OPPOSING side's craft). Belief-neutral: \
debate challenges the strength of your case, it never asserts which belief is true. A bare \
/mode shows the current mode.",
        },
        PaletteCommand {
            command: "/rest",
            description: "Rest your case — begin the closing ritual",
            detail: "A phase transition out of the question/answer loop into the CLOSING ritual, \
where the exchange becomes closing statements: your settled position plus the challenger's \
strongest remaining structural objection, ending in a belief-neutral verdict on how \
well-rounded the case is.",
        },
        PaletteCommand {
            command: "/back",
            description: "Step back to revisit the previous answer",
            detail: "Steps back along the answered path to revisit and, if you choose, revise a \
previous answer. Use it to reconsider an earlier turn without losing your place.",
        },
        PaletteCommand {
            command: "/punt",
            description: "Punt this question and move to a different topic",
            detail: "Sets the current question aside and moves to a different topic. Punting is a \
signal the tool records — a question you repeatedly punt is down-weighted. Use it when a \
question does not land for you right now.",
        },
        PaletteCommand {
            command: "/quit",
            description: "End the session",
            detail: "Ends the session, printing the session id and the command to resume it \
later. An empty session is discarded rather than saved.",
        },
    ]
}

/// The outcome of running the palette overlay.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum PaletteOutcome {
    /// The user selected a command; carries its canonical typed form (e.g.
    /// `/observe`), which the caller feeds back into the same
    /// command-recognition path as the typed form.
    Selected(String),
    /// The user cancelled (Esc) back to the prompt — nothing chosen.
    Cancelled,
}

/// The pure state behind the palette overlay: the registry, the current filter
/// string, and which of the filtered commands is highlighted.
///
/// All filter / navigation / selection logic lives here so it can be unit-tested
/// without a terminal; the crossterm driver in [`run_palette`] only translates
/// key events into these method calls and renders [`visible`] / [`filter`] /
/// [`highlighted`].
#[derive(Debug, Clone)]
pub(crate) struct PaletteState {
    commands: Vec<PaletteCommand>,
    filter: String,
    /// Index into the CURRENT filtered view (`visible()`), not into `commands`.
    highlight: usize,
}

impl PaletteState {
    /// Build the palette state from a command registry.
    pub(crate) fn new(commands: Vec<PaletteCommand>) -> Self {
        Self {
            commands,
            filter: String::new(),
            highlight: 0,
        }
    }

    /// The filter string typed so far (without the leading `/`).
    pub(crate) fn filter(&self) -> &str {
        &self.filter
    }

    /// The commands matching the current filter, in registry order.
    ///
    /// Matching is case-insensitive and substring-based against the command's
    /// name (sans leading `/`) and its description, so typing `ob` narrows to
    /// `/observe` and typing `nuance` finds `/tutor` by its description.
    pub(crate) fn visible(&self) -> Vec<PaletteCommand> {
        let needle = self.filter.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return self.commands.clone();
        }
        self.commands
            .iter()
            .filter(|command| command_matches(command, &needle))
            .copied()
            .collect()
    }

    /// The currently highlighted command, or `None` when the filter excludes
    /// everything.
    pub(crate) fn highlighted(&self) -> Option<PaletteCommand> {
        self.visible().get(self.highlight).copied()
    }

    /// The highlighted index into the filtered view (clamped to the view).
    pub(crate) fn highlight_index(&self) -> usize {
        self.highlight
    }

    /// Append a typed character to the filter, resetting the highlight to the
    /// top of the (newly narrowed) list so the selection never points past the
    /// end of the filtered view.
    pub(crate) fn push_filter(&mut self, character: char) {
        self.filter.push(character);
        self.highlight = 0;
    }

    /// Remove the last filter character (Backspace). Returns `false` when the
    /// filter is already empty — the caller treats that as "close the palette"
    /// (backspacing past the `/` cancels the overlay), matching the mental model
    /// that the `/` opened it.
    pub(crate) fn pop_filter(&mut self) -> bool {
        let popped = self.filter.pop().is_some();
        self.highlight = 0;
        popped
    }

    /// Move the highlight DOWN one entry, saturating at the last visible entry.
    pub(crate) fn move_down(&mut self) {
        let len = self.visible().len();
        if len == 0 {
            self.highlight = 0;
            return;
        }
        self.highlight = (self.highlight + 1).min(len - 1);
    }

    /// Move the highlight UP one entry, saturating at the top.
    pub(crate) fn move_up(&mut self) {
        self.highlight = self.highlight.saturating_sub(1);
    }
}

/// Whether a command matches the (already lowercased, non-empty) filter needle —
/// by its name (sans leading `/`) or its description.
fn command_matches(command: &PaletteCommand, needle: &str) -> bool {
    let name = command.command.trim_start_matches('/').to_ascii_lowercase();
    name.contains(needle) || command.description.to_ascii_lowercase().contains(needle)
}

/// Run the slash-command palette overlay and return the user's choice.
///
/// Degrades on a non-TTY stdin: returns `None` so the caller falls back to the
/// typed-command path (a bare `/` line is then handled as ordinary input). On a
/// TTY it takes over the terminal in raw mode, renders the menu to `output`,
/// drives the key loop via [`PaletteState`], restores the prompt, and returns
/// `Some(outcome)`.
pub(crate) fn run_palette(output: &mut impl Write) -> io::Result<Option<PaletteOutcome>> {
    if !io::stdin().is_terminal() {
        return Ok(None);
    }
    enable_raw_mode()?;
    let result = run_palette_in_raw(output);
    let _ = disable_raw_mode();
    result
}

/// Run the palette assuming the terminal is ALREADY in raw mode (and a TTY).
///
/// The single-key answer reader in `input.rs` opens the palette from inside its
/// own raw-mode guard; toggling raw mode again there would drop the outer guard
/// out of raw mode on return, so that path calls this variant and leaves the
/// raw-mode lifetime to its existing guard. Always returns `Some` — the caller
/// has already decided it is on a TTY.
pub(crate) fn run_palette_in_raw(output: &mut impl Write) -> io::Result<Option<PaletteOutcome>> {
    let mut state = PaletteState::new(command_registry());
    drive_palette(&mut state, output).map(Some)
}

/// The render + key loop, factored out of [`run_palette`] so the raw-mode guard
/// in the caller always restores the terminal even on an error path.
fn drive_palette(state: &mut PaletteState, output: &mut impl Write) -> io::Result<PaletteOutcome> {
    let mut show_detail = false;
    loop {
        render_palette(state, show_detail, output)?;
        let event = event::read()?;
        let Event::Key(key) = event else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        // Any navigation / typing dismisses an open detail pane first.
        match key.code {
            KeyCode::Esc => return Ok(PaletteOutcome::Cancelled),
            KeyCode::Enter => {
                if let Some(command) = state.highlighted() {
                    return Ok(PaletteOutcome::Selected(command.command.to_string()));
                }
                // Nothing matches the filter — Enter is a no-op; keep the menu up.
            }
            KeyCode::Up => {
                show_detail = false;
                state.move_up();
            }
            KeyCode::Down => {
                show_detail = false;
                state.move_down();
            }
            KeyCode::Char('?') => {
                // `?` toggles the DETAILED help for the highlighted command.
                show_detail = !show_detail;
            }
            KeyCode::Backspace => {
                show_detail = false;
                if !state.pop_filter() {
                    // Backspacing past the `/` closes the overlay.
                    return Ok(PaletteOutcome::Cancelled);
                }
            }
            KeyCode::Char(character) => {
                show_detail = false;
                state.push_filter(character);
            }
            _ => {}
        }
    }
}

/// Render the palette menu (and, when `show_detail`, the highlighted command's
/// detailed help) to `output`. Pure over the state + flag so the rendered text
/// is testable via [`render_to_string`]; the crossterm driver calls it each
/// frame after clearing.
fn render_palette(
    state: &PaletteState,
    show_detail: bool,
    output: &mut impl Write,
) -> io::Result<()> {
    let text = render_to_string(state, show_detail);
    // In raw mode the cursor does not auto-return; emit explicit CRLFs so each
    // line starts at column zero.
    write!(output, "\r\n{}", text.replace('\n', "\r\n"))?;
    output.flush()
}

/// The palette's rendered text for a given state — split out so the menu layout
/// (header, the filtered rows with a `>` highlight marker, and the optional
/// detail pane) is unit-testable without a terminal.
pub(crate) fn render_to_string(state: &PaletteState, show_detail: bool) -> String {
    let mut out = String::new();
    out.push_str("Slash-command palette");
    if !state.filter().is_empty() {
        out.push_str(&format!(" — filter: /{}", state.filter()));
    }
    out.push('\n');
    let visible = state.visible();
    if visible.is_empty() {
        out.push_str("  (no commands match — Backspace to widen, Esc to cancel)\n");
        return out;
    }
    for (index, command) in visible.iter().enumerate() {
        let marker = if index == state.highlight_index() {
            ">"
        } else {
            " "
        };
        out.push_str(&format!(
            "{marker} {:<10}  {}\n",
            command.command, command.description
        ));
    }
    out.push_str("  [type] filter  [↑/↓] move  [Enter] run  [?] details  [Esc] cancel\n");
    if show_detail {
        if let Some(command) = state.highlighted() {
            out.push_str(&format!("\n{} — {}\n", command.command, command.detail));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> PaletteState {
        PaletteState::new(command_registry())
    }

    // ---- registry -----------------------------------------------------------

    #[test]
    fn registry_includes_help_and_tutor() {
        // trace:STORY-163 | ai:claude — the palette is driven from a single
        // registry, and EPIC-162's /help (STORY-164) and /tutor (STORY-165) must
        // appear in it alongside the existing controls.
        let registry = command_registry();
        let names: Vec<&str> = registry.iter().map(|c| c.command).collect();
        for expected in [
            "/observe",
            "/synopsis",
            "/help",
            "/tutor",
            "/explore",
            "/add",
            "/goal",
            "/mode",
            "/rest",
            "/back",
            "/punt",
            "/quit",
        ] {
            assert!(names.contains(&expected), "registry missing {expected}");
        }
    }

    #[test]
    fn every_command_has_a_description_and_detail() {
        for command in command_registry() {
            assert!(
                !command.description.trim().is_empty(),
                "{} has no description",
                command.command
            );
            assert!(
                !command.detail.trim().is_empty(),
                "{} has no detail",
                command.command
            );
            assert!(
                command.command.starts_with('/'),
                "canonical form is slashed"
            );
        }
    }

    #[test]
    fn help_and_tutor_descriptions_stay_belief_neutral() {
        // trace:STORY-163 | ai:claude — /help is described by TOOL-CONTEXT, not
        // belief content; /tutor sharpens the user's OWN point and surfaces missing
        // nuance but NEVER supplies the belief or takes a side. The registry copy
        // is the contract the palette renders, so guard it here.
        let registry = command_registry();
        let help = registry.iter().find(|c| c.command == "/help").unwrap();
        assert!(help.description.to_lowercase().contains("belief-neutral"));
        assert!(help.detail.to_lowercase().contains("tool-context"));
        assert!(help
            .detail
            .to_lowercase()
            .contains("never from your belief"));

        let tutor = registry.iter().find(|c| c.command == "/tutor").unwrap();
        assert!(tutor.detail.to_lowercase().contains("never"));
        assert!(
            tutor.detail.to_lowercase().contains("supplies")
                || tutor.detail.to_lowercase().contains("supply"),
            "tutor detail must promise it never supplies the belief"
        );
        assert!(tutor.description.to_lowercase().contains("never supplies"));
    }

    // ---- filtering ----------------------------------------------------------

    #[test]
    fn empty_filter_shows_every_command() {
        let state = state();
        assert_eq!(state.visible().len(), command_registry().len());
    }

    #[test]
    fn filter_narrows_by_name_substring() {
        let mut state = state();
        state.push_filter('o');
        state.push_filter('b');
        let visible = state.visible();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].command, "/observe");
    }

    #[test]
    fn filter_is_case_insensitive_and_matches_descriptions() {
        // "nuance" appears only in /tutor's description, not its name.
        let mut state = state();
        for character in "NUANCE".chars() {
            state.push_filter(character);
        }
        let visible = state.visible();
        assert!(visible.iter().any(|c| c.command == "/tutor"));
    }

    #[test]
    fn filter_can_match_nothing() {
        let mut state = state();
        for character in "zzzznope".chars() {
            state.push_filter(character);
        }
        assert!(state.visible().is_empty());
        assert!(state.highlighted().is_none());
    }

    #[test]
    fn backspacing_an_empty_filter_reports_closure() {
        let mut state = state();
        // First a char, then two pops: the second pop is on an empty filter.
        state.push_filter('x');
        assert!(state.pop_filter(), "popping a typed char succeeds");
        assert!(
            !state.pop_filter(),
            "popping an empty filter reports closure (cancel the overlay)"
        );
    }

    // ---- navigation + selection --------------------------------------------

    #[test]
    fn highlight_starts_at_the_top() {
        let state = state();
        assert_eq!(state.highlight_index(), 0);
        assert_eq!(state.highlighted().unwrap().command, "/observe");
    }

    #[test]
    fn arrows_move_and_saturate_at_the_ends() {
        let mut state = state();
        let len = state.visible().len();
        // Up at the top stays at the top.
        state.move_up();
        assert_eq!(state.highlight_index(), 0);
        // Down walks to the last entry and saturates there.
        for _ in 0..(len + 5) {
            state.move_down();
        }
        assert_eq!(state.highlight_index(), len - 1);
        // Back up to the top.
        for _ in 0..(len + 5) {
            state.move_up();
        }
        assert_eq!(state.highlight_index(), 0);
    }

    #[test]
    fn typing_resets_the_highlight_into_the_filtered_view() {
        let mut state = state();
        state.move_down();
        state.move_down();
        assert_eq!(state.highlight_index(), 2);
        // Filtering resets the highlight to the top of the (newly narrowed) view so
        // it never points past the end of the filtered list.
        for character in "tutor".chars() {
            state.push_filter(character);
        }
        assert_eq!(state.highlight_index(), 0);
        assert_eq!(state.highlighted().unwrap().command, "/tutor");
    }

    #[test]
    fn selecting_returns_the_canonical_typed_form() {
        // The palette's job is to hand back the SAME string the user would type,
        // so selection routes to the identical action. Filter to /synopsis and
        // confirm the highlighted command is its canonical form.
        let mut state = state();
        for character in "syn".chars() {
            state.push_filter(character);
        }
        assert_eq!(state.highlighted().unwrap().command, "/synopsis");
    }

    // ---- rendering ----------------------------------------------------------

    #[test]
    fn render_marks_the_highlighted_row() {
        let state = state();
        let rendered = render_to_string(&state, false);
        // The first (highlighted) row carries the `>` marker; the others do not.
        let observe_line = rendered
            .lines()
            .find(|line| line.contains("/observe"))
            .unwrap();
        assert!(observe_line.trim_start().starts_with('>'));
        let quit_line = rendered
            .lines()
            .find(|line| line.contains("/quit"))
            .unwrap();
        assert!(!quit_line.trim_start().starts_with('>'));
    }

    #[test]
    fn render_shows_detail_only_when_requested() {
        let state = state();
        let without = render_to_string(&state, false);
        assert!(!without.contains("belief-neutral, clarify-only"));
        let with = render_to_string(&state, true);
        // The highlighted command is /observe; its detail mentions clarify-only.
        assert!(with.contains("belief-neutral, clarify-only"));
    }

    #[test]
    fn render_reports_an_empty_filtered_view() {
        let mut state = state();
        for character in "zzz".chars() {
            state.push_filter(character);
        }
        let rendered = render_to_string(&state, false);
        assert!(rendered.contains("no commands match"));
    }
}
