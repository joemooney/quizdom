// trace:STORY-168 | ai:claude
//! The FRONT-END seam between the session engine and the outside world.
//!
//! EPIC-167 / ADR-166 split quizdom into a front-end-agnostic ENGINE (the
//! session loop, strategy, observer/help/tutor, synopsis/roundedness, the
//! goal/mode/closing logic) and a small FRONT-END interface the engine talks
//! to. The engine no longer touches stdin/stdout directly: it RENDERS through
//! [`FrontEnd::out`] and REQUESTS input/control through [`FrontEnd::read_answer`]
//! and [`FrontEnd::read_line`].
//!
//! STORY-168 is a pure refactor with NO behavior change. The only front-end it
//! ships is [`LineFrontEnd`], the HEADLESS LINE front-end: it reproduces today's
//! line-based behavior byte-for-byte by delegating to the existing
//! [`crate::input`] readers and writing every render intent to one `Write` sink.
//! It is the front-end used by the ~336 piped/byte tests, by non-TTY / scripted
//! runs, by `--no-tui`, and by the non-interactive standalone commands. The
//! ratatui TUI front-end (STORY-169) becomes a second impl of this same trait.

use crate::error::Result;
use crate::input::{read_answer_or_end, AnswerInput, FreeTextInput, InputContext};
use crate::model::AnswerKind;
// trace:STORY-190 | ai:claude
use crate::palette::PaletteContext;
use std::io::{BufRead, BufReader, Read, Write};

/// The interface the session ENGINE talks to instead of stdin/stdout.
///
/// Two responsibilities:
///
/// * **Render intents** — the engine writes everything it wants the user to see
///   (questions, breadcrumbs, meta-readings, synopses, verdicts, menus) through
///   the [`out`](FrontEnd::out) sink. Keeping a single `Write` behind the trait
///   means a front-end controls *where* output goes (a byte buffer for tests, a
///   real terminal, or — in STORY-169 — a ratatui transcript pane) without the
///   engine knowing.
/// * **Input / control requests** — the engine asks for the next answer-or-control
///   ([`read_answer`](FrontEnd::read_answer)) and for a raw closing-ritual line
///   ([`read_line`](FrontEnd::read_line)). The front-end owns *how* that input is
///   gathered (piped bytes, a rustyline editor, single-key raw mode, or a TUI
///   event loop).
pub(crate) trait FrontEnd {
    /// The render-intent sink. Every `render_*` helper in the engine writes its
    /// bytes here, so a front-end fully controls where session output lands.
    fn out(&mut self) -> &mut dyn Write;

    /// Request the next answer-or-control from the user for a question of the
    /// given [`AnswerKind`] in the given [`InputContext`] (frontier vs review).
    /// Returns the parsed [`AnswerInput`] (an answer, a navigation/control
    /// action, a meta-channel request, or end-of-input).
    ///
    /// `palette_ctx` is the STORY-190 session snapshot the engine populates so a
    /// `/`-opened palette can grey inapplicable commands (e.g. `/judge` without an
    /// open objection). It does not affect TYPED commands — those still route
    /// through the recognizers unchanged.
    fn read_answer(
        &mut self,
        kind: &AnswerKind,
        context: InputContext,
        palette_ctx: PaletteContext,
    ) -> Result<AnswerInput>;

    /// Request a raw line of text with the given prompt (used by the closing
    /// ritual, the dead-end menu, and the term-honing prompts, where the engine
    /// parses the line itself rather than going through the answer recognizers).
    /// `None` signals EOF (a non-TTY stream that ran out, or Ctrl-D) so the
    /// engine can wind down gracefully instead of hanging.
    fn read_line(&mut self, prompt: &str) -> Result<Option<String>>;

    /// Read a line straight off the input stream WITHOUT the interactive
    /// (rustyline) editor and WITHOUT trimming — the term-honing confirmation
    /// reads this way today. Returns the raw line (newline included, as
    /// `BufRead::read_line` yields it) or `None` at EOF. Kept distinct from
    /// [`read_line`](FrontEnd::read_line) so the headless front-end reproduces
    /// today's exact byte behavior for that one prompt.
    fn read_raw_line(&mut self) -> Result<Option<String>>;

    /// Borrow the raw `(BufRead, Write)` channels so the engine can run a NESTED
    /// HEADLESS sub-flow — the in-session quick-add reuses the STORY-87 authoring
    /// core verbatim, which reads many prompts straight off `input` and writes to
    /// `output`. Exposing the line channels keeps that core unchanged (it is also
    /// the standalone `question add` command) while still routing the engine's I/O
    /// through the seam. The line front-end hands back its own channels; a future
    /// TUI front-end implements this by feeding the core from its own input source.
    fn author_io(&mut self) -> (&mut dyn BufRead, &mut dyn Write);
}

/// The HEADLESS LINE front-end: today's behavior, behind the seam.
///
/// Owns the input channels ([`BufReader`] over the byte stream + the
/// [`FreeTextInput`] that picks rustyline-interactive vs plain line reading) and
/// the single output `Write` sink. Every method delegates to the existing
/// [`crate::input`] readers, so the bytes it emits and the lines it reads are
/// IDENTICAL to the pre-seam engine — that is the safety net the ~336 existing
/// tests guard.
pub(crate) struct LineFrontEnd<R: Read, W: Write> {
    input: BufReader<R>,
    free_text_input: FreeTextInput,
    output: W,
}

impl<R: Read, W: Write> LineFrontEnd<R, W> {
    /// Build the headless line front-end over a byte input stream and an output
    /// sink. Mirrors the pre-seam setup that lived at the top of
    /// `run_session_from_current`: wrap the reader in a `BufReader` and select
    /// the free-text input mode from the real stdin's TTY-ness.
    pub(crate) fn new(input: R, output: W) -> Result<Self> {
        let free_text_input = FreeTextInput::from_stdin()?;
        Ok(Self {
            input: BufReader::new(input),
            free_text_input,
            output,
        })
    }

    /// Consume the front-end and hand back the output sink. Standalone commands
    /// that built a line front-end purely to capture bytes can recover the sink
    /// when they are done.
    #[allow(dead_code)]
    pub(crate) fn into_output(self) -> W {
        self.output
    }
}

impl<R: Read, W: Write> FrontEnd for LineFrontEnd<R, W> {
    fn out(&mut self) -> &mut dyn Write {
        &mut self.output
    }

    fn read_answer(
        &mut self,
        kind: &AnswerKind,
        context: InputContext,
        palette_ctx: PaletteContext,
    ) -> Result<AnswerInput> {
        read_answer_or_end(
            kind,
            context,
            palette_ctx,
            &mut self.input,
            &mut self.free_text_input,
            &mut self.output,
        )
    }

    fn read_line(&mut self, prompt: &str) -> Result<Option<String>> {
        self.free_text_input
            .read_line(&mut self.input, &mut self.output, prompt)
    }

    fn read_raw_line(&mut self) -> Result<Option<String>> {
        let mut raw = String::new();
        if self.input.read_line(&mut raw)? == 0 {
            Ok(None)
        } else {
            Ok(Some(raw))
        }
    }

    fn author_io(&mut self) -> (&mut dyn BufRead, &mut dyn Write) {
        (&mut self.input, &mut self.output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // trace:STORY-168 | ai:claude
    // The headless line front-end writes render intents through to its sink
    // byte-for-byte: `out()` is just the underlying Write.
    #[test]
    fn out_writes_through_to_the_sink() {
        let mut fe = LineFrontEnd::new(Cursor::new(Vec::new()), Vec::new()).unwrap();
        write!(fe.out(), "hello {}", 42).unwrap();
        let out = fe.into_output();
        assert_eq!(String::from_utf8(out).unwrap(), "hello 42");
    }

    // trace:STORY-168 | ai:claude
    // read_line returns the trimmed line and signals EOF with None, exactly like
    // the pre-seam free_text_input.read_line it delegates to (non-TTY path).
    #[test]
    fn read_line_reads_then_signals_eof() {
        let mut fe = LineFrontEnd::new(Cursor::new(b"a line\n".to_vec()), Vec::new()).unwrap();
        assert_eq!(fe.read_line("> ").unwrap(), Some("a line".to_string()));
        assert_eq!(fe.read_line("> ").unwrap(), None);
        // The prompt is written to the sink each call (plain, non-TTY mode).
        let out = String::from_utf8(fe.into_output()).unwrap();
        assert_eq!(out, "> > ");
    }

    // trace:STORY-168 | ai:claude
    // read_answer routes a piped answer through the existing recognizers: a `y`
    // on a YesNo question normalizes to the "yes" answer, unchanged from today.
    #[test]
    fn read_answer_routes_through_the_existing_recognizers() {
        let mut fe = LineFrontEnd::new(Cursor::new(b"y\n".to_vec()), Vec::new()).unwrap();
        match fe
            .read_answer(
                &AnswerKind::YesNo,
                InputContext::Frontier,
                PaletteContext::default(),
            )
            .unwrap()
        {
            AnswerInput::Answer(answer) => assert_eq!(answer.normalized, "yes"),
            other => panic!("expected an answer, got {other:?}"),
        }
    }
}
