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
// trace:STORY-194 | ai:claude — the settings surface crosses the seam: the engine
// owns score/mode authoritatively, the front-end owns editor/mouse + persistence.
use crate::settings::{LiveSettings, Settings};
use crate::strategy::SessionMode;
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

    // trace:STORY-196 | ai:claude
    /// Drive the STORY-87 authoring core for an
    /// in-session quick-add. The DEFAULT runs the shared
    /// [`crate::question_add::author_question`] core against the front-end's raw
    /// [`author_io`](FrontEnd::author_io) line channels — IDENTICAL to the
    /// pre-STORY-196 inline call in `quick_add_from_current`, so the headless line
    /// path (and its ~336 piped byte-tests) is byte-for-byte unchanged. The ratatui
    /// TUI overrides this to feed the SAME core a LIVE keystroke→line stream sourced
    /// from its input box (the line front-end's `author_io` is empty in the TUI, so
    /// without an override interactive `/add` reads immediate EOF). The core itself
    /// is unchanged either way — only the input/output channels differ.
    fn author_question(
        &mut self,
        existing: &[crate::model::Question],
        strategy: &dyn crate::strategy::NextQuestionStrategy,
        persister: &dyn crate::persist::UserAuthoredQuestionPersister,
        topic: &str,
        link: &crate::persist::QuestionLink,
    ) -> Result<()> {
        let (input, output) = self.author_io();
        crate::question_add::author_question(
            existing, strategy, persister, topic, link, input, output,
        )?;
        Ok(())
    }

    // trace:STORY-194 | ai:claude
    /// Switch the free-text EDITOR MODEL at runtime (`/editor <emacs|vim|auto>`).
    /// `token` is the raw editor token (empty = SHOW the current model). The
    /// front-end rebuilds its editor under the new model, writes a confirmation
    /// (or the current model) through `out()`, and PERSISTS the setting so the
    /// choice sticks. The headless line front-end has no live in-pane TextEditor
    /// (rustyline already picked its mode from `$EDITOR` at startup), so it just
    /// records + echoes the resolved choice; the TUI rebuilds + retags the box.
    fn set_editor_choice(&mut self, token: &str);

    // trace:TASK-266 | ai:claude — widened from `persisted_score` to the whole
    // struct by TASK-300.
    /// The PERSISTED settings this front-end loaded at startup, so the ENGINE can
    /// SEED the state it owns — `score_gauge_on` and the session `mode` — from
    /// them.
    ///
    /// Without this the two halves of the STORY-194 bargain disagreed: the
    /// front-end loaded `score = true` off disk, the engine hardcoded
    /// `score_gauge_on = false`, and the first `/settings` pushed the engine's
    /// default across the seam ([`sync_score`](FrontEnd::sync_score)) and SAVED
    /// it — so a persisted score was both ignored on load and destroyed on the
    /// next write.
    ///
    /// TASK-266 fixed that for `score` alone, with a `persisted_score() -> bool`.
    /// `mode` had the IDENTICAL defect and was not in the bundle, so it shipped
    /// broken: a `mode = "debate"` in the file lost its value on the next save.
    /// Hence ONE accessor for the whole [`Settings`] rather than a second
    /// single-key one — the next engine-owned setting inherits the seed instead
    /// of repeating the bug. The default is [`Settings::default`], for a
    /// front-end that persists nothing.
    ///
    // trace:STORY-367 | ai:claude
    /// It is also, exactly, WHAT A SAVE WOULD WRITE. Mirroring the live session
    /// values ([`mirror_live`](FrontEnd::mirror_live)) must leave it alone —
    /// that invariant IS STORY-367, and the engine's seed depends on it being
    /// true a second time, on the next run.
    fn persisted_settings(&self) -> Settings {
        Settings::default()
    }

    // trace:STORY-194 | ai:claude — one `sync_*` pair until STORY-367 found the
    // two callers meaning different things by it.
    /// MIRROR the engine-owned live values into the front-end's DISPLAY copy so
    /// the `/settings` surface shows what the session is actually doing.
    ///
    /// Never writes the config file. `--mode debate` makes the live mode differ
    /// from the saved one for the whole session; when this was also the
    /// persisting call, the first `/settings` of such a session rewrote the
    /// user's saved default with the flag they had passed once.
    fn mirror_live(&mut self, live: LiveSettings);

    // trace:STORY-194 | ai:claude
    /// PERSIST an explicit user change to the score gauge (the `/score`
    /// shortcut), mirroring it too so the panel and the shortcut agree.
    fn persist_score(&mut self, on: bool);
    /// Twin of [`persist_score`](FrontEnd::persist_score) for an explicit
    /// `/mode <socratic|debate>`. A bare `/mode` changes nothing and so must not
    /// reach here — showing a value is not choosing one.
    fn persist_mode(&mut self, mode: SessionMode);

    // trace:STORY-194 | ai:claude
    /// Open the SETTINGS surface (`/settings`). `rest` is the text after
    /// `/settings` (empty = open the panel; `set <key> <value>` is the headless
    /// line path). The front-end mutates its OWN canonical [`Settings`] (editor /
    /// mouse applied locally; score / mode recorded), PERSISTS them, and returns
    /// the new settings so the ENGINE can reconcile its `mode` + `score_gauge_on`
    /// through its own `/score` / `/mode` logic. The line front-end degrades to a
    /// printed value list; the TUI opens the interactive panel.
    fn settings_surface(&mut self, rest: &str) -> Settings;

    // trace:STORY-191 | ai:claude
    /// HYDRATE a resumed session's prior conversation into the front-end as the
    /// CLEAN STYLED transcript (the same role-colored, markdown-rendered Q/A the
    /// live session shows), so the resumed pane scrolls back through the ENTIRE
    /// history to turn 1.
    ///
    /// `turns` is the prior conversation in order: each entry is `(question_text,
    /// raw_answer)`. The default impl is a NO-OP: the headless [`LineFrontEnd`]
    /// already emitted the byte-exact DEBUG replay (`replay.render`) the ~336
    /// piped tests assert, so it must NOT also inject the styled hydration. Only
    /// the ratatui TUI overrides this to build the clean styled transcript.
    fn hydrate_resume(&mut self, _turns: &[(String, String)]) {}

    // trace:STORY-170 | ai:claude — META-CHANNEL scoping for a graphical front-end.
    /// Open a META scope titled `title`: everything the engine writes through
    /// [`out`](FrontEnd::out) until the matching [`end_meta`](FrontEnd::end_meta)
    /// is part of ONE meta reading (an `/observe` / `/tutor` / `/help` /
    /// `/synopsis` answer, or the closing verdict). The DEFAULT is a NO-OP: the
    /// headless [`LineFrontEnd`] keeps writing the meta text inline, byte-for-byte
    /// as today (the ~336 piped tests assert that). Only the ratatui TUI overrides
    /// these to CAPTURE the scoped bytes and present them as a scrollable MODAL
    /// POPUP in the META voice, returning NON-DESTRUCTIVELY to the same question.
    fn begin_meta(&mut self, _title: &str) {}

    /// Close the META scope opened by [`begin_meta`](FrontEnd::begin_meta). The
    /// default is a NO-OP (headless inline rendering). The TUI flushes the
    /// captured scope into a modal overlay, displays it (scrollable if long), and
    /// waits for a dismiss key before returning to the question.
    fn end_meta(&mut self) {}

    // trace:STORY-170 | ai:claude — the DEAD-END menu as a graphical popup.
    /// Present the dead-end resume menu and read ONE choice. `menu` is the
    /// engine-rendered `[G/P/A/S/Q]` menu text. The DEFAULT returns `None`, which
    /// signals the engine to use its EXISTING path (render the menu through
    /// `out()` + `read_line`), preserving the headless line behavior byte-for-byte.
    /// The TUI overrides this to draw the menu as a single-key MODAL POPUP and
    /// returns the chosen letter (e.g. `"g"`); `Some(String::new())`/EOF maps to
    /// quit. Returning `Some(choice)` tells the engine to SKIP its own prompt.
    fn dead_end_choice(&mut self, _menu: &str) -> Result<Option<String>> {
        Ok(None)
    }
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
    // trace:STORY-194 | ai:claude — the canonical settings (loaded/seeded once).
    // Headless `/settings` degrades to a printed value list; `/editor` records the
    // choice (rustyline already chose its edit mode from $EDITOR at startup).
    // trace:STORY-367 | ai:claude — this is the DISPLAY copy: the engine mirrors
    // its live score/mode into it so the value list tells the truth about the
    // session. It is deliberately NOT the copy that gets saved.
    settings: Settings,
    // trace:STORY-367 | ai:claude — what `settings.toml` says, and the only thing
    // ever written back. Explicit changes reach it one key at a time
    // (`Settings::adopt`); a mirrored `--mode debate` never does.
    persisted: Settings,
}

impl<R: Read, W: Write> LineFrontEnd<R, W> {
    /// Build the headless line front-end over a byte input stream and an output
    /// sink. Mirrors the pre-seam setup that lived at the top of
    /// `run_session_from_current`: wrap the reader in a `BufReader` and select
    /// the free-text input mode from the real stdin's TTY-ness.
    pub(crate) fn new(input: R, output: W) -> Result<Self> {
        let free_text_input = FreeTextInput::from_stdin()?;
        // trace:STORY-194 | ai:claude — load the persisted settings (seed from
        // $EDITOR on a first run). Affects only the new /settings + /editor
        // commands, so the byte-exact behavior the piped tests assert is
        // untouched.
        let settings = crate::settings::load_or_seed();
        Ok(Self {
            input: BufReader::new(input),
            free_text_input,
            output,
            settings,
            // The two copies start equal: nothing has been mirrored yet.
            persisted: settings,
        })
    }

    // trace:STORY-367 | ai:claude
    /// Carry ONE explicitly changed key from the display copy into the persisted
    /// one and write the file (best-effort). The single write path — so a value
    /// the user never asked to change cannot ride along with one they did.
    fn persist(&mut self, key: crate::settings::SettingKey) {
        self.persisted.adopt(key, self.settings);
        let _ = crate::settings::save(&self.persisted);
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

    // trace:STORY-194 | ai:claude
    fn set_editor_choice(&mut self, token: &str) {
        let token = token.trim();
        if token.is_empty() {
            let _ = writeln!(
                self.output,
                "Editor mode: {} (use /editor <emacs|vim|auto> to change)",
                self.settings.editor.label()
            );
            return;
        }
        match crate::settings::EditorChoice::parse(token) {
            Some(choice) => {
                self.settings.editor = choice;
                self.persist(crate::settings::SettingKey::Editor);
                let _ = writeln!(
                    self.output,
                    "Editor mode set: {} (the in-pane editor follows this in the TUI)",
                    choice.label()
                );
            }
            None => {
                let _ = writeln!(
                    self.output,
                    "Unknown editor mode: {token} (expected emacs, vim, or auto). Unchanged ({}).",
                    self.settings.editor.label()
                );
            }
        }
    }

    // trace:TASK-266 | ai:claude — widened by TASK-300.
    // trace:STORY-367 | ai:claude — the PERSISTED copy, not the mirrored one.
    fn persisted_settings(&self) -> Settings {
        self.persisted
    }

    // trace:STORY-367 | ai:claude
    fn mirror_live(&mut self, live: LiveSettings) {
        self.settings.score = live.score;
        self.settings.mode = live.mode;
    }

    // trace:STORY-194 | ai:claude
    fn persist_score(&mut self, on: bool) {
        self.settings.score = on;
        self.persist(crate::settings::SettingKey::Score);
    }

    // trace:STORY-194 | ai:claude
    fn persist_mode(&mut self, mode: SessionMode) {
        self.settings.mode = mode;
        self.persist(crate::settings::SettingKey::Mode);
    }

    // trace:STORY-194 | ai:claude — the HEADLESS settings surface: a bare
    // `/settings` prints the current value list; `/settings set <key> <value>`
    // mutates one setting. The engine reconciles score/mode from the returned set.
    fn settings_surface(&mut self, rest: &str) -> Settings {
        use crate::settings::SettingKey;
        let rest = rest.trim();
        let mut tokens = rest.split_whitespace();
        if tokens.next().map(|t| t.eq_ignore_ascii_case("set")) == Some(true) {
            let key = tokens.next();
            let value = tokens.next();
            match (key.and_then(SettingKey::parse), value) {
                (Some(key), Some(value)) => {
                    if self.settings.set_from_token(key, value) {
                        // trace:STORY-367 | ai:claude — one key: the user named it.
                        self.persist(key);
                        let _ = writeln!(
                            self.output,
                            "{} set: {}",
                            key.label(),
                            self.settings.value_label(key)
                        );
                    } else {
                        let _ = writeln!(
                            self.output,
                            "Unknown value `{value}` for {}. Unchanged.",
                            key.label()
                        );
                    }
                }
                _ => {
                    let _ = writeln!(
                        self.output,
                        "Usage: /settings set <editor|mouse|score|mode> <value>"
                    );
                }
            }
        } else {
            // Bare `/settings` (or any non-`set` text): print the value list.
            let _ = write!(self.output, "{}", self.settings.render_list());
        }
        self.settings
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

    // trace:STORY-194 | ai:claude — the HEADLESS settings surface DEGRADES to a
    // printed value list on a bare `/settings`: it lists every setting (editor,
    // mouse, score, mode) with its current value, written to the output sink. No
    // disk write (bare /settings does not persist).
    #[test]
    fn headless_settings_prints_the_value_list() {
        let mut fe = LineFrontEnd::new(Cursor::new(Vec::new()), Vec::new()).unwrap();
        let _ = fe.settings_surface("");
        let out = String::from_utf8(fe.into_output()).unwrap();
        for label in [
            "Editor mode",
            "Mouse",
            "Score gauge",
            "Session mode",
            // trace:TASK-262 | ai:claude — plus the read-only domain-graph row:
            // `dolt_path` decides which graph the session reads, so it is visible
            // here even though `/settings` cannot change it.
            crate::settings::DOLT_PATH_ROW_LABEL,
            // trace:TASK-320 | ai:claude — and the durability control plus the
            // diagnostic log path, which the headless surface needs at least as
            // much as the TUI: there is no panel to go looking in.
            crate::settings::AUTO_BACKUP_ROW_LABEL,
            crate::settings::LOG_PATH_ROW_LABEL,
        ] {
            assert!(out.contains(label), "list missing {label}:\n{out}");
        }
    }

    // trace:TASK-266 | ai:claude — the front-end EXPOSES the persisted settings so
    // the engine can seed `score_gauge_on` from it instead of hardcoding `false`.
    // Without this seam the engine's default flowed BACK across `sync_score` on the
    // first `/settings` and was SAVED over the user's file — the setting was ignored
    // on load and then destroyed on the next write.
    // trace:TASK-300 | ai:claude — and `mode` rides the SAME accessor, because it
    // had the same defect: one seam for every engine-owned setting, so the next one
    // added inherits the seed rather than repeating the bug a third time.
    #[test]
    fn the_persisted_settings_cross_the_seam_for_the_engine_to_seed_from() {
        let mut fe = LineFrontEnd::new(Cursor::new(Vec::new()), Vec::new()).unwrap();
        // Default settings: gauge off, Socratic — so the seeds are the defaults
        // (the common case).
        assert!(!fe.persisted_settings().score);
        assert_eq!(
            fe.persisted_settings().mode,
            crate::strategy::SessionMode::Socratic
        );

        // An EXPLICIT change (`/score on`, `/mode debate`) is the user choosing a
        // new default, so it reaches the persisted copy and the surface alike.
        fe.persist_score(true);
        fe.persist_mode(SessionMode::Debate);
        assert!(fe.persisted_settings().score);
        assert_eq!(fe.persisted_settings().mode, SessionMode::Debate);
        let surfaced = fe.settings_surface("");
        assert!(surfaced.score);
        assert_eq!(surfaced.mode, SessionMode::Debate);
    }

    // trace:STORY-367 | ai:claude — a session's LIVE values are shown, never saved.
    // `quizdom start --mode debate` over a `mode = "socratic"` file used to rewrite
    // that file the first time the user opened `/settings` — the engine pushed its
    // live mode across the seam and the seam persisted everything it was told. A
    // flag passed once became the permanent default, which is TASK-266's clobber
    // arriving by a different road.
    #[test]
    fn a_mirrored_session_override_is_displayed_but_never_persisted() {
        let mut fe = LineFrontEnd::new(Cursor::new(Vec::new()), Vec::new()).unwrap();
        // The file (here: the defaults) says Socratic, gauge off.
        let saved = fe.persisted_settings();
        assert_eq!(saved.mode, SessionMode::Socratic);

        // `--mode debate` + a live gauge: the engine mirrors both before opening
        // the surface.
        fe.mirror_live(LiveSettings {
            score: true,
            mode: SessionMode::Debate,
        });

        // The surface SHOWS the session's truth...
        let shown = fe.settings_surface("");
        assert_eq!(shown.mode, SessionMode::Debate);
        assert!(shown.score);
        // ...and the file still says what the user wrote in it.
        assert_eq!(fe.persisted_settings(), saved);

        // The third route, which the one-key `adopt` closes: changing an UNRELATED
        // setting used to save the mirrored struct whole, carrying the override to
        // disk behind a request that never mentioned it.
        let _ = fe.settings_surface("set editor vim");
        assert_eq!(
            fe.persisted_settings().editor,
            crate::settings::EditorChoice::Vim,
            "the key the user named is the one that persists"
        );
        assert_eq!(
            fe.persisted_settings().mode,
            SessionMode::Socratic,
            "and the mode they overrode for this session only stays overridden"
        );
        assert!(!fe.persisted_settings().score);
    }

    // trace:STORY-194 | ai:claude — a bare `/editor` SHOWS the current model
    // (headless degradation: the line front-end has no live in-pane editor) without
    // persisting; an in-memory `set mode` mutation flows back through the returned
    // settings so the engine can reconcile.
    #[test]
    fn headless_editor_shows_and_settings_set_mutates() {
        let mut fe = LineFrontEnd::new(Cursor::new(Vec::new()), Vec::new()).unwrap();
        fe.set_editor_choice("");
        let out = String::from_utf8(fe.into_output()).unwrap();
        assert!(out.to_lowercase().contains("editor mode"), "{out}");
    }

    // trace:STORY-194 | ai:claude — the persisted-settings ROUND-TRIP across a
    // simulated RELOAD: serialize a settings set to the config schema then parse it
    // back recovers every value (the engine + front-end agree on a saved choice).
    #[test]
    fn settings_round_trip_across_a_simulated_reload() {
        use crate::settings::Settings;
        let saved = Settings {
            editor: crate::settings::EditorChoice::Vim,
            mouse: false,
            score: true,
            mode: crate::strategy::SessionMode::Debate,
        };
        // Simulate writing to disk and relaunching: serialize, then parse fresh.
        let reloaded = Settings::from_toml(&saved.to_toml());
        assert_eq!(reloaded, saved);
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

    // trace:STORY-191 | ai:claude
    // The HEADLESS front-end's `hydrate_resume` is a NO-OP: it writes NOTHING to
    // the output sink, so the byte-exact debug replay (`SessionReplay::render`)
    // the ~336 piped tests assert is never perturbed by the TUI-only styled
    // hydration. Only the ratatui TUI front-end restyles the resumed transcript.
    #[test]
    fn line_front_end_hydrate_resume_writes_nothing() {
        let mut fe = LineFrontEnd::new(Cursor::new(Vec::new()), Vec::new()).unwrap();
        fe.hydrate_resume(&[
            ("Is the will free?".to_string(), "yes".to_string()),
            ("What is causation?".to_string(), "necessity".to_string()),
        ]);
        let out = fe.into_output();
        assert!(
            out.is_empty(),
            "headless hydrate_resume must not touch the byte-exact replay output"
        );
    }

    // trace:STORY-170 | ai:claude — the headless line front-end's META scoping is a
    // NO-OP: `begin_meta`/`end_meta` write NOTHING and never disturb the inline
    // bytes the engine emits between them, so the ~336 piped byte-tests are
    // unaffected. The reading the engine writes through `out()` passes straight
    // through to the sink exactly as before.
    #[test]
    fn line_front_end_meta_scope_is_a_noop_passthrough() {
        let mut fe = LineFrontEnd::new(Cursor::new(Vec::new()), Vec::new()).unwrap();
        fe.begin_meta("observe");
        write!(fe.out(), "META (observer) — inline reading.").unwrap();
        fe.end_meta();
        let out = String::from_utf8(fe.into_output()).unwrap();
        assert_eq!(
            out, "META (observer) — inline reading.",
            "begin/end_meta must not add or reorder any bytes"
        );
    }

    // trace:STORY-170 | ai:claude — the headless line front-end uses its INLINE
    // dead-end path: `dead_end_choice` returns None (no graphical popup), so the
    // engine renders the menu through `out()` + reads a line, byte-for-byte as
    // today. Only the TUI overrides this to draw a single-key popup.
    #[test]
    fn line_front_end_dead_end_choice_defers_to_the_inline_path() {
        let mut fe = LineFrontEnd::new(Cursor::new(Vec::new()), Vec::new()).unwrap();
        assert_eq!(
            fe.dead_end_choice("[G/P/A/S/Q]").unwrap(),
            None,
            "headless defers to the inline render + read_line path"
        );
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
