use crate::error::{QuizdomError, Result};
use crate::model::{Answer, AnswerKind, Question};
// trace:STORY-163 | ai:claude
use crate::palette;
use crate::style;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use rustyline::{Config as RustylineConfig, DefaultEditor, EditMode};
use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

// trace:STORY-69 | ai:codex
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum InputContext {
    Frontier,
    Review,
}

pub(crate) fn render_question(question: &Question, output: &mut dyn Write) -> Result<()> {
    render_question_for(question, InputContext::Frontier, output)
}

// trace:STORY-78 | ai:claude
/// Render the in-session orientation breadcrumb shown each frontier turn:
/// the current topic, exploration depth, and active branch. Keeping it on one
/// compact dimmed line keeps the user oriented in a long session without
/// crowding the question itself. The breadcrumb funnels through the same
/// styling gate as everything else, so non-TTY / `NO_COLOR` / test output stays
/// plain text.
pub(crate) fn render_breadcrumb(
    question: &Question,
    depth: usize,
    branch_id: &str,
    // trace:STORY-159 | ai:claude — the live session goal, shown in the
    // breadcrumb so the user always sees the thesis they are orienting toward.
    goal: Option<&str>,
    output: &mut dyn Write,
) -> Result<()> {
    let line = breadcrumb_line(question, depth, branch_id, goal);
    writeln!(output, "{}", style::paint(style::breadcrumb(), &line))?;
    Ok(())
}

// trace:STORY-78 | ai:claude
/// Pure formatter behind [`render_breadcrumb`], split out so the breadcrumb's
/// content is unit-testable without a buffer or the styling global.
// trace:STORY-159 | ai:claude — when a goal is set it is appended as its own
/// breadcrumb segment (`| goal: ...`); a free-flowing session omits the segment
/// entirely so the breadcrumb stays compact until a goal exists.
pub(crate) fn breadcrumb_line(
    question: &Question,
    depth: usize,
    branch_id: &str,
    goal: Option<&str>,
) -> String {
    let mut line = format!(
        "[topic: {} | depth: {} | branch: {}",
        breadcrumb_topic(question),
        depth,
        branch_id
    );
    if let Some(goal) = goal.map(str::trim).filter(|goal| !goal.is_empty()) {
        line.push_str(&format!(" | goal: {goal}"));
    }
    line.push(']');
    line
}

// trace:STORY-78 | ai:claude
/// The human-facing topic for the breadcrumb, read from the question's
/// `topic:<slug>` tag (dashes rendered as spaces). Untagged questions — e.g. a
/// runtime-minted contradiction prompt — fall back to a stable placeholder so
/// the breadcrumb never disappears mid-session.
fn breadcrumb_topic(question: &Question) -> String {
    question
        .tags
        .iter()
        .find_map(|tag| tag.strip_prefix("topic:"))
        .map(str::trim)
        .filter(|topic| !topic.is_empty())
        .map(|topic| topic.replace('-', " "))
        .unwrap_or_else(|| "(general)".to_string())
}

pub(crate) fn render_question_for(
    question: &Question,
    context: InputContext,
    output: &mut dyn Write,
) -> Result<()> {
    // trace:STORY-76 | ai:claude — a surfaced contradiction reuses this
    // renderer; style its prompt distinctly so the tension reads as a flag,
    // not just another question.
    let title_style = if question
        .tags
        .iter()
        .any(|tag| tag == "runtime:contradiction")
    {
        style::contradiction()
    } else {
        style::question()
    };
    writeln!(output, "\n{}", style::paint(title_style, &question.title))?;
    match &question.answer_kind {
        AnswerKind::YesNo => writeln!(
            output,
            "{}",
            style::paint(
                style::control(),
                &control_prompt("[Y] Yes  [N] No", context)
            )
        )?,
        AnswerKind::Choice(options) => {
            for (index, option) in options.iter().enumerate() {
                writeln!(
                    output,
                    "{} {}",
                    style::paint(style::option(), &format!("{}.", index + 1)),
                    option
                )?;
            }
            writeln!(
                output,
                "{}",
                style::paint(
                    style::control(),
                    &control_prompt(&format!("[1-{}] Choose", options.len()), context)
                )
            )?;
        }
        // trace:BUG-98 | ai:claude — free-text is rustyline line-mode
        // (STORY-55), so single keys can't be intercepted mid-edit. Display the
        // same control set as the single-key prompt, but as slash-commands the
        // line parser recognizes, so navigation is consistent across all kinds.
        AnswerKind::FreeText => writeln!(
            output,
            "Answer in your own words, or {}",
            free_text_controls(context)
        )?,
    }
    write!(output, "> ")?;
    output.flush()?;
    Ok(())
}

fn control_prompt(prefix: &str, context: InputContext) -> String {
    match context {
        // trace:STORY-88 | ai:claude — the quick-add control is offered only at
        // the frontier, where "add a question from here" is meaningful; the
        // review pane is for revising the saved path, not authoring.
        // trace:STORY-127 | ai:claude — the observer control ('?') is offered in
        // both contexts: it is non-destructive, so reading the exchange and
        // returning to the same prompt is always safe.
        // trace:STORY-128 | ai:claude — `[S] Synopsis` joins the observer
        // controls in both contexts: it is non-destructive, so a whole-session
        // reading and a return to the same prompt is always safe.
        // trace:STORY-159 | ai:claude — `/goal <text>` joins the controls; it has
        // no single key because it takes free-text, so it is shown as the typed
        // command form alongside the single-key set.
        // trace:STORY-160 | ai:claude — `/rest` (rest your case) joins the typed
        // controls; it begins the closing ritual, so it is shown alongside `/goal`
        // rather than taking a single key.
        // trace:STORY-161 | ai:claude — `/mode <socratic|debate>` joins the typed
        // controls; it toggles the questioning stance, so it is shown alongside
        // `/goal` rather than taking a single key.
        // trace:STORY-163 | ai:claude — `/` opens the slash-command PALETTE
        // (filter/arrow/Enter/Esc, `?` for per-command help); it is advertised
        // alongside the typed commands as the discoverable entry point.
        // trace:STORY-176 | ai:claude — observe is `[o]` now (moved off `?`); `[?]`
        // shows the keyboard cheat-sheet. Both appear in the single-key prompt.
        InputContext::Frontier => {
            format!("{prefix}  [o] Observe  [S] Synopsis  [X] eXplore  [A] Add  [P] Punt  [B] Back  [Q] Quit  [?] keys  (/ palette, /help, /tutor, /goal <text>, /mode <socratic|debate>, /rest)")
        }
        InputContext::Review => {
            format!("{prefix}  [o] Observe  [S] Synopsis  [X] eXplore  [P] Punt  [B] Back  [F] Forward  [Q] Quit  [?] keys  (/ palette, /help, /tutor, /goal <text>, /mode <socratic|debate>, /rest)")
        }
    }
}

// trace:BUG-98 | ai:claude
/// The free-text prompt's control suffix, expressed as slash-commands so a
/// user editing a line can still navigate. Mirrors the single-key
/// [`control_prompt`] set for each context: the frontier offers `/add` (author
/// a question), review offers `/forward` (re-walk the saved path) instead.
fn free_text_controls(context: InputContext) -> String {
    // trace:STORY-127 | ai:claude — `/observe` (or `?`) mirrors the single-key
    // observer control for the free-text line-mode prompt.
    match context {
        // trace:STORY-128 | ai:claude — `/synopsis` mirrors the single-key
        // synopsis control for the free-text line-mode prompt.
        // trace:STORY-159 | ai:claude — `/goal <text>` mirrors the single-key
        // control set for the free-text line-mode prompt.
        // trace:STORY-160 | ai:claude — `/rest` (rest your case) mirrors the typed
        // control set; it opens the closing ritual.
        // trace:STORY-161 | ai:claude — `/mode` mirrors the typed control set; it
        // toggles the questioning stance (socratic/debate).
        // trace:STORY-163 | ai:claude — a bare `/` opens the slash-command PALETTE
        // (a discoverable menu of these same commands with descriptions + `?` help);
        // `/help` and `/tutor` join the typed control set.
        // trace:STORY-174 | ai:claude — `/score` toggles the persistent gauge;
        // it mirrors the typed control set alongside `/synopsis`.
        InputContext::Frontier => {
            "/ (palette), /help, /tutor, /observe, /synopsis, /score, /goal, /request-goal, /mode, /objection, /resolved, /judge, /rest, /explore, /add, /punt, /back, /quit to navigate."
                .to_string()
        }
        InputContext::Review => {
            "/ (palette), /help, /tutor, /observe, /synopsis, /score, /goal, /request-goal, /mode, /objection, /resolved, /judge, /rest, /explore, /punt, /back, /forward, /quit to navigate."
                .to_string()
        }
    }
}

// trace:STORY-168 | ai:claude — Debug lets the front-end seam's tests assert on
// the parsed control variant; all payloads (Answer / String) are already Debug.
#[derive(Debug)]
pub(crate) enum AnswerInput {
    Answer(Answer),
    Back,
    Forward,
    // trace:STORY-88 | ai:claude
    // The user pressed the in-session quick-add control to author + link a new
    // question from the current node mid-exploration.
    Add,
    // trace:STORY-127 | ai:claude
    // The user pressed the in-session observer control ('?') to get a
    // belief-neutral reading of the current exchange. Non-destructive: the
    // session shows the reading, then re-presents the SAME question.
    Observe,
    // trace:STORY-128 | ai:claude
    // The user pressed the in-session synopsis control ('S') to get a
    // belief-neutral reading of the WHOLE session so far. Non-destructive: the
    // session shows the synopsis, then re-presents the SAME question.
    Synopsis,
    // trace:STORY-174 | ai:claude
    // The user toggled the persistent SCORE GAUGE via `/score`. Non-destructive:
    // it flips the gauge ON/OFF (a status-bar / breadcrumb-footer segment) and
    // re-presents the SAME question. When turning ON the session computes the
    // score immediately (a gate); when ON, it recomputes at gates (every N
    // answered turns), showing the last value with a freshness marker in between.
    // Default OFF until `/score` is typed, even with a goal set. Belief-neutral:
    // the gauge reads STRUCTURE / distance-to-goal, never belief-correctness.
    Score,
    // trace:STORY-159 | ai:claude
    // The user stated the session GOAL/thesis in-session via `/goal <text>`
    // (way 2 of 3). Carries the goal text. Non-destructive: the session records
    // the goal, then re-presents the SAME question — now oriented toward it. A
    // bare `/goal` with no text carries an empty string, which the session
    // treats as "show the current goal" rather than clearing it.
    Goal(String),
    // trace:STORY-173 | ai:claude
    // The user asked the Observer to PROPOSE a goal directly via `/request-goal`
    // (the on-demand alias). Unlike bare `/goal` (which first confirms with
    // `[y/N]`), this skips the confirm and proposes straight away, then offers
    // accept / edit / decline. Recognised in every context. Belief-neutral: the
    // proposed goal is the QUESTION being resolved, never a belief.
    RequestGoal,
    // trace:STORY-161 | ai:claude
    // The user toggled the session MODE in-session via `/mode <socratic|debate>`
    // (the EPIC-158 toggle). Carries the raw mode token (trimmed). Non-destructive:
    // the session switches the questioner's stance, then re-presents the SAME
    // question. A bare `/mode` (empty token) SHOWS the current mode without
    // changing it. Belief-neutral: debate steelmans the opposing side's CRAFT,
    // never asserting which belief is true.
    Mode(String),
    // trace:STORY-194 | ai:claude
    // The user switched the free-text EDITOR MODEL in-session via
    // `/editor <emacs|vim|auto>`. Carries the raw editor token (trimmed); an empty
    // token SHOWS the current model. Non-destructive: the TUI rebuilds the
    // TextEditor under the new model (the box title updates live) and re-presents
    // the SAME question. Belief-neutral: this only chooses HOW keys edit text.
    Editor(String),
    // trace:STORY-194 | ai:claude
    // The user opened the SETTINGS surface via `/settings` (panel) or mutated one
    // setting via `/settings set <key> <value>` (the headless line path). Carries
    // the REST of the line (empty for a bare `/settings`). The TUI opens the panel
    // (the headless front-end degrades to a printed value list); both keep the
    // dedicated shortcut commands (/editor, /mouse, /score, /mode) in sync.
    Settings(String),
    // trace:STORY-160 | ai:claude
    // The user (or challenger) called "rest your case": a PHASE TRANSITION out of
    // the question/answer loop into the CLOSING phase, where the exchange becomes
    // closing STATEMENTS (the user's settled position + the challenger's strongest
    // remaining objection) rather than questions. Recognised in every context.
    Rest,
    // trace:STORY-160 | ai:claude
    // The user requested a FINAL VERDICT: render the belief-neutral roundedness
    // assessment (EPIC-154) w.r.t. the goal and end the session. Recognised in the
    // closing phase (and at the frontier, where it short-circuits to the verdict).
    Verdict,
    // trace:STORY-160 | ai:claude
    // The user called "terminate" — end the closing ritual. The FAIRNESS RULE
    // applies: the terminator forfeits the last word, so the OTHER side makes the
    // final closing statement before the verdict renders.
    Terminate,
    // trace:STORY-163 | ai:claude
    // The user opened the /help channel (via the palette or the typed `/help`
    // command). Carries any free-form question typed after `/help` (empty when
    // none). Non-destructive: the session answers the process question and
    // re-presents the SAME question. STORY-163 wires the command + a graceful
    // placeholder; the belief-neutral, tool-context LLM answer lands in STORY-164.
    Help(String),
    // trace:STORY-165 | ai:claude
    // The user opened the /tutor articulation & nuance coach (via the palette or
    // the typed `/tutor` command). Carries any text typed after `/tutor`.
    // Non-destructive. The coaching LLM engine (reflect + sharpen the user's OWN
    // point, surface the missing nuance, never supply the belief) lives in
    // observer.rs (STORY-165); session.rs wires it to this variant.
    Tutor(String),
    // trace:STORY-175 | ai:claude
    // EITHER party raised a court-style `/objection <text>`: PIN the exchange on the
    // contested point. Carries the objection text. The session enters an OBJECTION
    // state — the questioner NARROWS to the point (reusing the STORY-159 goal-narrow
    // path), normal advancement pauses, and a gavel status motif shows it open. A
    // bare `/objection` (empty text) SHOWS the current open objection (or notes none).
    // One active objection at a time. Belief-neutral: the objection names a
    // STRUCTURAL tension, never asserts a belief.
    Objection(String),
    // trace:STORY-175 | ai:claude
    // The OBJECTING party calls `/resolved`: withdraw/accept the objection -> clear
    // it and return to normal flow (logged). ASYMMETRIC — only the objector may call
    // it; a wrong-caller is rejected with a helpful note. Recognised in every context.
    Resolved,
    // trace:STORY-175 | ai:claude
    // The OTHER (non-objecting) party calls `/judge`: escalate to the Observer, which
    // renders a BELIEF-NEUTRAL ruling (SUSTAINED / OVERRULED + resolving condition),
    // then clears the objection. ASYMMETRIC — only the non-objecting party may call
    // it. Offline degrades to a "needs an LLM backend" note. Recognised in every
    // context. Belief-neutral: the ruling judges STRUCTURE, never which belief is true.
    Judge,
    End,
}

pub(crate) enum FreeTextInput {
    Plain,
    Interactive(Box<DefaultEditor>),
}

struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

impl FreeTextInput {
    pub(crate) fn from_stdin() -> Result<Self> {
        if io::stdin().is_terminal() {
            Self::interactive()
        } else {
            Ok(Self::Plain)
        }
    }

    fn interactive() -> Result<Self> {
        // trace:STORY-55 | ai:codex
        let config = RustylineConfig::builder()
            .edit_mode(editor_edit_mode())
            .build();
        let editor = DefaultEditor::with_config(config)
            .map_err(|error| QuizdomError::Io(io::Error::new(io::ErrorKind::Other, error)))?;
        Ok(Self::Interactive(Box::new(editor)))
    }

    pub(crate) fn read_line(
        &mut self,
        input: &mut impl BufRead,
        output: &mut dyn Write,
        prompt: &str,
    ) -> Result<Option<String>> {
        match self {
            Self::Plain => {
                write!(output, "{prompt}")?;
                output.flush()?;
                let mut raw = String::new();
                if input.read_line(&mut raw)? == 0 {
                    Ok(None)
                } else {
                    Ok(Some(raw.trim().to_string()))
                }
            }
            Self::Interactive(editor) => match editor.readline(prompt) {
                Ok(line) => {
                    if !line.trim().is_empty() {
                        let _ = editor.add_history_entry(line.as_str());
                    }
                    Ok(Some(line.trim().to_string()))
                }
                Err(rustyline::error::ReadlineError::Interrupted)
                | Err(rustyline::error::ReadlineError::Eof) => Ok(None),
                Err(error) => Err(QuizdomError::Io(io::Error::new(
                    io::ErrorKind::Other,
                    error,
                ))),
            },
        }
    }
}

fn editor_edit_mode() -> EditMode {
    let editor = env::var("EDITOR")
        .ok()
        .or_else(|| env::var("VISUAL").ok())
        .unwrap_or_default();
    edit_mode_from_editor(&editor)
}

pub(crate) fn edit_mode_from_editor(editor: &str) -> EditMode {
    let editor_name = Path::new(&editor)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(editor)
        .to_ascii_lowercase();
    match editor_name.as_str() {
        "vi" | "vim" | "nvim" => EditMode::Vi,
        _ => EditMode::Emacs,
    }
}

pub(crate) fn read_answer_or_end(
    kind: &AnswerKind,
    context: InputContext,
    // trace:STORY-190 | ai:claude — the live session snapshot for the palette's
    // context-aware availability (which commands grey out right now). Threaded
    // from the engine through the front-end seam into `run_palette*`.
    palette_ctx: palette::PaletteContext,
    input: &mut impl BufRead,
    free_text_input: &mut FreeTextInput,
    output: &mut dyn Write,
) -> Result<AnswerInput> {
    loop {
        let mut raw = match kind {
            AnswerKind::FreeText => free_text_input
                .read_line(input, output, "")?
                .ok_or_else(|| QuizdomError::Parse("no answer provided".to_string()))?,
            _ => read_control_answer_or_line(input, output, kind, context, palette_ctx)?,
        };
        // trace:STORY-163 | ai:claude — a bare `/` line at a free-text prompt
        // opens the slash-command PALETTE overlay (the single-key prompts open it
        // via the `/` key in `read_single_key_answer`). A selected command
        // REPLACES the bare `/` and then flows through the SAME command
        // recognizers below, so the palette and the typed form route identically.
        // Cancelling (Esc / backspacing out) or a non-TTY leaves the line as the
        // bare `/`, which falls through to ordinary parsing — non-TTY use is
        // unaffected.
        if is_palette_trigger(&raw) {
            if let Some(palette::PaletteOutcome::Selected(command)) =
                palette::run_palette(palette_ctx, output)?
            {
                raw = command;
            }
        }
        if is_end_command(&raw) {
            return Ok(AnswerInput::End);
        }
        // trace:STORY-176 | ai:claude — `?` (or `/keys`) prints the keyboard
        // CHEAT-SHEET and re-prompts for the same input. In the headless / non-TTY
        // line path there is no overlay, so the cheat-sheet degrades to the static
        // printed list (generated from the keymap registry, so it never drifts from
        // the TUI dispatcher). Non-destructive — it loops back for the next input.
        if is_cheatsheet_command(&raw) {
            writeln!(output, "{}", crate::keymap::render_cheat_sheet())?;
            write!(output, "> ")?;
            output.flush()?;
            continue;
        }
        if is_back_command(&raw) {
            return Ok(AnswerInput::Back);
        }
        // trace:STORY-127 | ai:claude — the observer control is non-destructive,
        // so it is recognized in every context before any answer parsing.
        if is_observe_command(&raw) {
            return Ok(AnswerInput::Observe);
        }
        // trace:STORY-128 | ai:claude — the synopsis control is non-destructive
        // (a whole-session reading), so it too is recognized in every context.
        if is_synopsis_command(&raw) {
            return Ok(AnswerInput::Synopsis);
        }
        // trace:STORY-174 | ai:claude — the `/score` gauge toggle is
        // non-destructive (it flips the status-bar gauge, then re-presents the
        // same question), so it is recognized in every context like the other
        // meta controls.
        if is_score_command(&raw) {
            return Ok(AnswerInput::Score);
        }
        // trace:STORY-159 | ai:claude — the `/goal <text>` command is
        // non-destructive (it sets the orienting thesis, then re-presents the
        // same question), so it is recognized in every context.
        // trace:STORY-173 | ai:claude — `/request-goal` is the on-demand alias
        // that proposes a goal directly (skipping the bare-`/goal` `[y/N]`
        // confirm). Checked BEFORE `goal_command_text` so the longer keyword wins
        // (a bare-`/goal` recognizer would otherwise swallow `/request-goal` only
        // if it led with `goal`, but ordering it first keeps the intent explicit).
        if is_request_goal_command(&raw) {
            return Ok(AnswerInput::RequestGoal);
        }
        if let Some(goal) = goal_command_text(&raw) {
            return Ok(AnswerInput::Goal(goal));
        }
        // trace:STORY-161 | ai:claude — the `/mode <socratic|debate>` toggle is
        // non-destructive (it switches the questioner's stance, then re-presents
        // the same question), so it is recognized in every context.
        if let Some(mode) = mode_command_text(&raw) {
            return Ok(AnswerInput::Mode(mode));
        }
        // trace:STORY-194 | ai:claude — `/editor <emacs|vim|auto>` switches the
        // free-text editor model and `/settings` opens the settings surface. Both
        // are non-destructive (the same question is re-presented), so they are
        // recognized in every context like the other meta controls.
        if let Some(editor) = editor_command_text(&raw) {
            return Ok(AnswerInput::Editor(editor));
        }
        if let Some(rest) = settings_command_text(&raw) {
            return Ok(AnswerInput::Settings(rest));
        }
        // trace:STORY-160 | ai:claude — the closing-ritual controls are
        // non-destructive at the input layer (the session decides what each does)
        // and are recognized in every context so a user can rest / call a verdict /
        // terminate from wherever they are.
        if is_rest_command(&raw) {
            return Ok(AnswerInput::Rest);
        }
        if is_verdict_command(&raw) {
            return Ok(AnswerInput::Verdict);
        }
        if is_terminate_command(&raw) {
            return Ok(AnswerInput::Terminate);
        }
        // trace:STORY-163 | ai:claude — `/help` and `/tutor` are non-destructive
        // out-of-band channels (EPIC-162), so they are recognized in every context
        // like the other meta controls. They carry any free-form text typed after
        // the keyword.
        if let Some(question) = help_command_text(&raw) {
            return Ok(AnswerInput::Help(question));
        }
        if let Some(text) = tutor_command_text(&raw) {
            return Ok(AnswerInput::Tutor(text));
        }
        // trace:STORY-175 | ai:claude — the court-style objection controls. `/resolved`
        // and `/judge` are exact-keyword commands checked before `objection_command_text`
        // so the bare-keyword objection recognizer never swallows them. All three are
        // recognized in every context (the session enforces the asymmetric caller guards
        // + the one-at-a-time / offline rules), like the other out-of-band controls.
        if is_resolved_command(&raw) {
            return Ok(AnswerInput::Resolved);
        }
        if is_judge_command(&raw) {
            return Ok(AnswerInput::Judge);
        }
        if let Some(text) = objection_command_text(&raw) {
            return Ok(AnswerInput::Objection(text));
        }
        // trace:STORY-88 | ai:claude — quick-add is a frontier-only control.
        if context == InputContext::Frontier && is_add_command(&raw) {
            return Ok(AnswerInput::Add);
        }
        if context == InputContext::Review && is_forward_command(&raw) {
            return Ok(AnswerInput::Forward);
        }
        if let Some(normalized) = normalize_answer(kind, &raw) {
            return Ok(AnswerInput::Answer(Answer { raw, normalized }));
        }
        write!(output, "Please enter a valid answer or /end: ")?;
        output.flush()?;
    }
}

fn read_control_answer_or_line(
    input: &mut impl BufRead,
    output: &mut dyn Write,
    kind: &AnswerKind,
    context: InputContext,
    // trace:STORY-190 | ai:claude — passed through to the single-key reader so the
    // `/`-opened palette greys inapplicable commands.
    palette_ctx: palette::PaletteContext,
) -> Result<String> {
    // trace:STORY-51 | ai:codex
    if io::stdin().is_terminal() {
        if let Some(raw) = read_single_key_answer(output, kind, context, palette_ctx)? {
            return Ok(raw);
        }
    }
    let mut raw = String::new();
    if input.read_line(&mut raw)? == 0 {
        return Err(QuizdomError::Parse("no answer provided".to_string()));
    }
    Ok(raw.trim().to_string())
}

fn read_single_key_answer(
    output: &mut dyn Write,
    kind: &AnswerKind,
    context: InputContext,
    // trace:STORY-190 | ai:claude
    palette_ctx: palette::PaletteContext,
) -> Result<Option<String>> {
    let Ok(_raw_mode) = RawModeGuard::enter() else {
        return Ok(None);
    };
    loop {
        let event = event::read()
            .map_err(|error| QuizdomError::Io(io::Error::new(io::ErrorKind::Other, error)))?;
        let Event::Key(key) = event else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        let raw = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') if matches!(kind, AnswerKind::YesNo) => "y",
            KeyCode::Char('n') | KeyCode::Char('N') if matches!(kind, AnswerKind::YesNo) => "n",
            KeyCode::Char('x') | KeyCode::Char('X') => "x",
            // trace:STORY-127 | ai:claude — the observer key. Non-destructive in
            // every context, so it is accepted regardless of answer kind.
            // trace:STORY-176 | ai:claude — the observe key MOVED from `?` to `o`
            // (the DECIDED change); `?` now opens the cheat-sheet.
            KeyCode::Char('o') | KeyCode::Char('O') => "/observe",
            // trace:STORY-176 | ai:claude — `?` is the keyboard CHEAT-SHEET key.
            // Return it so the outer loop's `is_cheatsheet_command` prints the
            // static list AFTER raw mode is dropped (so it renders with normal line
            // endings) and re-prompts. Headless degrade of the TUI overlay.
            KeyCode::Char('?') => "?",
            // trace:STORY-128 | ai:claude — the synopsis key. Non-destructive in
            // every context, so it is accepted regardless of answer kind.
            KeyCode::Char('s') | KeyCode::Char('S') => "/synopsis",
            // trace:STORY-88 | ai:claude — frontier-only quick-add key.
            KeyCode::Char('a') | KeyCode::Char('A') if context == InputContext::Frontier => "/add",
            KeyCode::Char('p') | KeyCode::Char('P') => "p",
            KeyCode::Char('b') | KeyCode::Char('B') => "b",
            KeyCode::Char('f') | KeyCode::Char('F') if context == InputContext::Review => "f",
            KeyCode::Char('q') | KeyCode::Char('Q') => "/end",
            // trace:STORY-163 | ai:claude — typing '/' as the first key opens the
            // slash-command PALETTE overlay (we are already in raw mode here, so
            // use the in-raw variant to leave the raw-mode lifetime to the guard
            // above). A selected command is returned as its canonical typed form,
            // which then flows through the SAME command recognizers as the typed
            // form (so palette and typed routes are identical). Esc / backspacing
            // out cancels back to the prompt — we just re-loop for the next key.
            KeyCode::Char('/') => match palette::run_palette_in_raw(palette_ctx, output)? {
                Some(palette::PaletteOutcome::Selected(command)) => {
                    writeln!(output, "{command}")?;
                    output.flush()?;
                    return Ok(Some(command));
                }
                Some(palette::PaletteOutcome::Cancelled) | None => continue,
            },
            KeyCode::Char(character) if matches!(kind, AnswerKind::Choice(_)) => {
                if character.is_ascii_digit() {
                    write!(output, "{character}\n")?;
                    output.flush()?;
                    return Ok(Some(character.to_string()));
                }
                continue;
            }
            KeyCode::Esc => "/end",
            _ => continue,
        };
        writeln!(output, "{raw}")?;
        output.flush()?;
        return Ok(Some(raw.to_string()));
    }
}

// trace:BUG-98 | ai:claude — `/quit` joins the recognized end aliases so the
// free-text slash-command form matches the prompt the user is shown.
pub(crate) fn is_end_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "/end" | "/quit" | "q" | "quit"
    )
}

// trace:BUG-98 | ai:claude — `/back` joins the recognized aliases so the
// free-text slash-command form maps to the same Back action.
pub(crate) fn is_back_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "b" | "/b" | "/back" | "back"
    )
}

// trace:BUG-98 | ai:claude — `/forward` joins the recognized aliases so the
// free-text slash-command form maps to the same Forward action.
pub(crate) fn is_forward_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "f" | "/f" | "/forward" | "forward"
    )
}

// trace:STORY-176 | ai:claude
/// The keyboard CHEAT-SHEET control: `?` (the adopted convention) or `/keys`.
/// In the TUI this opens an overlay; in the headless / non-TTY line path it prints
/// the static cheat-sheet list (the graceful degrade). Recognised as a bare `?`,
/// `/?`, `/keys`, or the word `keys` (case-insensitive). Non-destructive.
pub(crate) fn is_cheatsheet_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "?" | "/?" | "/keys" | "keys" | "/cheatsheet" | "/cheat-sheet"
    )
}

// trace:STORY-127 | ai:claude
// trace:STORY-176 | ai:claude — the observe affordance MOVED off `?` to `o` (the
/// DECIDED change): `?` is now the keyboard CHEAT-SHEET. Observe is recognised as
/// a bare `o`, `/o`, `/observe`, or the word `observe`. The single-key `o` is
/// gated to single-key answer prompts (see `read_single_key_answer`); the slash /
/// word forms keep working in the palette and free-text line.
pub(crate) fn is_observe_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "o" | "/o" | "/observe" | "observe"
    )
}

// trace:STORY-128 | ai:claude
/// The in-session synopsis control: surface a belief-neutral reading of the
/// WHOLE session so far without disturbing the current question. Recognised as a
/// bare `s`, `/s`, `/synopsis`, or the word `synopsis`.
pub(crate) fn is_synopsis_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "s" | "/s" | "/synopsis" | "synopsis"
    )
}

// trace:STORY-174 | ai:claude
/// The persistent score-gauge TOGGLE: `/score` flips a distance-to-goal /
/// roundedness gauge ON/OFF in the status bar (headless: the breadcrumb footer).
/// Recognised as `/score` or the bare word `score` (case-insensitive). It is the
/// SOLE toggle — the gauge defaults OFF until `/score` is typed, even when a goal
/// is set. Non-destructive: it never changes the session, only the gauge
/// visibility. A free-text answer that merely contains "score" mid-sentence is
/// NOT a command — only the exact leading keyword triggers.
pub(crate) fn is_score_command(raw: &str) -> bool {
    matches!(raw.trim().to_ascii_lowercase().as_str(), "/score" | "score")
}

// trace:STORY-159 | ai:claude
/// The in-session goal command: state the session GOAL/thesis. Recognised as
/// `/goal <text>` or `goal <text>` (leading keyword, case-insensitive). Returns
/// the goal text (trimmed) when the line is a goal command — an empty string for
/// a bare `/goal` (the session treats that as "show the current goal"). Returns
/// `None` when the line is not a goal command, so ordinary free-text answers
/// that merely mention the word "goal" mid-sentence are unaffected (only a
/// leading `goal`/`/goal` keyword triggers it).
pub(crate) fn goal_command_text(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    // The keyword is whichever leading token the user typed: `/goal` or `goal`.
    // Match it case-insensitively, then carry the REST verbatim (the goal text
    // must preserve the user's own casing). Only a leading keyword followed by
    // whitespace or end-of-line triggers — a free-text answer that merely
    // contains "goal" mid-sentence is left as an answer.
    for keyword in ["/goal", "goal"] {
        if trimmed.len() >= keyword.len() && trimmed[..keyword.len()].eq_ignore_ascii_case(keyword)
        {
            let rest = &trimmed[keyword.len()..];
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                return Some(rest.trim().to_string());
            }
        }
    }
    None
}

// trace:STORY-173 | ai:claude
/// The on-demand goal-request command: ask the Observer to PROPOSE a session goal
/// directly. Recognised as `/request-goal`, `/request goal`, or `request-goal`
/// (case-insensitive, leading keyword only). Unlike bare `/goal`, this skips the
/// `[y/N]` confirm and proposes straight away. A free-text answer that merely
/// contains "request" mid-sentence is NOT a command — only the exact leading
/// keyword triggers, so ordinary answers are unaffected.
pub(crate) fn is_request_goal_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "/request-goal" | "/request goal" | "request-goal" | "/requestgoal"
    )
}

// trace:STORY-161 | ai:claude
/// The in-session mode toggle: switch the questioning MODE (the EPIC-158 toggle).
/// Recognised ONLY as a leading `/mode <text>` keyword (slash-prefixed), so an
/// ordinary free-text answer that merely contains the word "mode" mid-sentence is
/// left as an answer — unlike `/goal`, we do not accept a bare `mode` keyword
/// because "mode" is a far more common ordinary word. Returns the mode token
/// (trimmed) when the line is a mode command — an empty string for a bare `/mode`
/// (the session treats that as "show the current mode"). Returns `None` otherwise.
pub(crate) fn mode_command_text(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let keyword = "/mode";
    if trimmed.len() >= keyword.len() && trimmed[..keyword.len()].eq_ignore_ascii_case(keyword) {
        let rest = &trimmed[keyword.len()..];
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

// trace:STORY-194 | ai:claude
/// The runtime EDITOR-MODE toggle: `/editor <emacs|vim|auto>` switches the
/// free-text editor model live (rebuilding the TUI TextEditor). Recognised ONLY
/// as a leading `/editor` keyword (slash-prefixed), like `/mode`, since the bare
/// word "editor" is plausible mid-answer. Returns the editor token (trimmed) when
/// the line is an editor command — an empty string for a bare `/editor` (the
/// session SHOWS the current model). Returns `None` otherwise so an ordinary
/// free-text answer mentioning "editor" is left as an answer.
pub(crate) fn editor_command_text(raw: &str) -> Option<String> {
    leading_keyword_text(raw, &["/editor"])
}

// trace:STORY-194 | ai:claude
/// The SETTINGS surface: `/settings` opens the panel (TUI) / prints the value
/// list (headless); `/settings set <key> <value>` mutates one setting on the
/// line path. Recognised ONLY as a leading `/settings` keyword. Returns the REST
/// of the line (e.g. `"set editor vim"`, or `""` for a bare `/settings`) so the
/// session can route the panel vs the headless set-path. Returns `None` for an
/// ordinary answer. `/config` is accepted as a friendly alias.
pub(crate) fn settings_command_text(raw: &str) -> Option<String> {
    leading_keyword_text(raw, &["/settings", "/config"])
}

// trace:STORY-160 | ai:claude
/// The "rest your case" control: a PHASE TRANSITION out of the question/answer
/// loop into the CLOSING phase. Recognised as `/rest`, `rest`, `/rest case`, or
/// `rest case` (the natural phrasing the spec uses), case-insensitively. A
/// free-text answer that merely contains the word "rest" mid-sentence is NOT a
/// command — only the leading keyword (optionally followed by `case`) triggers.
pub(crate) fn is_rest_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "/rest" | "rest" | "/rest case" | "rest case" | "/rest-case" | "rest-case"
    )
}

// trace:STORY-160 | ai:claude
/// The "final verdict" control: render the belief-neutral roundedness verdict
/// (EPIC-154) and end. Recognised as `/verdict` or the word `verdict`.
pub(crate) fn is_verdict_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "/verdict" | "verdict"
    )
}

// trace:STORY-160 | ai:claude
/// The "terminate" control: end the closing ritual under the FAIRNESS RULE (the
/// terminator forfeits the last word). Recognised as `/terminate` or the word
/// `terminate`. Distinct from the session-end controls (`/end` / `q`), which
/// quit without the closing ritual.
pub(crate) fn is_terminate_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "/terminate" | "terminate"
    )
}

// trace:STORY-88 | ai:claude
/// The in-session quick-add control: author + link a new question from the
/// current node. Recognised as a bare `a`, `/a`, `/add`, or the word `add`.
pub(crate) fn is_add_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "a" | "/a" | "/add" | "add"
    )
}

// trace:STORY-163 | ai:claude
/// The `/help` control: an out-of-band process-help channel (EPIC-162 /
/// STORY-164). Recognised ONLY as a leading `/help` keyword (slash-prefixed),
/// optionally followed by a free-form question — we do not accept a bare `help`
/// keyword so a one-word free-text answer is never swallowed. The trailing text,
/// if any, is the user's question; an empty string means "open help with no
/// question yet". The LLM engine that answers it lands in STORY-164 — STORY-163
/// only wires the command through the palette + recognizer so selection routes
/// somewhere graceful.
pub(crate) fn help_command_text(raw: &str) -> Option<String> {
    leading_keyword_text(raw, &["/help"])
}

// trace:STORY-163 | ai:claude
/// The `/tutor` control: the articulation & nuance coach (EPIC-162 /
/// STORY-165). Recognised ONLY as a leading `/tutor` keyword (slash-prefixed),
/// like `/mode`, since "tutor" is rare enough as an ordinary word that the slash
/// form is the safe trigger. Trailing text is carried verbatim. The coaching LLM
/// engine lands in STORY-165 — STORY-163 only routes the command.
pub(crate) fn tutor_command_text(raw: &str) -> Option<String> {
    leading_keyword_text(raw, &["/tutor"])
}

// trace:STORY-163 | ai:claude
/// Shared leading-keyword matcher: if `raw` (trimmed) starts with one of
/// `keywords` (case-insensitively) followed by whitespace or end-of-line, return
/// the REST of the line verbatim-trimmed; otherwise `None`. Mirrors the pattern
/// [`goal_command_text`] / [`mode_command_text`] use so a free-text answer that
/// merely contains the keyword mid-sentence is left as an answer.
fn leading_keyword_text(raw: &str, keywords: &[&str]) -> Option<String> {
    let trimmed = raw.trim();
    for keyword in keywords {
        if trimmed.len() >= keyword.len() && trimmed[..keyword.len()].eq_ignore_ascii_case(keyword)
        {
            let rest = &trimmed[keyword.len()..];
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                return Some(rest.trim().to_string());
            }
        }
    }
    None
}

// trace:STORY-175 | ai:claude
/// The court-style objection command: PIN the exchange on a contested point.
/// Recognised ONLY as a leading `/objection` keyword (slash-prefixed), like
/// `/mode` / `/tutor`, since the bare word "objection" is plausible mid-answer.
/// Returns the objection TEXT (trimmed) when the line is an objection command — an
/// empty string for a bare `/objection` (the session treats that as "show the open
/// objection"). Returns `None` otherwise, so a free-text answer that merely
/// mentions "objection" mid-sentence is left as an answer.
pub(crate) fn objection_command_text(raw: &str) -> Option<String> {
    leading_keyword_text(raw, &["/objection", "/object"])
}

// trace:STORY-175 | ai:claude
/// The OBJECTOR-only `/resolved` control: withdraw/accept the open objection.
/// Recognised as `/resolved`, `/resolve`, or the word `resolved` (case-insensitive,
/// exact line). The session enforces that ONLY the objecting party may call it.
pub(crate) fn is_resolved_command(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "/resolved" | "/resolve" | "resolved"
    )
}

// trace:STORY-175 | ai:claude
/// The NON-OBJECTOR-only `/judge` control: escalate the open objection to the
/// Observer for a belief-neutral SUSTAINED/OVERRULED ruling. Recognised as
/// `/judge` or the word `judge` (case-insensitive, exact line). The session
/// enforces that ONLY the non-objecting party may call it, and degrades offline.
pub(crate) fn is_judge_command(raw: &str) -> bool {
    matches!(raw.trim().to_ascii_lowercase().as_str(), "/judge" | "judge")
}

// trace:STORY-163 | ai:claude
/// The palette trigger: a line that is exactly `/` (the first character `/` with
/// nothing after it) opens the slash-command palette overlay. Only a BARE `/`
/// triggers it — `/observe`, `/goal foo`, or any other already-typed
/// slash-command is left alone so it routes through its own recognizer, and an
/// ordinary free-text answer that merely contains a slash mid-sentence is
/// unaffected.
pub(crate) fn is_palette_trigger(raw: &str) -> bool {
    raw.trim() == "/"
}

pub(crate) fn normalize_answer(kind: &AnswerKind, raw: &str) -> Option<String> {
    match kind {
        // trace:STORY-163 | ai:claude — the full `/explore` / `/punt` slash forms
        // (the canonical forms the palette returns) are accepted for YesNo too, so
        // selecting them from the palette routes identically to the typed form
        // regardless of answer kind (previously only `/x` / `/p` were accepted
        // here; the long forms were FreeText-only via BUG-98).
        AnswerKind::YesNo => match raw.trim().to_ascii_lowercase().as_str() {
            "yes" | "y" => Some("yes".to_string()),
            "no" | "n" => Some("no".to_string()),
            "x" | "/x" | "/explore" | "explore" => Some("explore".to_string()),
            "p" | "/p" | "/punt" | "punt" => Some("punt".to_string()),
            _ => None,
        },
        AnswerKind::Choice(options) => {
            let trimmed = raw.trim();
            match trimmed.to_ascii_lowercase().as_str() {
                // trace:STORY-163 | ai:claude — `/explore` / `/punt` accepted for
                // Choice too (same rationale as YesNo above) so palette selection
                // routes uniformly across answer kinds.
                "x" | "/x" | "/explore" | "explore" => return Some("explore".to_string()),
                "p" | "/p" | "/punt" | "punt" => return Some("punt".to_string()),
                _ => {}
            }
            if let Ok(index) = trimmed.parse::<usize>() {
                return options.get(index.checked_sub(1)?).cloned();
            }
            options
                .iter()
                .find(|option| option.eq_ignore_ascii_case(trimmed))
                .cloned()
        }
        // trace:BUG-98 | ai:claude — recognize the full `/explore` / `/punt`
        // slash-commands (the form the free-text prompt advertises) alongside
        // the short `/x` / `/p` aliases. Bare words like `explore` stay a
        // legitimate free-text answer; only the leading-slash form is a command.
        AnswerKind::FreeText => match raw.trim().to_ascii_lowercase().as_str() {
            "x" | "/x" | "/explore" => Some("explore".to_string()),
            "p" | "/p" | "/punt" => Some("punt".to_string()),
            _ => {
                let other = raw.trim();
                (!other.is_empty()).then(|| other.to_string())
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- STORY-163: palette trigger ----------------------------------------

    #[test]
    fn a_bare_slash_triggers_the_palette() {
        // trace:STORY-163 | ai:claude — typing `/` as the whole line opens the
        // palette; whitespace around it is tolerated.
        assert!(is_palette_trigger("/"));
        assert!(is_palette_trigger("  /  "));
    }

    #[test]
    fn an_already_typed_slash_command_does_not_trigger_the_palette() {
        // A command the user already typed in full routes through its own
        // recognizer, not the palette overlay.
        for raw in ["/observe", "/goal free will", "//", "/ x", "explore"] {
            assert!(!is_palette_trigger(raw), "{raw} must not open the palette");
        }
    }

    // ---- STORY-163: /help + /tutor recognizers -----------------------------

    #[test]
    fn help_command_is_recognized_with_and_without_a_question() {
        // trace:STORY-163 | ai:claude
        assert_eq!(help_command_text("/help"), Some(String::new()));
        assert_eq!(
            help_command_text("/help how do I rest my case?"),
            Some("how do I rest my case?".to_string())
        );
        assert_eq!(
            help_command_text("  /HELP  what is observe  "),
            Some("what is observe".to_string())
        );
    }

    #[test]
    fn help_does_not_swallow_an_ordinary_answer() {
        // Slash-only: a free-text answer that merely contains or equals "help"
        // (no leading slash) is left as an answer, and `/helper` is not `/help`.
        assert_eq!(help_command_text("help"), None);
        assert_eq!(help_command_text("I need help understanding this"), None);
        assert_eq!(help_command_text("/helper"), None);
    }

    #[test]
    fn tutor_command_is_slash_only_and_carries_its_text() {
        // trace:STORY-163 | ai:claude
        assert_eq!(tutor_command_text("/tutor"), Some(String::new()));
        assert_eq!(
            tutor_command_text("/tutor I think determinism but..."),
            Some("I think determinism but...".to_string())
        );
        // Bare word and mid-sentence mentions are ordinary answers.
        assert_eq!(tutor_command_text("tutor"), None);
        assert_eq!(tutor_command_text("a tutor helped me"), None);
        assert_eq!(tutor_command_text("/tutored"), None);
    }

    // ---- STORY-163: palette selection routes like the typed form -----------

    #[test]
    fn palette_explore_and_punt_route_for_every_answer_kind() {
        // trace:STORY-163 | ai:claude — the palette returns the canonical `/explore`
        // / `/punt` forms; selection must route to the same action the typed form
        // does, regardless of answer kind.
        let choice = AnswerKind::Choice(vec!["a".to_string(), "b".to_string()]);
        for kind in [&AnswerKind::YesNo, &choice, &AnswerKind::FreeText] {
            assert_eq!(
                normalize_answer(kind, "/explore"),
                Some("explore".to_string()),
                "/explore must route for {kind:?}"
            );
            assert_eq!(
                normalize_answer(kind, "/punt"),
                Some("punt".to_string()),
                "/punt must route for {kind:?}"
            );
        }
    }

    #[test]
    fn bare_explore_is_still_a_freetext_answer_not_a_command() {
        // The slash form is the command; the bare word stays a legitimate answer
        // for a free-text question (unchanged from BUG-98).
        assert_eq!(
            normalize_answer(&AnswerKind::FreeText, "explore"),
            Some("explore".to_string())
        );
    }

    #[test]
    fn palette_command_strings_route_through_the_existing_recognizers() {
        // trace:STORY-163 | ai:claude — every canonical command string the palette
        // can return is recognized by exactly one input recognizer, so a palette
        // selection is indistinguishable from typing the command. This is the
        // acceptance guarantee: "running a command routes to the same action as the
        // typed form".
        assert!(is_observe_command("/observe"));
        assert!(is_synopsis_command("/synopsis"));
        assert!(is_back_command("/back"));
        assert!(is_add_command("/add"));
        assert!(is_end_command("/quit"));
        assert!(is_rest_command("/rest"));
        assert!(goal_command_text("/goal").is_some());
        // trace:STORY-173 | ai:claude — `/request-goal` routes to its own variant.
        assert!(is_request_goal_command("/request-goal"));
        assert!(mode_command_text("/mode").is_some());
        // trace:STORY-194 | ai:claude — `/editor` and `/settings` route to their
        // own variants (the palette/typed forms are indistinguishable).
        assert!(editor_command_text("/editor").is_some());
        assert!(settings_command_text("/settings").is_some());
        assert!(help_command_text("/help").is_some());
        assert!(tutor_command_text("/tutor").is_some());
        assert_eq!(
            normalize_answer(&AnswerKind::YesNo, "/explore"),
            Some("explore".to_string())
        );
        assert_eq!(
            normalize_answer(&AnswerKind::YesNo, "/punt"),
            Some("punt".to_string())
        );
    }

    // ---- STORY-194: /editor + /settings recognizers ------------------------

    // trace:STORY-194 | ai:claude — `/editor <token>` carries the editor token; a
    // bare `/editor` carries the empty string ("show current"); an ordinary answer
    // mentioning "editor" mid-sentence is NOT a command.
    #[test]
    fn editor_command_recognizes_leading_keyword_only() {
        assert_eq!(editor_command_text("/editor vim"), Some("vim".to_string()));
        assert_eq!(
            editor_command_text("/EDITOR  auto "),
            Some("auto".to_string())
        );
        assert_eq!(editor_command_text("/editor"), Some(String::new()));
        assert_eq!(editor_command_text("my editor is broken"), None);
        assert_eq!(editor_command_text("editor"), None);
    }

    // trace:STORY-194 | ai:claude — `/settings` (and the `/config` alias) carries
    // the rest of the line (empty = open panel; `set ...` = headless mutate); an
    // ordinary answer mentioning "settings" is NOT a command.
    #[test]
    fn settings_command_recognizes_leading_keyword_only() {
        assert_eq!(settings_command_text("/settings"), Some(String::new()));
        assert_eq!(
            settings_command_text("/settings set editor vim"),
            Some("set editor vim".to_string())
        );
        assert_eq!(settings_command_text("/config"), Some(String::new()));
        assert_eq!(settings_command_text("the settings menu"), None);
    }

    // ---- STORY-176: observe moved from `?` to `o` --------------------------

    // trace:STORY-176 | ai:claude — the observe affordance is now `o` (and the
    // slash / word forms), NOT `?`. `?` is reserved for the keyboard cheat-sheet,
    // so it must no longer be recognized as observe.
    #[test]
    fn observe_is_now_o_not_question_mark() {
        assert!(is_observe_command("o"));
        assert!(is_observe_command("/o"));
        assert!(is_observe_command("/observe"));
        assert!(is_observe_command("observe"));
        // `?` and `/?` are no longer observe — they belong to the cheat-sheet now.
        assert!(!is_observe_command("?"));
        assert!(!is_observe_command("/?"));
    }

    // trace:STORY-176 | ai:claude — `?` (and `/keys`) are the cheat-sheet control,
    // NOT observe; observe is `o`. Guards the binding swap at the recognizer level.
    #[test]
    fn cheatsheet_is_question_mark_and_keys_not_observe() {
        assert!(is_cheatsheet_command("?"));
        assert!(is_cheatsheet_command("/?"));
        assert!(is_cheatsheet_command("/keys"));
        assert!(is_cheatsheet_command("keys"));
        // `o` / `/observe` are observe, not the cheat-sheet.
        assert!(!is_cheatsheet_command("o"));
        assert!(!is_cheatsheet_command("/observe"));
    }

    // ---- STORY-175: the /objection court-mechanic recognizers --------------

    #[test]
    fn objection_command_is_slash_only_and_carries_its_text() {
        // trace:STORY-175 | ai:claude — a leading `/objection <text>` is a command
        // carrying the contested point; a bare `/objection` carries an empty string
        // (the session shows the open objection).
        assert_eq!(objection_command_text("/objection"), Some(String::new()));
        assert_eq!(
            objection_command_text("/objection you never defined free"),
            Some("you never defined free".to_string())
        );
        // `/object` is an accepted shorthand.
        assert_eq!(
            objection_command_text("/object that begs the question"),
            Some("that begs the question".to_string())
        );
        // Bare word / mid-sentence mentions are ordinary answers, not commands.
        assert_eq!(objection_command_text("objection"), None);
        assert_eq!(objection_command_text("I have no objection to that"), None);
        assert_eq!(objection_command_text("/objections"), None);
    }

    #[test]
    fn resolved_and_judge_are_exact_keyword_commands() {
        // trace:STORY-175 | ai:claude — `/resolved` (objector) and `/judge` (other
        // party) are exact-line keywords; the session enforces WHO may call each.
        assert!(is_resolved_command("/resolved"));
        assert!(is_resolved_command("/resolve"));
        assert!(is_resolved_command("resolved"));
        assert!(is_judge_command("/judge"));
        assert!(is_judge_command("judge"));
        // Not commands: mid-sentence mentions / partials.
        assert!(!is_resolved_command("I resolved to keep going"));
        assert!(!is_judge_command("do not judge me"));
        assert!(!is_judge_command("/judgement"));
    }

    #[test]
    fn resolved_and_judge_win_over_the_bare_objection_recognizer() {
        // trace:STORY-175 | ai:claude — `/resolved` / `/judge` are recognized BEFORE
        // the objection text recognizer, so they never get swallowed as objection
        // text (they are not `/objection`-prefixed anyway, but order is the contract).
        assert!(objection_command_text("/resolved").is_none());
        assert!(objection_command_text("/judge").is_none());
    }
}
