// trace:STORY-76 | ai:claude
//! Lightweight terminal styling for interactive session output.
//!
//! Styling is gated on a process-global flag that defaults **off**, so unit
//! tests (which render into in-memory buffers) and piped / redirected use stay
//! plain text. [`run_cli`](crate::run_cli) calls [`init_from_env`] once at
//! startup to enable color only when stdout is a TTY and `NO_COLOR` is unset.
//! Every styled call site funnels through [`paint`], which is a no-op when the
//! flag is off — so a session whose stdout is captured, piped, or under test
//! emits exactly the same bytes it did before this module existed.

use anstyle::{AnsiColor, Style};
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

static COLOR_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable color iff stdout is a terminal and `NO_COLOR` is unset.
///
/// Honors the [`NO_COLOR`](https://no-color.org) convention: any value of the
/// variable — even empty — disables color. Non-TTY stdout (pipes, redirects,
/// captured output) degrades to plain.
pub(crate) fn init_from_env() {
    let enabled = decide(
        std::env::var_os("NO_COLOR").is_some(),
        std::io::stdout().is_terminal(),
    );
    set_enabled(enabled);
}

/// Pure decision behind [`init_from_env`] — split out so the gating logic is
/// testable without touching the real environment or stdout.
fn decide(no_color_set: bool, stdout_is_tty: bool) -> bool {
    !no_color_set && stdout_is_tty
}

pub(crate) fn set_enabled(enabled: bool) {
    COLOR_ENABLED.store(enabled, Ordering::Relaxed);
}

pub(crate) fn enabled() -> bool {
    COLOR_ENABLED.load(Ordering::Relaxed)
}

/// Wrap `text` in `style`'s SGR codes when color is enabled; otherwise return
/// it unchanged. Returns an owned `String` so call sites can drop it straight
/// into a `writeln!`.
pub(crate) fn paint(style: Style, text: &str) -> String {
    paint_with(enabled(), style, text)
}

/// Pure core of [`paint`] — takes the enabled flag explicitly so it can be
/// unit-tested without mutating the shared global (which would race the other
/// tests running in parallel).
fn paint_with(enabled: bool, style: Style, text: &str) -> String {
    if enabled {
        format!("{}{}{}", style.render(), text, style.render_reset())
    } else {
        text.to_string()
    }
}

/// The question title / prompt — the line the user must read first.
pub(crate) fn question() -> Style {
    Style::new().bold().fg_color(Some(AnsiColor::Cyan.into()))
}

/// A numbered multiple-choice option marker.
pub(crate) fn option() -> Style {
    Style::new().fg_color(Some(AnsiColor::Green.into()))
}

/// The `[Y/N/X/P/B/F/Q]` control prompt — present but secondary, so dimmed.
pub(crate) fn control() -> Style {
    Style::new().fg_color(Some(AnsiColor::BrightBlack.into()))
}

/// Header for a surfaced block of TERM definitions to distinguish.
pub(crate) fn term() -> Style {
    Style::new().bold().fg_color(Some(AnsiColor::Yellow.into()))
}

/// A surfaced contradiction prompt — flag a real tension in the user's beliefs.
pub(crate) fn contradiction() -> Style {
    Style::new()
        .bold()
        .fg_color(Some(AnsiColor::Magenta.into()))
}

// trace:STORY-78 | ai:claude
/// The in-session orientation breadcrumb (topic / depth / branch) — present
/// every turn but secondary to the question, so dimmed like the control prompt.
pub(crate) fn breadcrumb() -> Style {
    Style::new().fg_color(Some(AnsiColor::BrightBlack.into()))
}

// trace:STORY-175 | ai:claude
/// The GAVEL motif for an open court-case `/objection` — the status glyph shown
/// while the exchange is PINNED on a contested point, and on the Observer's
/// `/judge` ruling. A single source of truth so the headless footer and the TUI
/// status bar render the same court motif. Belief-neutral chrome: it marks that a
/// point is contested, never which belief is true.
pub(crate) const OBJECTION_GAVEL: &str = "[gavel]";

// trace:STORY-127 | ai:claude
/// The Observer's META voice — the belief-neutral exchange reading surfaced by
/// the `?` key. Styled distinctly (dimmed italic blue) so it reads as a
/// separate, secondary voice commenting on the exchange, never mistaken for the
/// question itself.
pub(crate) fn meta() -> Style {
    Style::new()
        .italic()
        .fg_color(Some(AnsiColor::BrightBlue.into()))
}

// trace:STORY-171 | ai:claude
/// The centralized ratatui THEME for the full-screen TUI front-end.
///
/// One place to tweak every color the TUI paints: pane borders, the input
/// cursor, the colorized status bar, and the per-role transcript palette. The
/// line front-end and the engine's plain-text rendering never touch this — they
/// funnel through [`paint`]/[`init_from_env`] above and honor `NO_COLOR` + the
/// non-TTY gate. The TUI disables engine-side color and re-styles the plain
/// transcript through this module instead, so a quoted span in the user's answer
/// can carry the interrogator's color (quote-attribution) and each voice reads
/// in its own hue. Belief-neutral: this is presentation only — it never decides
/// WHAT is said, only how the already-emitted text is colored.
pub(crate) mod theme {
    use ratatui::style::{Color, Modifier, Style};

    // ----- accent + structural colors ------------------------------------

    /// The warm gold/amber accent used for pane borders.
    pub(crate) const BORDER: Color = Color::Rgb(0xD4, 0xA0, 0x17); // amber gold
    /// The input cursor / caret accent — GOLD, matching the borders.
    pub(crate) const CURSOR: Color = Color::Rgb(0xFF, 0xC1, 0x07); // bright gold
    /// The input-box prompt marker (`> `).
    pub(crate) const INPUT_MARKER: Color = CURSOR;

    // ----- status bar palette --------------------------------------------

    /// The status bar's own dim backdrop text (default hint / separators).
    pub(crate) const STATUS_DIM: Color = Color::DarkGray;
    /// A status-bar segment LABEL (e.g. `topic:`, `mode:`).
    pub(crate) const STATUS_LABEL: Color = Color::Rgb(0xD4, 0xA0, 0x17);
    /// A status-bar segment VALUE (the topic / depth / branch / mode text).
    pub(crate) const STATUS_VALUE: Color = Color::Rgb(0x9E, 0xC1, 0xE0);

    // ----- per-role transcript palette (PROPOSED defaults) ---------------

    /// The interrogator / questioner voice — CYAN.
    pub(crate) const INTERROGATOR: Color = Color::Cyan;
    /// The user's own answers — GREEN.
    pub(crate) const USER: Color = Color::Green;
    /// The challenger (debate questions + closing objections) — MAGENTA.
    pub(crate) const CHALLENGER: Color = Color::Magenta;
    /// The observer / META voice — bright blue, ITALIC (mirrors [`super::meta`]).
    pub(crate) const META: Color = Color::LightBlue;

    /// The ratatui style for a pane border.
    pub(crate) fn border() -> Style {
        Style::default().fg(BORDER)
    }

    // trace:STORY-176 | ai:claude
    /// The background accent for the re-read HIGHLIGHT line (the exchange the user
    /// is re-reading via Ctrl-←/→). A subtle dark-gray band so the highlighted row
    /// stands out without recoloring its voice. Belief-neutral chrome: it marks
    /// WHERE the user is looking, never which belief is true.
    pub(crate) fn reread_highlight() -> Style {
        Style::default()
            .bg(Color::Rgb(0x33, 0x33, 0x33))
            .add_modifier(Modifier::BOLD)
    }

    /// The ratatui style for the input cursor marker (`> `).
    pub(crate) fn input_marker() -> Style {
        Style::default()
            .fg(INPUT_MARKER)
            .add_modifier(Modifier::BOLD)
    }

    /// The ratatui style for the META voice in the transcript — bright blue,
    /// italic — matching the line front-end's [`super::meta`] anstyle exactly.
    pub(crate) fn meta_style() -> Style {
        Style::default().fg(META).add_modifier(Modifier::ITALIC)
    }

    /// The base ratatui style for a transcript [`Role`].
    pub(crate) fn role_style(role: Role) -> Style {
        match role {
            Role::Interrogator => Style::default().fg(INTERROGATOR),
            Role::User => Style::default().fg(USER),
            Role::Challenger => Style::default().fg(CHALLENGER),
            Role::Meta => meta_style(),
            // A plain / structural line (blank lines, control prompts,
            // breadcrumbs already mirrored into the status bar) gets no accent
            // so the colored voices stand out against it.
            Role::Plain => Style::default(),
        }
    }

    /// The voice a transcript line belongs to, for coloring. Attribution is a
    /// pure heuristic over the plain text the engine already emitted (the TUI
    /// runs the engine with color OFF) — never a belief judgement.
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub(crate) enum Role {
        /// The interrogator / questioner (the question title + its framing).
        Interrogator,
        /// The user's own typed answer (the echoed `> …` line).
        User,
        /// The observer / META voice (observe / tutor / help / synopsis / verdict).
        Meta,
        /// The challenger (debate questions + closing objections).
        Challenger,
        /// Structural / neutral text with no voice accent.
        Plain,
    }

    /// Classify a single transcript line into the voice that produced it.
    ///
    /// Heuristic, in priority order, over the textual markers the engine prints:
    /// - the META voice prefixes every block with `META` (observer / synopsis /
    ///   verdict / help / tutor / conclusion / closing).
    /// - the challenger labels its closing turn `Challenger …`.
    /// - the user's answer is echoed back as `> …` by the TUI input loop.
    /// - an empty line carries no voice.
    /// - anything else is the interrogator's question / framing.
    ///
    /// Pure + total so the attribution is unit-testable without a terminal.
    pub(crate) fn classify_line(line: &str) -> Role {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            return Role::Plain;
        }
        if trimmed.starts_with("META") {
            return Role::Meta;
        }
        if trimmed.starts_with("Challenger") {
            return Role::Challenger;
        }
        if trimmed.starts_with("> ") || trimmed == ">" {
            return Role::User;
        }
        // The orientation breadcrumb is mirrored into the status bar; keep it
        // neutral in the transcript so it does not read as the question.
        if trimmed.starts_with("[topic:") {
            return Role::Plain;
        }
        Role::Interrogator
    }

    /// A styled fragment of a transcript line: a slice of text plus the style it
    /// should render in. The TUI maps these straight to ratatui `Span`s.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct StyledFragment {
        pub(crate) text: String,
        pub(crate) style: Style,
    }

    /// The OPPOSING role for quote-attribution within the interrogator<->user
    /// pair: each party quotes the other, so a quoted span in a line renders in
    /// the other party's color. Only the interrogator<->user pair participates;
    /// every other role has no opposing voice (returns `None`), leaving its
    /// lines un-reattributed.
    // trace:BUG-172 | ai:claude
    pub(crate) fn opposing_role(role: Role) -> Option<Role> {
        match role {
            Role::User => Some(Role::Interrogator),
            Role::Interrogator => Some(Role::User),
            Role::Challenger | Role::Meta | Role::Plain => None,
        }
    }

    /// Split one transcript line into styled fragments, applying SYMMETRIC QUOTE
    /// ATTRIBUTION across the interrogator<->user pair: any double-quoted span
    /// (`"…"`) within a participating line renders in the OPPOSING role's color
    /// (each party quotes the other). So a quote inside the USER's answer renders
    /// in the INTERROGATOR's color, and a quote inside the INTERROGATOR's line
    /// renders in the USER's color (BUG-172, extending STORY-171's one-directional
    /// heuristic). Non-participating roles (meta / challenger / plain) and lines
    /// without a quote render as a single fragment in the base style.
    ///
    /// Pure over `(role, line)` so the span model is testable without drawing.
    // trace:BUG-172 | ai:claude
    pub(crate) fn line_fragments(role: Role, line: &str) -> Vec<StyledFragment> {
        let base = role_style(role);
        let opposing = opposing_role(role);
        if opposing.is_none() || !line.contains('"') {
            return vec![StyledFragment {
                text: line.to_string(),
                style: base,
            }];
        }

        // The quoted span carries the OPPOSING role's color (the party being
        // quoted). Safe to unwrap: the `opposing.is_none()` guard returned above.
        let quote_style = role_style(opposing.expect("opposing role present"));
        let mut fragments: Vec<StyledFragment> = Vec::new();
        let mut current = String::new();
        let mut in_quote = false;
        for ch in line.chars() {
            if ch == '"' {
                if in_quote {
                    // Closing quote: include it in the quoted (opposing-role) span.
                    current.push(ch);
                    push_fragment(&mut fragments, &mut current, quote_style);
                    in_quote = false;
                } else {
                    // Opening quote: flush the base-colored run, then start the
                    // quoted span WITH the opening quote char.
                    push_fragment(&mut fragments, &mut current, base);
                    current.push(ch);
                    in_quote = true;
                }
            } else {
                current.push(ch);
            }
        }
        // Trailing run: an unterminated quote stays attributed to the opposing
        // role (the quote ran to end of line); otherwise it is the base role.
        let trailing_style = if in_quote { quote_style } else { base };
        push_fragment(&mut fragments, &mut current, trailing_style);

        if fragments.is_empty() {
            fragments.push(StyledFragment {
                text: line.to_string(),
                style: base,
            });
        }
        fragments
    }

    /// Flush a non-empty pending run into `fragments` under `style`, clearing it.
    fn push_fragment(fragments: &mut Vec<StyledFragment>, current: &mut String, style: Style) {
        if !current.is_empty() {
            fragments.push(StyledFragment {
                text: std::mem::take(current),
                style,
            });
        } else {
            current.clear();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn classify_line_attributes_each_voice_by_its_marker() {
            assert_eq!(
                classify_line("META (observer) — a belief-neutral reading:"),
                Role::Meta
            );
            assert_eq!(
                classify_line("Challenger (closing) — strongest remaining objection:"),
                Role::Challenger
            );
            assert_eq!(classify_line("> free will is an illusion"), Role::User);
            assert_eq!(
                classify_line("Is your will truly free?"),
                Role::Interrogator
            );
            assert_eq!(classify_line(""), Role::Plain);
            assert_eq!(classify_line("   "), Role::Plain);
            assert_eq!(
                classify_line("[topic: free will | depth: 2 | branch: main]"),
                Role::Plain
            );
        }

        #[test]
        fn role_style_gives_each_voice_its_themed_color() {
            assert_eq!(role_style(Role::Interrogator).fg, Some(INTERROGATOR));
            assert_eq!(role_style(Role::User).fg, Some(USER));
            assert_eq!(role_style(Role::Challenger).fg, Some(CHALLENGER));
            assert_eq!(role_style(Role::Meta).fg, Some(META));
            // The META voice keeps the italic modifier of the line front-end.
            assert!(role_style(Role::Meta)
                .add_modifier
                .contains(Modifier::ITALIC));
            assert_eq!(role_style(Role::Plain).fg, None);
        }

        #[test]
        fn non_user_lines_render_as_a_single_base_fragment() {
            let frags = line_fragments(Role::Interrogator, "Is your will free?");
            assert_eq!(frags.len(), 1);
            assert_eq!(frags[0].text, "Is your will free?");
            assert_eq!(frags[0].style.fg, Some(INTERROGATOR));
        }

        #[test]
        fn user_line_without_a_quote_is_one_green_fragment() {
            let frags = line_fragments(Role::User, "> I think it is real");
            assert_eq!(frags.len(), 1);
            assert_eq!(frags[0].style.fg, Some(USER));
        }

        #[test]
        fn quoted_span_in_user_answer_takes_the_interrogator_color() {
            // The user quotes the interrogator: the quoted run renders in CYAN,
            // the surrounding answer stays GREEN.
            let frags = line_fragments(Role::User, r#"> you asked "is it free" and I say no"#);
            // Three runs: leading user text, the quoted interrogator span, trailing user text.
            assert_eq!(frags.len(), 3);
            assert_eq!(frags[0].style.fg, Some(USER));
            assert_eq!(frags[0].text, "> you asked ");
            assert_eq!(frags[1].style.fg, Some(INTERROGATOR));
            assert_eq!(frags[1].text, r#""is it free""#);
            assert_eq!(frags[2].style.fg, Some(USER));
            assert_eq!(frags[2].text, " and I say no");
            // Reassembling the fragments reproduces the original line exactly.
            let joined: String = frags.iter().map(|f| f.text.as_str()).collect();
            assert_eq!(joined, r#"> you asked "is it free" and I say no"#);
        }

        // trace:BUG-172 | ai:claude
        #[test]
        fn quoted_span_in_interrogator_line_takes_the_user_color() {
            // SYMMETRIC complement of the user-quotes-interrogator case: the
            // interrogator quotes the user, so the quoted run renders in GREEN
            // (the user's color) while the framing stays CYAN.
            let frags = line_fragments(Role::Interrogator, r#"You said "it is free" — did you?"#);
            // Three runs: leading interrogator text, the quoted user span, trailing.
            assert_eq!(frags.len(), 3);
            assert_eq!(frags[0].style.fg, Some(INTERROGATOR));
            assert_eq!(frags[0].text, "You said ");
            assert_eq!(frags[1].style.fg, Some(USER));
            assert_eq!(frags[1].text, r#""it is free""#);
            assert_eq!(frags[2].style.fg, Some(INTERROGATOR));
            assert_eq!(frags[2].text, " — did you?");
            // Reassembling the fragments reproduces the original line exactly.
            let joined: String = frags.iter().map(|f| f.text.as_str()).collect();
            assert_eq!(joined, r#"You said "it is free" — did you?"#);
        }

        // trace:BUG-172 | ai:claude
        #[test]
        fn opposing_role_is_only_defined_for_the_interrogator_user_pair() {
            assert_eq!(opposing_role(Role::User), Some(Role::Interrogator));
            assert_eq!(opposing_role(Role::Interrogator), Some(Role::User));
            assert_eq!(opposing_role(Role::Challenger), None);
            assert_eq!(opposing_role(Role::Meta), None);
            assert_eq!(opposing_role(Role::Plain), None);
        }

        // trace:BUG-172 | ai:claude
        #[test]
        fn quote_attribution_skips_non_paired_voices() {
            // A quoted span in a META / CHALLENGER line has no opposing voice,
            // so it is NOT re-attributed and renders as a single base fragment.
            let meta = line_fragments(Role::Meta, r#"META — they meant "free""#);
            assert_eq!(meta.len(), 1);
            assert_eq!(meta[0].style.fg, Some(META));
            let challenger = line_fragments(Role::Challenger, r#"Challenger: "really?""#);
            assert_eq!(challenger.len(), 1);
            assert_eq!(challenger[0].style.fg, Some(CHALLENGER));
        }

        #[test]
        fn unterminated_quote_runs_to_end_of_line_as_interrogator() {
            let frags = line_fragments(Role::User, r#"> she said "it is so"#);
            // Leading user run, then the unterminated quoted run to EOL.
            assert_eq!(frags.len(), 2);
            assert_eq!(frags[0].style.fg, Some(USER));
            assert_eq!(frags[1].style.fg, Some(INTERROGATOR));
            assert_eq!(frags[1].text, r#""it is so"#);
        }

        // trace:BUG-172 | ai:claude
        #[test]
        fn unterminated_quote_in_interrogator_line_runs_to_end_as_user() {
            let frags = line_fragments(Role::Interrogator, r#"You said "it is so"#);
            // Leading interrogator run, then the unterminated quoted run to EOL
            // in the user's color (the opposing role).
            assert_eq!(frags.len(), 2);
            assert_eq!(frags[0].style.fg, Some(INTERROGATOR));
            assert_eq!(frags[1].style.fg, Some(USER));
            assert_eq!(frags[1].text, r#""it is so"#);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_requires_tty_and_no_no_color() {
        assert!(decide(false, true), "tty + NO_COLOR unset -> color");
        assert!(!decide(true, true), "NO_COLOR set -> plain even on a tty");
        assert!(!decide(false, false), "non-tty -> plain");
        assert!(!decide(true, false), "non-tty + NO_COLOR -> plain");
    }

    #[test]
    fn paint_is_a_noop_when_disabled() {
        assert_eq!(paint_with(false, question(), "Why?"), "Why?");
    }

    #[test]
    fn paint_wraps_text_when_enabled() {
        let painted = paint_with(true, question(), "Why?");
        assert!(painted.contains("Why?"), "original text is preserved");
        assert!(painted.starts_with('\u{1b}'), "leads with an SGR escape");
        assert!(painted.ends_with('m'), "trails with the reset escape");
        assert_ne!(painted, "Why?", "enabled output differs from plain");
    }
}
