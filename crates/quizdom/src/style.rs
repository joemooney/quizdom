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

    // trace:BUG-178 | ai:claude
    /// The single role-AGNOSTIC color for any QUOTED span (single or double,
    /// straight or curly), applied across every voice including META. A soft,
    /// warm YELLOW — deliberately distinct from the gold/amber border+cursor
    /// accent ([`BORDER`]/[`CURSOR`]) and from the role hues (cyan/green/
    /// magenta/blue) so a quotation reads as a quotation, not as chrome.
    /// Belief-neutral: it marks that a span is quoted, never which belief is true.
    pub(crate) const QUOTE: Color = Color::Rgb(0xE6, 0xD2, 0x6B); // soft warm yellow

    // trace:STORY-179 | ai:claude
    /// The distinct color for a markdown HEADING in the transcript. Terminals
    /// have no font size, so headings render bold + this color (top level also
    /// underlined) to read as a title rather than body text.
    pub(crate) const HEADING: Color = Color::Rgb(0x8B, 0xD4, 0xC4); // muted teal

    // trace:STORY-179 | ai:claude
    /// The color for inline code spans and fenced code blocks — a dim, cool
    /// monospace-ish hue so code reads as code, never inline-parsed and never
    /// recolored by the quote-yellow rule.
    pub(crate) const CODE: Color = Color::Rgb(0xB0, 0xB0, 0xC0); // dim slate

    // trace:STORY-179 | ai:claude
    /// The muted color of a blockquote's left `|` bar / indent.
    pub(crate) const BLOCKQUOTE_BAR: Color = Color::DarkGray;

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

    // trace:BUG-178 | ai:claude — the per-line quote-attribution scanner
    // (STORY-171 + BUG-172: `StyledFragment`, `opposing_role`, `line_fragments`,
    // `push_fragment`) is RETIRED here. Quote coloring is now role-agnostic and
    // apostrophe-safe, realized as a pass over the inline text runs inside the
    // markdown renderer ([`crate::markdown::quote_color_runs`], keyed on
    // [`QUOTE`]). The transcript pane renders whole messages through
    // [`crate::markdown::render_lines`] instead of splitting line fragments
    // here. History for the old heuristic lives in BUG-172/STORY-171.

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

        // trace:BUG-178 | ai:claude — quote-coloring tests moved to the markdown
        // renderer (`crate::markdown`), which now owns role-agnostic, apostrophe-
        // safe quote-yellow over inline text runs. The old per-line attribution
        // tests (`line_fragments`/`opposing_role`) were retired with the scanner.
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
