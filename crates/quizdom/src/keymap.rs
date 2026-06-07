// trace:STORY-176 | ai:claude
//! The single KEYMAP REGISTRY — the source of truth for every TUI keyboard
//! binding AND the cheat-sheet that documents them.
//!
//! ## Why one table
//!
//! STORY-176's key design principle: the key DISPATCHER (what a keystroke DOES)
//! and the CHEAT-SHEET overlay (what the user is TOLD a keystroke does) must
//! render from the SAME table so they can never drift. Adding a binding here
//! adds its cheat-sheet row automatically, and a test asserts the cheat-sheet is
//! generated from this registry (no hand-maintained duplicate).
//!
//! ## What lives here vs. elsewhere
//!
//! This registry is the DESCRIPTIVE surface: each [`KeyBinding`] pairs a
//! human-readable key label (`Ctrl-←`, `PageUp`, `o`, …) with the [`KeyAction`]
//! it triggers, a one-line description, and the [`KeyGroup`] it belongs to. The
//! TUI event loop (`tui.rs`) consults [`dispatch`] to turn a crossterm key event
//! into a [`KeyAction`], and renders the cheat-sheet from [`cheat_sheet_groups`]
//! / [`render_cheat_sheet`]. The headless front-end has no event loop, so it
//! degrades the cheat-sheet to a static printed list via the SAME renderer.
//!
//! Belief-neutral throughout: a keymap is pure plumbing — it decides HOW input
//! flows, never WHAT is asked or which belief is true.

use crossterm::event::{KeyCode, KeyModifiers};

/// The category a binding is grouped under in the cheat-sheet, in display order.
///
/// The cheat-sheet lists EVERY binding grouped by these, so the user can scan
/// "how do I answer?" / "how do I move around?" / "what meta-channels exist?" at
/// a glance. Ordering here is the ordering the cheat-sheet renders.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) enum KeyGroup {
    /// Answering the current question (Y/N/X/P/A/S/B and choice digits).
    Answering,
    /// Moving through the transcript without changing answers.
    Navigation,
    /// Out-of-band meta channels (observe / synopsis / score / help / tutor …).
    Meta,
    /// Line-editing affordances (vim/emacs per $EDITOR).
    Editing,
    /// Session-level controls (quit, interrupt, cheat-sheet).
    Session,
}

impl KeyGroup {
    /// The cheat-sheet section heading for this group.
    pub(crate) fn title(self) -> &'static str {
        match self {
            KeyGroup::Answering => "Answering",
            KeyGroup::Navigation => "Navigation",
            KeyGroup::Meta => "Meta",
            KeyGroup::Editing => "Editing",
            KeyGroup::Session => "Session",
        }
    }

    /// The groups in cheat-sheet display order.
    pub(crate) fn order() -> [KeyGroup; 5] {
        [
            KeyGroup::Answering,
            KeyGroup::Navigation,
            KeyGroup::Meta,
            KeyGroup::Editing,
            KeyGroup::Session,
        ]
    }
}

/// What a TUI keystroke DOES — the action the dispatcher returns and the engine
/// (or the event loop) acts on.
///
/// Navigation actions (`HighlightPrev`/`HighlightNext`, the scroll family,
/// `CheatSheet`) are TUI-only affordances handled inside the event loop; the
/// command actions (`Answer`-routed via the existing recognizers, etc.) are not
/// modeled here because they flow through the typed/`parse_control` path. This
/// enum captures the NON-text keystrokes the keymap owns.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum KeyAction {
    /// Ctrl-← : move the re-read HIGHLIGHT to the previous exchange (scroll-to-view
    /// only, non-destructive, clamped at the first exchange). 'B'/back stays the
    /// only way to actually revise.
    HighlightPrev,
    /// Ctrl-→ : move the re-read highlight to the next exchange (clamped at last).
    HighlightNext,
    /// Scroll the transcript pane UP one line (↑).
    ScrollLineUp,
    /// Scroll the transcript pane DOWN one line (↓).
    ScrollLineDown,
    /// Scroll the transcript pane up a page (PageUp / Ctrl-↑).
    ScrollPageUp,
    /// Scroll the transcript pane down a page (PageDown / Ctrl-↓).
    ScrollPageDown,
    /// `?` : open the keyboard CHEAT-SHEET overlay.
    CheatSheet,
}

/// One row of the keymap: a key label, the action it triggers (for non-text
/// keystrokes the TUI dispatches), its description, and its cheat-sheet group.
///
/// `action` is `Some` for the keystrokes the TUI event loop dispatches directly
/// (navigation + cheat-sheet); it is `None` for rows that DOCUMENT a binding the
/// engine handles through the typed-command recognizers (answer keys, meta
/// commands, editing, quit) — those still appear in the cheat-sheet (the user
/// must see them) but are not dispatched by [`dispatch`].
#[derive(Debug, Clone)]
pub(crate) struct KeyBinding {
    /// The human-readable key label shown in the cheat-sheet (e.g. `Ctrl-←`).
    pub(crate) keys: &'static str,
    /// The action a TUI keystroke triggers, when the keymap dispatches it.
    pub(crate) action: Option<KeyAction>,
    /// The concrete crossterm events that fire this row's action. Empty for rows
    /// the dispatcher does not own (answer keys, meta commands, editing) — those
    /// are DOCUMENTED here but routed by the event loop / command recognizers.
    /// [`dispatch`] scans these so the registry is the source of truth for BOTH
    /// what a key does and how it is documented.
    pub(crate) triggers: &'static [(KeyCode, KeyModifiers)],
    /// The one-line description shown in the cheat-sheet.
    pub(crate) description: &'static str,
    /// The cheat-sheet group this binding belongs to.
    pub(crate) group: KeyGroup,
}

/// The SINGLE keymap registry — every binding the TUI honors AND documents.
///
/// The dispatcher ([`dispatch`]) matches crossterm key events against the
/// dispatchable rows (the ones with an `action`), and the cheat-sheet renders
/// EVERY row grouped by [`KeyGroup`]. Adding a row here adds it to both, so the
/// two can never drift — the STORY-176 acceptance contract.
pub(crate) fn keymap_registry() -> Vec<KeyBinding> {
    // A DOCUMENTED row the dispatcher does not own (answer keys, meta commands,
    // editing, quit): it appears in the cheat-sheet but has no action / triggers.
    fn doc(keys: &'static str, description: &'static str, group: KeyGroup) -> KeyBinding {
        KeyBinding {
            keys,
            action: None,
            triggers: &[],
            description,
            group,
        }
    }
    // A DISPATCHABLE row: the keymap both documents it AND maps its crossterm
    // events to the action. `dispatch` scans these triggers, so the registry is
    // the single source of truth for what a key does and how it is documented.
    fn key(
        keys: &'static str,
        action: KeyAction,
        triggers: &'static [(KeyCode, KeyModifiers)],
        description: &'static str,
        group: KeyGroup,
    ) -> KeyBinding {
        KeyBinding {
            keys,
            action: Some(action),
            triggers,
            description,
            group,
        }
    }

    const CTRL: KeyModifiers = KeyModifiers::CONTROL;
    const NONE: KeyModifiers = KeyModifiers::NONE;

    vec![
        // ----- Answering ------------------------------------------------------
        doc("Y / N", "Answer a yes/no question", KeyGroup::Answering),
        doc("1-9", "Pick a multiple-choice option", KeyGroup::Answering),
        doc(
            "X",
            "eXplore — branch deeper into the question",
            KeyGroup::Answering,
        ),
        doc("P", "Punt — set this question aside", KeyGroup::Answering),
        doc(
            "A",
            "Add — author your own question from here (frontier)",
            KeyGroup::Answering,
        ),
        doc(
            "S",
            "Synopsis — belief-neutral reading of the whole session",
            KeyGroup::Answering,
        ),
        doc(
            "B",
            "Back — revisit and, if you choose, revise a previous answer",
            KeyGroup::Answering,
        ),
        // ----- Navigation -----------------------------------------------------
        key(
            "Ctrl-←",
            KeyAction::HighlightPrev,
            &[(KeyCode::Left, CTRL)],
            "Re-read the previous exchange (scroll-to-view; non-destructive)",
            KeyGroup::Navigation,
        ),
        key(
            "Ctrl-→",
            KeyAction::HighlightNext,
            &[(KeyCode::Right, CTRL)],
            "Re-read the next exchange (scroll-to-view; non-destructive)",
            KeyGroup::Navigation,
        ),
        key(
            "↑ / ↓",
            KeyAction::ScrollLineUp,
            &[(KeyCode::Up, NONE), (KeyCode::Down, NONE)],
            "Scroll the transcript one line",
            KeyGroup::Navigation,
        ),
        key(
            "PageUp / PageDown",
            KeyAction::ScrollPageUp,
            &[
                (KeyCode::PageUp, NONE),
                (KeyCode::PageDown, NONE),
                (KeyCode::Up, CTRL),
                (KeyCode::Down, CTRL),
            ],
            "Scroll the transcript a page (Ctrl-↑/↓ also)",
            KeyGroup::Navigation,
        ),
        // ----- Meta -----------------------------------------------------------
        doc(
            "o",
            "Observe — belief-neutral reading of THIS exchange (also /observe)",
            KeyGroup::Meta,
        ),
        doc(
            "/observe",
            "Observe this exchange (the palette / typed form of 'o')",
            KeyGroup::Meta,
        ),
        doc(
            "/tutor",
            "Articulation & nuance coach (sharpens YOUR point; never supplies it)",
            KeyGroup::Meta,
        ),
        doc(
            "/help",
            "Ask how the tool / dialogue works (belief-neutral)",
            KeyGroup::Meta,
        ),
        doc(
            "/synopsis",
            "Belief-neutral reading of the whole session (also 'S')",
            KeyGroup::Meta,
        ),
        doc(
            "/score",
            "Toggle the persistent distance-to-goal / roundedness gauge",
            KeyGroup::Meta,
        ),
        doc(
            "/objection",
            "Object — pin the exchange on a contested point",
            KeyGroup::Meta,
        ),
        doc(
            "/goal",
            "State or show the session goal/thesis",
            KeyGroup::Meta,
        ),
        doc(
            "/",
            "Open the slash-command palette (every command, with help)",
            KeyGroup::Meta,
        ),
        // ----- Editing --------------------------------------------------------
        doc(
            "vim / emacs",
            "Line editing follows $EDITOR (vi/vim/nvim → vi keys, else emacs)",
            KeyGroup::Editing,
        ),
        // ----- Session --------------------------------------------------------
        key(
            "?",
            KeyAction::CheatSheet,
            &[(KeyCode::Char('?'), NONE)],
            "Show this keyboard cheat-sheet",
            KeyGroup::Session,
        ),
        doc("/quit", "End the session (Q / Esc also)", KeyGroup::Session),
        doc("Ctrl-C", "Interrupt / end input", KeyGroup::Session),
    ]
}

/// Dispatch a crossterm key event to the [`KeyAction`] the keymap binds it to, or
/// `None` when no dispatchable binding matches (the event then flows on to the
/// text-editing / command path).
///
/// This is the dispatcher half of the single-table contract: it SCANS the SAME
/// registry the cheat-sheet renders, matching `(code, modifiers)` against each
/// row's `triggers`. The keymap only OWNS the non-text keystrokes (navigation
/// highlight, transcript scroll, the cheat-sheet key); the ordinary editing /
/// answer / command keys are handled by the event loop and the shared command
/// recognizers, so they carry no triggers (documented, not dispatched here).
///
/// One nuance the registry encodes compactly: the `↑ / ↓` and `PageUp / PageDown`
/// rows list MULTIPLE triggers (Down/PageDown included), but each carries a single
/// representative `action`. So a plain `↓` and `PageDown` resolve to the DOWN
/// variant via the small post-scan below; everything else dispatches by the
/// matched row's action directly. Pure over `(code, modifiers)`.
pub(crate) fn dispatch(code: KeyCode, modifiers: KeyModifiers) -> Option<KeyAction> {
    let normalized = (code, modifiers);
    let action = keymap_registry().into_iter().find_map(|binding| {
        binding
            .action
            .filter(|_| binding.triggers.contains(&normalized))
    })?;
    // The two scroll rows fold their up/down pair into one representative action;
    // resolve the DOWN direction from the concrete key so the event loop scrolls
    // the right way.
    let action = match (action, code) {
        (KeyAction::ScrollLineUp, KeyCode::Down) => KeyAction::ScrollLineDown,
        (KeyAction::ScrollPageUp, KeyCode::PageDown) => KeyAction::ScrollPageDown,
        (KeyAction::ScrollPageUp, KeyCode::Down) => KeyAction::ScrollPageDown,
        (other, _) => other,
    };
    Some(action)
}

/// The bindings for one cheat-sheet group, in registry order.
pub(crate) fn bindings_in_group(group: KeyGroup) -> Vec<KeyBinding> {
    keymap_registry()
        .into_iter()
        .filter(|binding| binding.group == group)
        .collect()
}

/// The cheat-sheet as a list of `(group, its bindings)` pairs in display order —
/// EVERY binding in the registry, grouped. The overlay and the headless static
/// list both render from this, so neither can drift from the dispatcher.
pub(crate) fn cheat_sheet_groups() -> Vec<(KeyGroup, Vec<KeyBinding>)> {
    KeyGroup::order()
        .into_iter()
        .map(|group| (group, bindings_in_group(group)))
        .collect()
}

/// Render the cheat-sheet to a plain string, grouped by [`KeyGroup`].
///
/// Generated ENTIRELY from [`keymap_registry`] (via [`cheat_sheet_groups`]) — no
/// hand-maintained row text — so adding a binding adds its row automatically and
/// the cheat-sheet can never drift from the dispatcher. The TUI overlay wraps
/// this in a styled paragraph; the headless front-end prints it verbatim.
pub(crate) fn render_cheat_sheet() -> String {
    let mut out = String::new();
    out.push_str("Keyboard cheat-sheet\n");
    for (group, bindings) in cheat_sheet_groups() {
        out.push_str(&format!("\n{}\n", group.title()));
        for binding in bindings {
            out.push_str(&format!(
                "  {:<18}  {}\n",
                binding.keys, binding.description
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the dispatch table -------------------------------------------------

    // trace:STORY-176 | ai:claude — the dispatcher maps keys to actions: the
    // navigation keys, page scroll (PageUp/Down + Ctrl-Up/Down), and the `?`
    // cheat-sheet key each resolve to their action.
    #[test]
    fn dispatch_maps_keys_to_actions() {
        assert_eq!(
            dispatch(KeyCode::Left, KeyModifiers::CONTROL),
            Some(KeyAction::HighlightPrev)
        );
        assert_eq!(
            dispatch(KeyCode::Right, KeyModifiers::CONTROL),
            Some(KeyAction::HighlightNext)
        );
        assert_eq!(
            dispatch(KeyCode::PageUp, KeyModifiers::NONE),
            Some(KeyAction::ScrollPageUp)
        );
        assert_eq!(
            dispatch(KeyCode::PageDown, KeyModifiers::NONE),
            Some(KeyAction::ScrollPageDown)
        );
        assert_eq!(
            dispatch(KeyCode::Up, KeyModifiers::CONTROL),
            Some(KeyAction::ScrollPageUp)
        );
        assert_eq!(
            dispatch(KeyCode::Down, KeyModifiers::CONTROL),
            Some(KeyAction::ScrollPageDown)
        );
        assert_eq!(
            dispatch(KeyCode::Up, KeyModifiers::NONE),
            Some(KeyAction::ScrollLineUp)
        );
        assert_eq!(
            dispatch(KeyCode::Down, KeyModifiers::NONE),
            Some(KeyAction::ScrollLineDown)
        );
        assert_eq!(
            dispatch(KeyCode::Char('?'), KeyModifiers::NONE),
            Some(KeyAction::CheatSheet)
        );
    }

    // trace:STORY-176 | ai:claude — the dispatcher SCANS the registry: every trigger
    // a dispatchable row declares resolves through `dispatch` to a non-None action,
    // so the registry's triggers and the dispatch path cannot drift — adding a
    // dispatchable binding row makes its keys live automatically.
    #[test]
    fn every_registry_trigger_is_dispatched() {
        for binding in keymap_registry() {
            if binding.action.is_none() {
                assert!(
                    binding.triggers.is_empty(),
                    "documented row {:?} must declare no triggers",
                    binding.keys
                );
                continue;
            }
            assert!(
                !binding.triggers.is_empty(),
                "dispatchable row {:?} must declare at least one trigger",
                binding.keys
            );
            for &(code, modifiers) in binding.triggers {
                assert!(
                    dispatch(code, modifiers).is_some(),
                    "trigger {:?} for {:?} is not dispatched",
                    (code, modifiers),
                    binding.keys
                );
            }
        }
    }

    // trace:STORY-176 | ai:claude — a plain answer key is NOT dispatched by the
    // keymap; it flows on to the editing / command path.
    #[test]
    fn dispatch_leaves_text_keys_to_the_editor() {
        assert_eq!(dispatch(KeyCode::Char('y'), KeyModifiers::NONE), None);
        assert_eq!(dispatch(KeyCode::Char('o'), KeyModifiers::NONE), None);
        assert_eq!(dispatch(KeyCode::Enter, KeyModifiers::NONE), None);
        assert_eq!(dispatch(KeyCode::Backspace, KeyModifiers::NONE), None);
    }

    // ---- cheat-sheet is GENERATED from the registry (no drift) --------------

    // trace:STORY-176 | ai:claude — the cheat-sheet rows are generated from the
    // SAME registry the dispatcher reads: every registry binding appears in the
    // rendered cheat-sheet, so the two can never drift (no hand-maintained list).
    #[test]
    fn cheat_sheet_is_generated_from_the_registry_with_no_drift() {
        let rendered = render_cheat_sheet();
        for binding in keymap_registry() {
            assert!(
                rendered.contains(binding.keys),
                "cheat-sheet missing the key label {:?}",
                binding.keys
            );
            assert!(
                rendered.contains(binding.description),
                "cheat-sheet missing the description for {:?}",
                binding.keys
            );
        }
        // Every group heading appears, in order.
        let mut last = 0usize;
        for group in KeyGroup::order() {
            let at = rendered
                .find(group.title())
                .unwrap_or_else(|| panic!("cheat-sheet missing the {} group", group.title()));
            assert!(at >= last, "groups must render in order");
            last = at;
        }
    }

    // trace:STORY-176 | ai:claude — every registry row is partitioned into exactly
    // one cheat-sheet group, so the grouped view documents every binding (the
    // "lists EVERY binding, grouped" acceptance criterion).
    #[test]
    fn every_binding_appears_in_exactly_one_group() {
        let total = keymap_registry().len();
        let grouped: usize = cheat_sheet_groups()
            .iter()
            .map(|(_, bindings)| bindings.len())
            .sum();
        assert_eq!(grouped, total, "every binding is grouped exactly once");
    }

    // trace:STORY-176 | ai:claude — the observe affordance is now 'o' (the DECIDED
    // move off '?'), and '?' is the cheat-sheet key — documented in the registry.
    #[test]
    fn observe_is_o_and_question_mark_is_the_cheat_sheet() {
        let registry = keymap_registry();
        let observe = registry
            .iter()
            .find(|b| b.keys == "o")
            .expect("an 'o' observe binding");
        assert_eq!(observe.group, KeyGroup::Meta);
        assert!(observe.description.to_lowercase().contains("observe"));

        let cheat = registry
            .iter()
            .find(|b| b.keys == "?")
            .expect("a '?' cheat-sheet binding");
        assert_eq!(cheat.action, Some(KeyAction::CheatSheet));
        // No registry row binds '?' to observe any more.
        assert!(
            !registry
                .iter()
                .any(|b| b.keys == "?" && b.description.to_lowercase().contains("observe")),
            "'?' must no longer be the observe key"
        );
    }
}
