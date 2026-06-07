// trace:STORY-179 | ai:claude — TUI markdown renderer (inline + block).
// trace:BUG-178  | ai:claude — quote-yellow realized as a pass over inline runs.
//! Render the TUI transcript's plain-text buffer as styled ratatui [`Line`]s,
//! interpreting a useful subset of CommonMark.
//!
//! The interrogator / observer emit markdown — `*going toward*`, `` `code` ``,
//! lists, headings, blockquotes, fenced code. The full-screen TUI front-end
//! renders it instead of showing the literal markers, while the headless
//! [`LineFrontEnd`](crate::frontend) path keeps the literal markdown
//! byte-for-byte (the piped tests assert exact output). Only the TUI calls
//! this module.
//!
//! ## Design
//!
//! We parse with [`pulldown_cmark`] (a pure-Rust CommonMark parser) and walk
//! the event stream OURSELVES to build [`Line`]/[`Span`]s, rather than handing
//! styling to a third-party renderer, so we keep control of three layers that
//! compose on every text run:
//!
//! 1. the line's ROLE base color ([`theme::role_style`], STORY-171) as the
//!    default foreground;
//! 2. inline emphasis / strong / code as MODIFIERS on top of that base
//!    (italic / bold / a code style);
//! 3. the QUOTE-YELLOW rule (BUG-178) — quoted spans inside ordinary text runs
//!    recolor to [`theme::QUOTE`], role-agnostically and apostrophe-safely; it
//!    is NOT applied inside code spans or code blocks.
//!
//! pulldown-cmark handles the CommonMark subtleties we relied on a hand-rolled
//! scanner for before — intra-word `_` is not emphasis, `2 * 3` stays literal,
//! escaped markers stay literal, unterminated markers run as text.
//!
//! ## Line correspondence
//!
//! [`render_lines`] emits EXACTLY one ratatui [`Line`] per source line, so the
//! transcript pane's line-indexed scroll + re-read highlight (STORY-176) keep
//! working unchanged. Block constructs add a prefix (a bullet/number, a
//! blockquote `|` bar, a heading style) to the line they own; fenced code
//! fences are kept (dimmed) rather than dropped so the 1:1 mapping holds.
//! Paragraph reflow to the pane width is left to the `Paragraph` widget's
//! `Wrap`, so wrapping never perturbs the source-line indices either.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::style::theme;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Render the whole transcript buffer (already split into source lines) into one
/// styled [`Line`] per source line. The per-line ROLE is classified exactly as
/// the line front-end attributes voices ([`theme::classify_line`]), so each
/// voice keeps its base color; markdown + quote-yellow layer on top.
///
/// Emits `lines.len()` output lines in order — the 1:1 correspondence the
/// transcript pane's scroll/highlight indices depend on.
pub(crate) fn render_lines(lines: &[String]) -> Vec<Line<'static>> {
    lines
        .iter()
        .map(|line| {
            let role = theme::classify_line(line);
            // Render each source line as its own little markdown document and
            // take its single produced line. A lone line cannot express a
            // multi-line fenced block, but heading / list / blockquote / inline
            // constructs are all single-line, and the buffer is already newline
            // split, so per-line rendering keeps the index mapping exact while
            // still interpreting every per-line construct.
            let base = theme::role_style(role);

            // The user's echoed answer is prefixed with the UI's `> ` marker,
            // which is a transcript convention, NOT markdown — feeding it to
            // pulldown-cmark would parse it as a blockquote. Keep the literal
            // `> ` in the user color and markdown-render only the answer text
            // (so emphasis/quotes inside the user's words still render).
            // trace:BUG-178 | ai:claude
            if role == theme::Role::User {
                if let Some(answer) = line.strip_prefix("> ") {
                    let mut spans: Vec<Span<'static>> = vec![Span::styled("> ".to_string(), base)];
                    let mut produced = render_message(base, answer);
                    if let Some(first) = produced.pop() {
                        spans.extend(first.spans);
                    }
                    return Line::from(spans);
                }
                // A bare ">" (empty answer) or other user line: render literally.
                return Line::from(Span::styled(line.clone(), base));
            }

            let mut produced = render_message(base, line);
            // Exactly one line per source line: a blank source line yields an
            // empty paragraph (no events) — emit an empty styled line for it.
            match produced.len() {
                0 => Line::from(Span::styled(String::new(), base)),
                1 => produced.pop().expect("len checked"),
                _ => {
                    // Defensive: a single source line should not span multiple
                    // rendered lines, but if it ever does, flatten the spans so
                    // the 1:1 mapping is preserved.
                    let spans: Vec<Span<'static>> =
                        produced.into_iter().flat_map(|l| l.spans).collect();
                    Line::from(spans)
                }
            }
        })
        .collect()
}

/// Render one transcript MESSAGE (a possibly multi-line markdown string) into
/// styled ratatui [`Line`]s, with `base` as the role's base foreground style.
///
/// This is the full inline+block renderer; [`render_lines`] drives it per
/// source line to keep the pane's index mapping, but tests exercise whole
/// messages (lists, headings, blockquotes, fenced code) through here directly.
pub(crate) fn render_message(base: Style, text: &str) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    // Only the constructs we render; keep the grammar small + predictable.
    let parser = Parser::new_ext(text, opts_for(&mut opts));
    let mut r = Renderer::new(base);
    for event in parser {
        r.event(event);
    }
    r.finish()
}

/// The CommonMark options we enable. Kept as a helper so `render_message` reads
/// cleanly; we deliberately stay close to vanilla CommonMark.
fn opts_for(opts: &mut Options) -> Options {
    // No GFM extensions: we want predictable, minimal markdown matching what the
    // interrogator/observer emit. (Strikethrough/tables/etc. would surprise.)
    *opts
}

/// The walker that turns a CommonMark event stream into styled lines, applying
/// role base color + inline modifiers + quote-yellow.
struct Renderer {
    base: Style,
    lines: Vec<Line<'static>>,
    /// Spans accumulated for the line currently being built.
    current: Vec<Span<'static>>,
    /// Inline modifier stack (emphasis / strong) currently in effect.
    emphasis: u32,
    strong: u32,
    /// Heading level in effect, if inside a heading.
    heading: Option<HeadingLevel>,
    /// Blockquote nesting depth (for the `|` bar + indent).
    blockquote_depth: u32,
    /// Whether we are inside a fenced/indented code block.
    in_code_block: bool,
    /// The list marker stack: `None` for a bullet list, `Some(n)` for the next
    /// ordinal in an ordered list.
    lists: Vec<Option<u64>>,
    /// Whether the current paragraph/heading has emitted any text yet (so we
    /// only prefix the first line of a list item once).
    item_pending_prefix: Option<String>,
}

impl Renderer {
    fn new(base: Style) -> Self {
        Self {
            base,
            lines: Vec::new(),
            current: Vec::new(),
            emphasis: 0,
            strong: 0,
            heading: None,
            blockquote_depth: 0,
            in_code_block: false,
            lists: Vec::new(),
            item_pending_prefix: None,
        }
    }

    /// The style for ordinary (non-code) inline text at the current nesting:
    /// the role base color plus any active emphasis/strong/heading modifiers.
    fn text_style(&self) -> Style {
        let mut style = self.base;
        if self.emphasis > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strong > 0 {
            style = style.add_modifier(Modifier::BOLD);
        }
        if let Some(level) = self.heading {
            // Headings: bold + a distinct heading color (terminals have no font
            // size). Underline the top level so it reads as a title.
            style = style.fg(theme::HEADING).add_modifier(Modifier::BOLD);
            if matches!(level, HeadingLevel::H1) {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
        }
        style
    }

    /// The style for an inline code span / code block: a dim monospace-ish code
    /// color, never recolored by quote-yellow.
    fn code_style(&self) -> Style {
        self.base.fg(theme::CODE).add_modifier(Modifier::DIM)
    }

    /// Flush the current span buffer as a finished line, prefixing any pending
    /// block decoration (blockquote bar, list marker) first.
    fn flush_line(&mut self) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        // Blockquote bar(s) + indent, one muted `|` per nesting level.
        for _ in 0..self.blockquote_depth {
            spans.push(Span::styled(
                "\u{2502} ".to_string(),
                Style::default().fg(theme::BLOCKQUOTE_BAR),
            ));
        }
        // A list item's marker (bullet / number) + hanging indent for nesting.
        if let Some(prefix) = self.item_pending_prefix.take() {
            // Hanging indent: deeper lists indent two spaces per ancestor.
            let indent = "  ".repeat(self.lists.len().saturating_sub(1));
            spans.push(Span::styled(format!("{indent}{prefix}"), self.base));
        }
        spans.append(&mut self.current);
        // A flushed line with no content at all still occupies a row.
        self.lines.push(Line::from(spans));
    }

    /// Push a finished inline text run, applying quote-yellow if it is ordinary
    /// text (not inside a code block). Inline code spans arrive as
    /// [`Event::Code`] and are styled directly, so they never reach here.
    fn push_text(&mut self, text: &str) {
        if self.in_code_block {
            self.current
                .push(Span::styled(text.to_string(), self.code_style()));
            return;
        }
        let style = self.text_style();
        for span in quote_color_runs(text, style) {
            self.current.push(span);
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if self.in_code_block {
                    // Fenced/indented code: each physical line is its own row,
                    // not inline-parsed, dim monospace.
                    self.push_code_block_text(&text);
                } else {
                    self.push_text(&text);
                }
            }
            Event::Code(code) => {
                // An inline code span: code style, no quote-yellow.
                self.current
                    .push(Span::styled(code.to_string(), self.code_style()));
            }
            Event::SoftBreak | Event::HardBreak => {
                // Within a paragraph, treat a break as a space so reflow can wrap
                // (the `Paragraph` widget owns width-wrapping).
                if self.in_code_block {
                    self.flush_line();
                } else {
                    self.push_text(" ");
                }
            }
            // We render no HTML / footnotes / rules specially: surface as text.
            Event::Html(t) | Event::InlineHtml(t) => self.push_text(&t),
            Event::Rule => {
                self.push_text("\u{2500}\u{2500}\u{2500}");
            }
            Event::TaskListMarker(done) => {
                self.push_text(if done { "[x] " } else { "[ ] " });
            }
            Event::FootnoteReference(t) => self.push_text(&t),
            Event::InlineMath(t) | Event::DisplayMath(t) => self.push_text(&t),
        }
    }

    /// Emit fenced-code text, splitting on its embedded newlines so each code
    /// line becomes its own row (dim monospace; never inline-parsed).
    fn push_code_block_text(&mut self, text: &str) {
        let mut first = true;
        for piece in text.split('\n') {
            if !first {
                self.flush_line();
            }
            first = false;
            if !piece.is_empty() {
                self.current
                    .push(Span::styled(piece.to_string(), self.code_style()));
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => self.heading = Some(level),
            Tag::BlockQuote(_) => self.blockquote_depth += 1,
            Tag::CodeBlock(kind) => {
                self.in_code_block = true;
                // A fenced block opens its own row(s); the opening fence line is
                // not emitted as text by pulldown-cmark, so the dim block starts
                // at the first content line.
                let _ = kind_label(&kind);
            }
            Tag::List(start) => self.lists.push(start),
            Tag::Item => {
                // Compute this item's marker now; it is prefixed onto the first
                // line the item flushes.
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *self.lists.last_mut().expect("list present") = Some(*n + 1);
                        m
                    }
                    _ => "\u{2022} ".to_string(), // bullet
                };
                self.item_pending_prefix = Some(marker);
            }
            // Emphasis / strong nest as modifier counters.
            Tag::Emphasis => self.emphasis += 1,
            Tag::Strong => self.strong += 1,
            // Links / images: render the visible text, drop the URL chrome.
            Tag::Link { .. } | Tag::Image { .. } => {}
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_line(),
            TagEnd::Heading(_) => {
                self.heading = None;
                self.flush_line();
            }
            TagEnd::BlockQuote(_) => {
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                self.flush_line();
                self.in_code_block = false;
            }
            TagEnd::List(_) => {
                self.lists.pop();
            }
            TagEnd::Item => {
                // If an item produced no paragraph (tight list), its inline text
                // is still pending in `current`; flush it as the item's line.
                if !self.current.is_empty() || self.item_pending_prefix.is_some() {
                    self.flush_line();
                }
            }
            TagEnd::Emphasis => self.emphasis = self.emphasis.saturating_sub(1),
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            _ => {}
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        // Any trailing text not closed by a block end (shouldn't happen for
        // well-formed CommonMark, but be safe).
        if !self.current.is_empty() {
            self.flush_line();
        }
        self.lines
    }
}

/// A human label for a fenced code block's info string (currently unused for
/// rendering, but kept so a future "language tag" chip is a one-liner).
fn kind_label(kind: &CodeBlockKind<'_>) -> Option<String> {
    match kind {
        CodeBlockKind::Fenced(info) if !info.is_empty() => Some(info.to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Quote-yellow (BUG-178): an apostrophe-safe pass over an ordinary text run.
// ---------------------------------------------------------------------------

/// Split `text` into styled [`Span`]s, recoloring quoted spans to
/// [`theme::QUOTE`] while leaving the rest in `base`. Operates only on ordinary
/// text runs (callers never pass code-span/code-block text here), so a quote
/// inside backticks is never colorized.
///
/// Detection (role-agnostic, apostrophe-safe — BUG-178):
/// - DOUBLE quotes (straight `"`, curly `“ ”`): colorize the span between the
///   matched pair; unambiguous.
/// - SINGLE quotes (straight `'`, curly `‘ ’`): open a span ONLY when the
///   opening quote is at run-start or preceded by whitespace / `( [ { : , -`,
///   AND a matching closing quote is followed by whitespace / punctuation /
///   end. This excludes intra-word apostrophes (don't, it's, climber's) which
///   are letter-flanked.
/// - Multiple quoted spans each colorize independently; an unterminated quote
///   runs to the end of the run gracefully.
///
/// The opening/closing quote characters are INCLUDED in the colorized span.
pub(crate) fn quote_color_runs(text: &str, base: Style) -> Vec<Span<'static>> {
    let quote_style = base.fg(theme::QUOTE);
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        if let Some(close) = matching_close(ch) {
            if quote_opens(&chars, i, ch) {
                if let Some(end) = find_close(&chars, i + 1, ch, close) {
                    // Flush the plain run, then the colorized quoted span
                    // (inclusive of both quote chars).
                    flush_plain(&mut spans, &mut plain, base);
                    let quoted: String = chars[i..=end].iter().collect();
                    spans.push(Span::styled(quoted, quote_style));
                    i = end + 1;
                    continue;
                }
                // No matching close before run-end: an unterminated quote runs
                // to end of run, colorized.
                flush_plain(&mut spans, &mut plain, base);
                let quoted: String = chars[i..].iter().collect();
                spans.push(Span::styled(quoted, quote_style));
                return spans;
            }
        }
        plain.push(ch);
        i += 1;
    }
    flush_plain(&mut spans, &mut plain, base);
    if spans.is_empty() {
        spans.push(Span::styled(text.to_string(), base));
    }
    spans
}

fn flush_plain(spans: &mut Vec<Span<'static>>, plain: &mut String, base: Style) {
    if !plain.is_empty() {
        spans.push(Span::styled(std::mem::take(plain), base));
    }
}

/// The closing-quote char that matches an opening `ch`, if `ch` can open a
/// quote span. Straight quotes are their own close; curly quotes pair.
fn matching_close(ch: char) -> Option<char> {
    match ch {
        '"' => Some('"'),
        '\u{201C}' => Some('\u{201D}'), // “ ”
        '\'' => Some('\''),
        '\u{2018}' => Some('\u{2019}'), // ‘ ’
        _ => None,
    }
}

/// Whether the character at `i` legitimately OPENS a quote span.
///
/// Double + curly-double quotes always open (unambiguous). Single + curly-single
/// quotes open only when the preceding char is run-start / whitespace / one of
/// `( [ { : , -` — so intra-word apostrophes (letter-preceded) never open.
fn quote_opens(chars: &[char], i: usize, ch: char) -> bool {
    let is_single = matches!(ch, '\'' | '\u{2018}');
    if !is_single {
        // Double / curly-double always open.
        return true;
    }
    match i.checked_sub(1).map(|p| chars[p]) {
        None => true, // run-start
        Some(prev) => prev.is_whitespace() || matches!(prev, '(' | '[' | '{' | ':' | ',' | '-'),
    }
}

/// Find the matching CLOSING quote for an opener of char `open` (whose close is
/// `close`), starting the search at index `from`. For single quotes the close
/// is accepted only when it is followed by whitespace / punctuation / run-end
/// (so it cannot be an intra-word apostrophe). Returns the close index.
fn find_close(chars: &[char], from: usize, open: char, close: char) -> Option<usize> {
    let is_single = matches!(open, '\'' | '\u{2018}');
    let mut j = from;
    while j < chars.len() {
        if chars[j] == close {
            if !is_single {
                return Some(j);
            }
            // Single-quote close must be followed by whitespace / punctuation /
            // end so a possessive/contraction apostrophe doesn't close a span.
            let ok = match chars.get(j + 1) {
                None => true,
                Some(&next) => next.is_whitespace() || is_closing_punct(next),
            };
            if ok {
                return Some(j);
            }
        }
        j += 1;
    }
    None
}

/// Punctuation that may legitimately follow a closing single quote.
fn is_closing_punct(ch: char) -> bool {
    matches!(
        ch,
        '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '-' | '\u{2014}' | '\u{2013}'
    )
}

/// Convenience used by tests: collect the visible text of a line.
#[cfg(test)]
fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Convenience used by tests: does any span of `line` carry `color` as its fg?
#[cfg(test)]
fn has_fg(line: &Line<'_>, color: ratatui::style::Color) -> bool {
    line.spans.iter().any(|s| s.style.fg == Some(color))
}

#[cfg(test)]
#[allow(clippy::needless_borrow)]
mod tests {
    use super::*;
    use crate::style::theme;
    use ratatui::style::{Modifier, Style};

    fn cyan() -> Style {
        theme::role_style(theme::Role::Interrogator)
    }

    // ---- inline emphasis / strong / code ---------------------------------

    // trace:STORY-179 | ai:claude
    #[test]
    fn emphasis_renders_italic_with_markers_hidden_in_role_color() {
        let lines = render_message(cyan(), "I am *going toward* it");
        assert_eq!(lines.len(), 1);
        let text = line_text(&lines[0]);
        assert_eq!(text, "I am going toward it", "the * markers are hidden");
        // The emphasized run carries ITALIC on top of the interrogator's cyan.
        let italic = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("going toward"))
            .expect("emphasized span present");
        assert!(italic.style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(italic.style.fg, Some(theme::INTERROGATOR));
    }

    // trace:STORY-179 | ai:claude
    #[test]
    fn strong_renders_bold_markers_hidden() {
        let lines = render_message(cyan(), "this is **very** important");
        let text = line_text(&lines[0]);
        assert_eq!(text, "this is very important");
        let bold = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("very"))
            .expect("strong span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    // trace:STORY-179 | ai:claude
    #[test]
    fn inline_code_renders_code_style_markers_hidden() {
        let lines = render_message(cyan(), "call `do_it()` now");
        let text = line_text(&lines[0]);
        assert_eq!(text, "call do_it() now", "backticks hidden");
        let code = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("do_it"))
            .expect("code span");
        assert_eq!(code.style.fg, Some(theme::CODE));
        assert!(code.style.add_modifier.contains(Modifier::DIM));
    }

    // ---- CommonMark literal safety ---------------------------------------

    // trace:STORY-179 | ai:claude
    #[test]
    fn snake_case_underscores_stay_literal() {
        let lines = render_message(cyan(), "the variable my_long_name is fine");
        let text = line_text(&lines[0]);
        assert_eq!(text, "the variable my_long_name is fine");
        // No italic anywhere — the intra-word underscores are not emphasis.
        assert!(lines[0]
            .spans
            .iter()
            .all(|s| !s.style.add_modifier.contains(Modifier::ITALIC)));
    }

    // trace:STORY-179 | ai:claude
    #[test]
    fn spaced_asterisks_stay_literal() {
        let lines = render_message(cyan(), "compute 2 * 3 * 4 here");
        let text = line_text(&lines[0]);
        assert_eq!(text, "compute 2 * 3 * 4 here");
        assert!(lines[0]
            .spans
            .iter()
            .all(|s| !s.style.add_modifier.contains(Modifier::ITALIC)));
    }

    // trace:STORY-179 | ai:claude
    #[test]
    fn escaped_marker_stays_literal() {
        let lines = render_message(cyan(), r"a literal \*star\* here");
        let text = line_text(&lines[0]);
        assert_eq!(text, "a literal *star* here");
        assert!(lines[0]
            .spans
            .iter()
            .all(|s| !s.style.add_modifier.contains(Modifier::ITALIC)));
    }

    // ---- block: list / heading / blockquote / code fence -----------------

    // trace:STORY-179 | ai:claude
    #[test]
    fn bullet_list_renders_marker_and_hanging_indent() {
        let lines = render_message(cyan(), "- first\n- second");
        assert_eq!(lines.len(), 2);
        assert!(line_text(&lines[0]).starts_with("\u{2022} "));
        assert!(line_text(&lines[0]).contains("first"));
        assert!(line_text(&lines[1]).contains("second"));
    }

    // trace:STORY-179 | ai:claude
    #[test]
    fn numbered_list_renders_incrementing_markers() {
        let lines = render_message(cyan(), "1. alpha\n2. beta\n3. gamma");
        assert_eq!(lines.len(), 3);
        assert!(line_text(&lines[0]).starts_with("1. "));
        assert!(line_text(&lines[1]).starts_with("2. "));
        assert!(line_text(&lines[2]).starts_with("3. "));
    }

    // trace:STORY-179 | ai:claude
    #[test]
    fn heading_renders_bold_in_heading_color() {
        let lines = render_message(cyan(), "# A Title");
        assert_eq!(lines.len(), 1);
        assert_eq!(line_text(&lines[0]), "A Title", "the # marker is hidden");
        let span = &lines[0].spans[0];
        assert_eq!(span.style.fg, Some(theme::HEADING));
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
    }

    // trace:STORY-179 | ai:claude
    #[test]
    fn blockquote_renders_indent_bar() {
        let lines = render_message(cyan(), "> a quoted thought");
        assert_eq!(lines.len(), 1);
        // The first span is the muted bar.
        assert!(lines[0].spans[0].content.contains('\u{2502}'));
        assert_eq!(lines[0].spans[0].style.fg, Some(theme::BLOCKQUOTE_BAR));
        assert!(line_text(&lines[0]).contains("a quoted thought"));
    }

    // trace:STORY-179 | ai:claude
    #[test]
    fn fenced_code_block_renders_dim_block_not_inline_parsed() {
        let msg = "```\nlet x = *y*;\nmore_code();\n```";
        let lines = render_message(cyan(), msg);
        // Two content lines, both dim code color; the `*y*` is NOT italicized.
        let joined: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains("let x = *y*;"),
            "asterisks stay literal in code"
        );
        assert!(lines.iter().all(|l| l
            .spans
            .iter()
            .all(|s| !s.style.add_modifier.contains(Modifier::ITALIC))));
        let has_code = lines.iter().any(|l| has_fg(l, theme::CODE));
        assert!(has_code, "code block carries the code color");
    }

    // ---- quote-yellow (BUG-178) ------------------------------------------

    // trace:BUG-178 | ai:claude
    #[test]
    fn double_quoted_span_is_quote_colored() {
        let spans = quote_color_runs(r#"he said "hello" to me"#, cyan());
        let quoted = spans
            .iter()
            .find(|s| s.content.contains("hello"))
            .expect("quoted span");
        assert_eq!(quoted.style.fg, Some(theme::QUOTE));
        assert_eq!(quoted.content.as_ref(), r#""hello""#);
        // Surrounding text keeps the base (interrogator) color.
        assert!(spans
            .iter()
            .any(|s| s.content.contains("he said") && s.style.fg == Some(theme::INTERROGATOR)));
    }

    // trace:BUG-178 | ai:claude
    #[test]
    fn single_quoted_span_is_quote_colored() {
        let spans = quote_color_runs("the word 'free' matters", cyan());
        let quoted = spans
            .iter()
            .find(|s| s.content.contains("free"))
            .expect("quoted span");
        assert_eq!(quoted.style.fg, Some(theme::QUOTE));
        assert_eq!(quoted.content.as_ref(), "'free'");
    }

    // trace:BUG-178 | ai:claude
    #[test]
    fn curly_quotes_are_quote_colored() {
        let dbl = quote_color_runs("a \u{201C}curly\u{201D} word", cyan());
        assert!(dbl
            .iter()
            .any(|s| s.content.contains("curly") && s.style.fg == Some(theme::QUOTE)));
        let sgl = quote_color_runs("a \u{2018}curly\u{2019} word", cyan());
        assert!(sgl
            .iter()
            .any(|s| s.content.contains("curly") && s.style.fg == Some(theme::QUOTE)));
    }

    // trace:BUG-178 | ai:claude
    #[test]
    fn contraction_apostrophe_is_not_colorized() {
        for text in [
            "don't worry",
            "it's fine",
            "the climber's grip",
            "life's work",
        ] {
            let spans = quote_color_runs(text, cyan());
            assert!(
                spans.iter().all(|s| s.style.fg != Some(theme::QUOTE)),
                "no quote color in {text:?}"
            );
        }
    }

    // trace:BUG-178 | ai:claude
    #[test]
    fn multiple_quoted_spans_each_colorize() {
        let spans = quote_color_runs(r#"both "one" and "two" here"#, cyan());
        let colored: Vec<_> = spans
            .iter()
            .filter(|s| s.style.fg == Some(theme::QUOTE))
            .collect();
        assert_eq!(colored.len(), 2);
        assert_eq!(colored[0].content.as_ref(), r#""one""#);
        assert_eq!(colored[1].content.as_ref(), r#""two""#);
    }

    // trace:BUG-178 | ai:claude
    #[test]
    fn unterminated_quote_runs_to_end() {
        let spans = quote_color_runs(r#"she said "it goes on"#, cyan());
        let last = spans.last().expect("a span");
        assert_eq!(last.style.fg, Some(theme::QUOTE));
        assert!(last.content.contains("it goes on"));
    }

    // trace:BUG-178 | ai:claude — the OBSERVED example from the bug report.
    #[test]
    fn observed_example_colorizes_both_single_quoted_spans() {
        let line = "You've replaced the verdict with a hope: not 'I pronounce my life well \
                    lived' but 'I hope that verdict is within reach'";
        let spans = quote_color_runs(line, theme::meta_style());
        let colored: Vec<_> = spans
            .iter()
            .filter(|s| s.style.fg == Some(theme::QUOTE))
            .collect();
        assert_eq!(colored.len(), 2, "both single-quoted spans colorize");
        assert_eq!(
            colored[0].content.as_ref(),
            "'I pronounce my life well lived'"
        );
        assert_eq!(
            colored[1].content.as_ref(),
            "'I hope that verdict is within reach'"
        );
        // The leading contraction "You've" must NOT have started a span.
        assert!(spans
            .iter()
            .any(|s| s.content.contains("You've") && s.style.fg != Some(theme::QUOTE)));
    }

    // trace:BUG-178 | ai:claude — quote color is role-agnostic (META included).
    #[test]
    fn quote_color_applies_regardless_of_role() {
        for role in [
            theme::Role::Interrogator,
            theme::Role::User,
            theme::Role::Challenger,
            theme::Role::Meta,
            theme::Role::Plain,
        ] {
            let base = theme::role_style(role);
            let spans = quote_color_runs(r#"a "quote" here"#, base);
            assert!(
                spans
                    .iter()
                    .any(|s| s.content.contains("quote") && s.style.fg == Some(theme::QUOTE)),
                "quote colorized for role {role:?}"
            );
        }
    }

    // ---- composition: role + quote-yellow + inline emphasis --------------

    // trace:STORY-179 | ai:claude
    // trace:BUG-178  | ai:claude
    #[test]
    fn composition_role_quote_and_emphasis_layer() {
        let lines = render_message(cyan(), r#"consider *this* and "that" together"#);
        assert_eq!(lines.len(), 1);
        let spans = &lines[0].spans;
        // emphasized "this" -> italic + cyan
        assert!(spans.iter().any(|s| s.content.contains("this")
            && s.style.add_modifier.contains(Modifier::ITALIC)
            && s.style.fg == Some(theme::INTERROGATOR)));
        // quoted "that" -> quote-yellow on top of the same base
        assert!(spans
            .iter()
            .any(|s| s.content.contains("that") && s.style.fg == Some(theme::QUOTE)));
    }

    // trace:STORY-179 | ai:claude
    // trace:BUG-178  | ai:claude — quote-yellow is NOT applied inside code spans.
    #[test]
    fn quote_inside_code_span_is_not_quote_colored() {
        let lines = render_message(cyan(), r#"run `say "hi"` please"#);
        let spans = &lines[0].spans;
        // The code span keeps the code color, never quote-yellow.
        let code = spans
            .iter()
            .find(|s| s.content.contains("hi"))
            .expect("code span with the quote inside");
        assert_eq!(code.style.fg, Some(theme::CODE));
        assert!(spans.iter().all(|s| {
            // No span containing the inner quote got quote-yellow.
            !(s.content.contains("hi") && s.style.fg == Some(theme::QUOTE))
        }));
    }

    // ---- wrap / reflow smoke + render_lines mapping ----------------------

    // trace:STORY-179 | ai:claude
    #[test]
    fn long_paragraph_reflows_without_crashing() {
        let para = "word ".repeat(200);
        let lines = render_message(cyan(), &para);
        // A single paragraph stays one logical line; width-wrapping is the
        // Paragraph widget's job (Wrap), so this must not panic and must
        // preserve the text.
        assert_eq!(lines.len(), 1);
        assert!(line_text(&lines[0]).contains("word word"));
    }

    // trace:STORY-179 | ai:claude — render_lines keeps a 1:1 source-line mapping.
    #[test]
    fn render_lines_emits_one_line_per_source_line() {
        let src = vec![
            "Is your will *free*?".to_string(),
            String::new(),
            "> I said \"yes\"".to_string(),
            "META — a reading".to_string(),
        ];
        let out = render_lines(&src);
        assert_eq!(out.len(), src.len(), "one rendered line per source line");
        // The interrogator line keeps cyan + italic on the emphasized word.
        assert!(out[0].spans.iter().any(|s| s.content.contains("free")
            && s.style.add_modifier.contains(Modifier::ITALIC)
            && s.style.fg == Some(theme::INTERROGATOR)));
        // The user line's quoted span is quote-yellow.
        assert!(out[2]
            .spans
            .iter()
            .any(|s| s.content.contains("yes") && s.style.fg == Some(theme::QUOTE)));
        // The META line keeps the META color as its base.
        assert!(out[3].spans.iter().any(|s| s.style.fg == Some(theme::META)));
    }
}
