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
        // trace:STORY-174 | ai:claude — the persistent score-gauge toggle.
        PaletteCommand {
            command: "/score",
            description: "Toggle a persistent distance-to-goal / roundedness gauge",
            detail: "Toggles a PERSISTENT gauge in the status bar (headless: the breadcrumb \
footer). With a goal set it reads as estimated DISTANCE TO GOAL (how far the goal is settled \
plus the remaining open thread); with no goal it reads general structural roundedness. \
Default OFF until you type /score. It needs an LLM pass, so it recomputes at GATES (every few \
answered turns), showing the last value with a freshness marker in between — never every turn. \
Belief-neutral: it scores STRUCTURE / progress, never which belief is correct. Non-destructive: \
returns to your question.",
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
With a goal set, a bare /goal shows the current goal. With none set, a bare /goal offers to \
REQUEST one (the Observer proposes from the conversation so far). Once set, the goal orients \
the next questions and the breadcrumb.",
        },
        // trace:STORY-173 | ai:claude
        PaletteCommand {
            command: "/request-goal",
            description: "Propose a session goal from the conversation so far",
            detail: "Asks the Observer to PROPOSE a session goal directly from the conversation \
so far, skipping the bare-/goal confirm. It offers the proposal to accept, edit, or decline; \
nothing is set unless you accept. Belief-neutral: the proposed goal is the QUESTION being \
resolved, never a belief to adopt.",
        },
        PaletteCommand {
            command: "/mode",
            description: "Toggle questioning mode (socratic | debate)",
            detail: "Switches the questioner's stance between socratic (neutral Socratic \
questioning) and debate (the questioner steelmans the OPPOSING side's craft). Belief-neutral: \
debate challenges the strength of your case, it never asserts which belief is true. A bare \
/mode shows the current mode.",
        },
        // trace:STORY-175 | ai:claude — the court-case objection mechanic.
        PaletteCommand {
            command: "/objection",
            description: "Object — pin the exchange on a contested point",
            detail: "Raises a court-style OBJECTION (either party may), PINNING the exchange on a \
contested point: the questioner narrows its next questions to it and normal advancement pauses \
until it is cleared. Clear it with /resolved (only the party who raised it) or /judge (only the \
OTHER party, who hands it to the Observer to rule on). One objection at a time. Belief-neutral: \
an objection names a STRUCTURAL tension, never a counter-belief.",
        },
        PaletteCommand {
            command: "/resolved",
            description: "Resolve YOUR open objection (objector only)",
            detail: "Clears the open objection by WITHDRAWING or ACCEPTING its resolution — only \
the party who RAISED the objection may call it. Returns the dialogue to normal flow and logs the \
resolution. If you are the other party, use /judge instead.",
        },
        PaletteCommand {
            command: "/judge",
            description: "Have the Observer rule on the other party's objection",
            detail: "Escalates the open objection to the OBSERVER for a BELIEF-NEUTRAL ruling — \
only the party who did NOT raise it may call it. The Observer rules SUSTAINED (the point is \
material and unaddressed) or OVERRULED (immaterial or already addressed) and names the resolving \
condition; a sustained objection becomes a tracked open thread that widens the distance-to-goal \
until addressed, while the dialogue proceeds. It judges STRUCTURE, never which belief is true. \
Needs an LLM backend (degrades to a note offline).",
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

// trace:STORY-177 | ai:claude — the palette's two MATCH MODES, derived live from
// whether the input buffer still carries its leading `/` sigil.
/// Which way the palette is matching the buffer against the registry.
///
/// The mode is never stored — it is recomputed on every keystroke from whether
/// the buffer starts with `/` (see [`PaletteState::mode`]). It exists only to
/// name the two behaviours [`PaletteState::visible`] selects between.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum FilterMode {
    /// Buffer starts with `/`: the `/` is the command SIGIL ("I'm typing a
    /// command"), so we PREFIX-match the rest of the buffer against the command
    /// NAME only. `/q` → `/quit`, `/question…`; `/sc` → `/score`.
    Prefix,
    /// Buffer has no leading `/` (the user backspaced it away, or it was never
    /// there): case-insensitive SUBSTRING match ANYWHERE in the command name OR
    /// description (discovery/search). `goal` → `/goal`, `/request-goal`, and
    /// anything whose description mentions goals.
    Search,
}

/// The pure state behind the palette overlay: the registry, the current input
/// buffer, and which of the filtered commands is highlighted.
///
/// All filter / navigation / selection logic lives here so it can be unit-tested
/// without a terminal; the crossterm driver in [`run_palette`] only translates
/// key events into these method calls and renders [`visible`] / [`filter`] /
/// [`highlighted`].
///
/// The buffer is the EXACT text the user has typed, INCLUDING a leading `/` when
/// present — the `/` that opened the palette starts in the buffer, and whether
/// it survives selects the [`FilterMode`] live on every keystroke (STORY-177).
#[derive(Debug, Clone)]
pub(crate) struct PaletteState {
    commands: Vec<PaletteCommand>,
    /// The raw input buffer, including a leading `/` when present.
    filter: String,
    /// Index into the CURRENT filtered view (`visible()`), not into `commands`.
    highlight: usize,
}

impl PaletteState {
    /// Build the palette state from a command registry.
    ///
    /// The buffer starts with the `/` sigil that opened the palette, so the
    /// palette begins in [`FilterMode::Prefix`] (with an empty prefix, which
    /// shows every command). Backspacing that `/` away flips to
    /// [`FilterMode::Search`] without closing (STORY-177).
    pub(crate) fn new(commands: Vec<PaletteCommand>) -> Self {
        Self {
            commands,
            filter: String::from("/"),
            highlight: 0,
        }
    }

    /// The raw input buffer typed so far, INCLUDING a leading `/` when present.
    pub(crate) fn filter(&self) -> &str {
        &self.filter
    }

    // trace:STORY-177 | ai:claude
    /// The current match mode, recomputed live from whether the buffer still
    /// carries its leading `/` sigil. Never stored — always derived here.
    pub(crate) fn mode(&self) -> FilterMode {
        if self.filter.starts_with('/') {
            FilterMode::Prefix
        } else {
            FilterMode::Search
        }
    }

    // trace:STORY-177 | ai:claude
    /// The commands matching the current buffer, in registry order.
    ///
    /// The match RULE switches on [`mode`](Self::mode):
    /// - [`FilterMode::Prefix`] (buffer starts with `/`): the rest of the buffer
    ///   PREFIX-matches the command NAME only (case-insensitive). An empty
    ///   prefix (bare `/`) shows every command.
    /// - [`FilterMode::Search`] (no leading `/`): a case-insensitive SUBSTRING
    ///   match ANYWHERE in the command name OR description. An empty buffer shows
    ///   every command.
    pub(crate) fn visible(&self) -> Vec<PaletteCommand> {
        match self.mode() {
            FilterMode::Prefix => {
                // Strip the sigil; the remainder is the NAME prefix to match.
                let needle = self.filter[1..].trim().to_ascii_lowercase();
                if needle.is_empty() {
                    return self.commands.clone();
                }
                self.commands
                    .iter()
                    .filter(|command| name_has_prefix(command, &needle))
                    .copied()
                    .collect()
            }
            FilterMode::Search => {
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
        }
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

    // trace:STORY-177 | ai:claude
    /// Remove the last buffer character (Backspace). Returns `false` ONLY when
    /// the buffer is already EMPTY — the caller treats that as "close the
    /// palette".
    ///
    /// Note the STORY-177 nuance: backspacing the LEADING `/` no longer closes
    /// the palette. With the `/` in the buffer, popping it leaves an empty
    /// (non-`/`) buffer — still a successful pop (`true`) — which FLIPS the
    /// palette into [`FilterMode::Search`] showing all commands. Only a further
    /// Backspace, on the now-empty buffer, returns `false` to close.
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

/// Whether a command matches the (already lowercased, non-empty) SEARCH needle —
/// a substring anywhere in its name (sans leading `/`) or its description.
fn command_matches(command: &PaletteCommand, needle: &str) -> bool {
    let name = command.command.trim_start_matches('/').to_ascii_lowercase();
    name.contains(needle) || command.description.to_ascii_lowercase().contains(needle)
}

// trace:STORY-177 | ai:claude
/// Whether a command's NAME (sans leading `/`) STARTS WITH the (already
/// lowercased, non-empty) prefix needle — the prefix-mode predicate. Names only;
/// the description is intentionally NOT consulted in prefix mode.
fn name_has_prefix(command: &PaletteCommand, needle: &str) -> bool {
    let name = command.command.trim_start_matches('/').to_ascii_lowercase();
    name.starts_with(needle)
}

/// Run the slash-command palette overlay and return the user's choice.
///
/// Degrades on a non-TTY stdin: returns `None` so the caller falls back to the
/// typed-command path (a bare `/` line is then handled as ordinary input). On a
/// TTY it takes over the terminal in raw mode, renders the menu to `output`,
/// drives the key loop via [`PaletteState`], restores the prompt, and returns
/// `Some(outcome)`.
pub(crate) fn run_palette(output: &mut dyn Write) -> io::Result<Option<PaletteOutcome>> {
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
pub(crate) fn run_palette_in_raw(output: &mut dyn Write) -> io::Result<Option<PaletteOutcome>> {
    let mut state = PaletteState::new(command_registry());
    drive_palette(&mut state, output).map(Some)
}

/// The render + key loop, factored out of [`run_palette`] so the raw-mode guard
/// in the caller always restores the terminal even on an error path.
fn drive_palette(state: &mut PaletteState, output: &mut dyn Write) -> io::Result<PaletteOutcome> {
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
                // trace:STORY-177 | ai:claude — `pop_filter` returns false ONLY
                // on a truly empty buffer; backspacing the leading `/` succeeds
                // (flips to search mode) and keeps the overlay open.
                if !state.pop_filter() {
                    // Backspacing an EMPTY buffer closes the overlay.
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
    output: &mut dyn Write,
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
    // trace:STORY-177 | ai:claude — the buffer is shown VERBATIM (it already
    // carries its leading `/` when present), and the active match mode is named
    // so prefix-vs-search is visible at a glance.
    out.push_str("Slash-command palette");
    if !state.filter().is_empty() {
        let mode = match state.mode() {
            FilterMode::Prefix => "name-prefix",
            FilterMode::Search => "search",
        };
        out.push_str(&format!(" — {mode}: {}", state.filter()));
    } else {
        out.push_str(" — search (Backspace to close)");
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

    // trace:STORY-177 | ai:claude — a palette in SEARCH mode: open it (buffer
    // `/`) then backspace the sigil away, the same path a user takes to flip
    // modes. The buffer is now empty and `mode()` reports `Search`.
    fn search_state() -> PaletteState {
        let mut state = state();
        assert!(state.pop_filter(), "popping the `/` sigil succeeds");
        assert_eq!(state.mode(), FilterMode::Search);
        state
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
        // trace:STORY-175 | ai:claude — "synopsis" uniquely names /synopsis (it does
        // not appear in any other command's name or one-line description), so the
        // filter narrows to exactly it. (The broader "ob" / "obs" now also match the
        // court-case /objection / /judge family, by design — a name substring still
        // narrows, just to whatever genuinely contains it.)
        let mut state = state();
        for character in "synopsis".chars() {
            state.push_filter(character);
        }
        let visible = state.visible();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].command, "/synopsis");
    }

    // trace:STORY-177 | ai:claude — in SEARCH mode (no leading `/`), the match is
    // a case-insensitive substring anywhere in the name OR description. "nuance"
    // appears only in /tutor's description, not its name.
    #[test]
    fn search_mode_is_case_insensitive_and_matches_descriptions() {
        let mut state = search_state();
        for character in "NUANCE".chars() {
            state.push_filter(character);
        }
        assert_eq!(state.mode(), FilterMode::Search);
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

    // trace:STORY-177 | ai:claude — backspace closes ONLY on a truly empty
    // buffer. From a fresh palette the buffer is `/`; one Backspace removes a
    // typed char (or, at the sigil, flips to search), and only a Backspace on the
    // now-empty buffer reports closure.
    #[test]
    fn backspacing_an_empty_buffer_reports_closure() {
        let mut state = state();
        // Buffer is `/x`. Pop the char (-> `/`), pop the sigil (-> ``, still a
        // successful pop that FLIPS to search), then pop the empty buffer -> close.
        state.push_filter('x');
        assert!(state.pop_filter(), "popping a typed char succeeds");
        assert!(
            state.pop_filter(),
            "popping the leading `/` succeeds (flips to search, does NOT close)"
        );
        assert_eq!(state.mode(), FilterMode::Search);
        assert!(
            !state.pop_filter(),
            "popping the now-empty buffer reports closure (cancel the overlay)"
        );
    }

    // trace:STORY-177 | ai:claude — with the leading `/`, the palette is in
    // PREFIX mode and matches the command NAME ONLY (not the description). `/q`
    // lists exactly the commands whose name starts with `q`.
    #[test]
    fn prefix_mode_matches_names_only_by_prefix() {
        let mut state = state();
        assert_eq!(state.mode(), FilterMode::Prefix);
        state.push_filter('q'); // buffer is now `/q`
        let visible = state.visible();
        let names: Vec<&str> = visible.iter().map(|c| c.command).collect();
        // /quit is the only command whose NAME starts with `q`.
        assert_eq!(names, vec!["/quit"]);
        // Description-only matches must NOT appear: /punt's description mentions
        // "question" but its name does not start with `q`.
        assert!(!names.contains(&"/punt"));
    }

    // trace:STORY-177 | ai:claude — a more pointed names-only prefix check: `/sc`
    // resolves to /score, and a command merely mentioning "score" in its
    // description (none here start with `sc`) would not slip in.
    #[test]
    fn prefix_mode_resolves_a_two_char_prefix_to_one_command() {
        let mut state = state();
        for character in "sc".chars() {
            state.push_filter(character);
        }
        let visible = state.visible();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].command, "/score");
    }

    // trace:STORY-177 | ai:claude — in SEARCH mode, "goal" matches by NAME
    // (/goal, /request-goal) AND by DESCRIPTION (any command whose description
    // mentions goals), proving the substring spans both fields.
    #[test]
    fn search_mode_matches_name_and_description_substrings() {
        let mut state = search_state();
        for character in "goal".chars() {
            state.push_filter(character);
        }
        let names: Vec<&str> = state.visible().iter().map(|c| c.command).collect();
        // Name matches.
        assert!(names.contains(&"/goal"));
        assert!(names.contains(&"/request-goal"));
        // Description matches: /request-goal aside, /score's detail/description
        // and others reference goals — at minimum /score's description mentions
        // distance-to-goal. Assert a description-only hit beyond the name hits.
        assert!(
            names.contains(&"/score"),
            "search mode must catch description-only goal mentions, got {names:?}"
        );
    }

    // trace:STORY-177 | ai:claude — the mode FLIPS live when the leading `/` is
    // removed: same residual text, different result set. `/q` (prefix, names) vs
    // `q` (search, name OR description) must differ.
    #[test]
    fn removing_the_slash_flips_prefix_to_search() {
        let mut state = state();
        state.push_filter('q'); // `/q` — prefix mode, names starting with `q`
        assert_eq!(state.mode(), FilterMode::Prefix);
        let prefix_names: Vec<&str> = state.visible().iter().map(|c| c.command).collect();
        assert_eq!(prefix_names, vec!["/quit"]);

        // Backspace the char, then the `/` — buffer becomes empty, search mode.
        assert!(state.pop_filter()); // -> `/`
        assert!(state.pop_filter()); // -> `` (flip to search, palette stays open)
        assert_eq!(state.mode(), FilterMode::Search);
        // Re-type `q`: now SEARCH — names OR descriptions containing `q` anywhere.
        state.push_filter('q');
        let search_names: Vec<&str> = state.visible().iter().map(|c| c.command).collect();
        assert!(search_names.contains(&"/quit"));
        // /punt's description ("Punt this question…") contains `q` via "question";
        // search mode catches it where prefix mode did not.
        assert!(
            search_names.contains(&"/punt"),
            "search mode must catch description `q`, got {search_names:?}"
        );
        assert!(
            search_names.len() > prefix_names.len(),
            "search is broader than prefix for the same residual text"
        );
    }

    // trace:STORY-177 | ai:claude — an empty buffer shows ALL commands in BOTH
    // modes (bare `/` prefix and fully-empty search).
    #[test]
    fn empty_buffer_shows_all_in_both_modes() {
        // Bare `/` (prefix, empty prefix) shows all.
        let prefix = state();
        assert_eq!(prefix.mode(), FilterMode::Prefix);
        assert_eq!(prefix.visible().len(), command_registry().len());
        // Empty buffer (search) shows all.
        let search = search_state();
        assert_eq!(search.visible().len(), command_registry().len());
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
