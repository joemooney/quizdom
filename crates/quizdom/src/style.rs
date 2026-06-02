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
