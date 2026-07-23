// trace:STORY-194 | ai:claude
//! The runtime SETTINGS surface — the model behind `/settings`, `/editor`, and
//! the shortcut commands (`/mouse`, `/score`, `/mode`), plus the small
//! forward-compatible config file they persist to.
//!
//! ## Why a settings surface
//!
//! Before STORY-194 the editor model was inferred ONCE from `$EDITOR`/`$VISUAL`
//! at startup (STORY-180) with no in-app switch, and the mouse / score / mode
//! toggles were each their own dedicated command with no unified home. This
//! module is the single source of truth for the runtime-adjustable preferences:
//!
//! * [`EditorChoice`] — Emacs / Vim / **Auto** (the `$EDITOR`-inferred default).
//! * `mouse` — the STORY-193 mouse-capture toggle.
//! * `score` — the STORY-174 distance-to-goal gauge toggle.
//! * `mode`  — the EPIC-158 Socratic / Debate session mode.
//!
//! The dedicated commands stay as SHORTCUTS that mutate the same [`Settings`], so
//! the `/settings` panel and the shortcuts can never drift.
//!
//! ## Persistence (DECIDED — STORY-194)
//!
//! Settings PERSIST to `~/.config/quizdom/settings.toml` (or
//! `$XDG_CONFIG_HOME/quizdom/`). `$VISUAL`/`$EDITOR` seeds the editor default on
//! the FIRST run only (when no config file exists yet); thereafter the SAVED
//! value wins. The schema is a small flat `key = value` table — UNKNOWN keys are
//! ignored on load (forward-compatible), so a newer quizdom adding a setting
//! never breaks an older one. We hand-roll the tiny parse/serialize so the crate
//! needs no `toml`/`serde`/`dirs` dependency.
//!
//! Belief-NEUTRAL throughout: a setting decides HOW input flows / what chrome is
//! shown, never WHAT is asked or which belief is true.
//!
//! ## The file is shared, so this module owns the whole of it (STORY-258)
//!
//! Keys outside the `/settings` surface live in the same file — `dolt_path`
//! selects the Dolt domain-graph repo, `dolt_backup_path` its file-remote
//! backup directory (STORY-261), `backup_remote` the name of the Dolt remote
//! pointed at it (STORY-326), `auto_backup` opts into pushing to that backup
//! when a writing session ends and `log_path` names the diagnostic log (both
//! STORY-299). Three of them — the graph, the auto-backup switch and the log —
//! are DISPLAYED read-only by `/settings` (TASK-262, TASK-320); see
//! [`ReadOnlyRows`]. Two consequences, both handled here so no second
//! reader/writer of the file can drift from this one:
//!
//! * **Foreign keys survive a save.** [`Settings::to_toml_merged`] rewrites the
//!   modelled keys IN PLACE and keeps every other line verbatim, so saving from
//!   `/settings` no longer drops a hand-added `dolt_path` (TASK-218).
//! * **One parser, one resolution chain.** [`config_value`] and
//!   [`Settings::from_toml`] share [`config_entry`], so the same file resolves
//!   identically whichever reader sees it (TASK-222), and
//!   [`resolve_dolt_path`] is the single env/settings/default chain the runtime
//!   store and the `db-init` / `db-migrate` subcommands all call (TASK-228),
//!   with [`resolve_dolt_backup_path`] the same shape for `db-backup` /
//!   `db-restore`.
//!
//! ## The file is `.toml`, so values parse like TOML (STORY-290)
//!
//! Three correctness rules the hand-rolled parser now honours, because the file
//! is one a human hand-edits:
//!
//! * **Values are TOML-legal.** [`parse_value`] unwraps DOUBLE *and* SINGLE
//!   quotes and ends an unquoted value at an inline `# comment` (TASK-265).
//!   Before that, `dolt_path = "/mnt/data"  # the big disk` resolved to the
//!   literal string *including* the comment — and `db-init` would cheerfully
//!   CREATE a repo at that garbage path.
//! * **A double-quoted value is a TOML BASIC string** (TASK-307), so its
//!   backslash escapes are processed and the closing quote is the first
//!   UNESCAPED one; a single-quoted value is a LITERAL string, taken verbatim.
//! * **A leading `~` expands to `$HOME`** (TASK-307), the way it does in every
//!   shell and every other config file the user writes — see [`expand_tilde`].
//!   `dolt_path = "~/graphs/main"` used to name a literal `~` directory.
//! * **Relative paths are ANCHORED to this file's directory** (TASK-263), not
//!   to the process cwd — see [`anchor_to_config_dir`] for the rule and why.
//! * **A save never silently loses keys.** [`merged_body`] refuses to write when
//!   the existing file is present-but-unreadable, rather than degrading to a
//!   fresh write that would drop exactly the foreign keys TASK-218 taught the
//!   merge to preserve (TASK-268).

use crate::db_init::DEFAULT_DOLT_DB_PATH;
use crate::editor::{editor_model_from_editor, EditorModel};
use crate::strategy::SessionMode;
use std::env;
use std::path::{Path, PathBuf};

/// Which free-text editor model the user has CHOSEN at runtime. Distinct from
/// [`EditorModel`] (the RESOLVED Emacs/Vim layer the editor runs) because the
/// user can pick [`EditorChoice::Auto`] — "follow `$EDITOR`" — which only
/// resolves to a concrete model when the editor is built.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub(crate) enum EditorChoice {
    /// Emacs / readline keybindings (explicit).
    Emacs,
    /// Vim modal editing (explicit).
    Vim,
    /// Follow `$VISUAL`/`$EDITOR` — the STORY-180 inference. The default on a
    /// first run before any explicit choice is saved.
    #[default]
    Auto,
}

impl EditorChoice {
    /// The config/`/editor` token for this choice (`emacs` / `vim` / `auto`).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Emacs => "emacs",
            Self::Vim => "vim",
            Self::Auto => "auto",
        }
    }

    /// The human label shown in the `/settings` panel.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Emacs => "Emacs",
            Self::Vim => "Vim",
            Self::Auto => "Auto",
        }
    }

    /// Parse an `/editor <value>` token (case-insensitive). `readline` is accepted
    /// as a friendly alias for Emacs; `vi`/`nvim` for Vim. Returns `None` for an
    /// unrecognized token so the caller can report a usage hint.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "emacs" | "readline" => Some(Self::Emacs),
            "vim" | "vi" | "nvim" => Some(Self::Vim),
            "auto" | "$editor" | "default" => Some(Self::Auto),
            _ => None,
        }
    }

    /// Cycle to the NEXT choice (the panel's in-place toggle): Emacs → Vim → Auto
    /// → Emacs.
    pub(crate) fn cycle(self) -> Self {
        match self {
            Self::Emacs => Self::Vim,
            Self::Vim => Self::Auto,
            Self::Auto => Self::Emacs,
        }
    }

    /// Resolve this choice to a concrete [`EditorModel`] for building the editor.
    /// `Auto` infers from `$VISUAL`/`$EDITOR` (the STORY-180 logic); the explicit
    /// choices map straight through. `env_editor` is the resolved `$EDITOR` value
    /// (passed in so the resolution is testable without touching the environment).
    pub(crate) fn resolve(self, env_editor: &str) -> EditorModel {
        match self {
            Self::Emacs => EditorModel::Emacs,
            Self::Vim => EditorModel::Vim,
            Self::Auto => editor_model_from_editor(env_editor),
        }
    }
}

/// The runtime-adjustable session preferences — the model behind `/settings`.
///
/// One struct shared by the panel AND the shortcut commands so they stay in sync:
/// `/editor vim` and the panel's editor row mutate the SAME `editor` field, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Settings {
    /// The free-text editor model choice (Emacs / Vim / Auto).
    pub(crate) editor: EditorChoice,
    /// Mouse capture ON/OFF (STORY-193). Default ON.
    pub(crate) mouse: bool,
    /// The persistent distance-to-goal / roundedness gauge ON/OFF (STORY-174).
    /// Default OFF.
    pub(crate) score: bool,
    /// The session questioning mode (Socratic / Debate, EPIC-158).
    pub(crate) mode: SessionMode,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            editor: EditorChoice::default(),
            // trace:STORY-193 | ai:claude — mouse capture is ON by default (DECIDED).
            mouse: true,
            // trace:STORY-174 | ai:claude — the score gauge defaults OFF.
            score: false,
            mode: SessionMode::default(),
        }
    }
}

// trace:STORY-367 | ai:claude
/// The two settings the ENGINE owns and a session can hold a TRANSIENT override
/// of: the score gauge and the questioning mode.
///
/// They travel to the front-end so the `/settings` surface DISPLAYS what the
/// session is actually doing — `quizdom start --mode debate` runs a debate even
/// when the file says `mode = "socratic"`. They must never travel any further:
/// a CLI override is a choice about THIS session, not a new default, and the
/// route that carried one to disk is what STORY-367 closed. Everything reaching
/// `settings.toml` goes through [`Settings::adopt`] instead, one explicitly
/// changed key at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LiveSettings {
    pub(crate) score: bool,
    pub(crate) mode: SessionMode,
}

/// The config keys [`Settings`] models, in write order. Every OTHER key in the
/// file is foreign — read by someone else (`dolt_path`) or written by a future
/// version — and [`Settings::to_toml_merged`] preserves it untouched.
const MODELLED_KEYS: [&str; 4] = ["editor", "mouse", "score", "mode"];

/// The comment line at the top of a freshly written config file.
const CONFIG_HEADER: &str =
    "# quizdom settings (STORY-194) — edited live by /settings; other keys are preserved\n";

// trace:TASK-262 | ai:claude
/// The label of the read-only `dolt_path` row `/settings` shows below the four
/// toggles. `dolt_path` selects WHICH domain graph the session reads, so leaving
/// it invisible after STORY-258 took the preserve-foreign-lines option meant the
/// single most consequential value in the file had no surface at all.
pub(crate) const DOLT_PATH_ROW_LABEL: &str = "Domain graph:";

// trace:TASK-320 | ai:claude
/// The label of the read-only `auto_backup` row. A durability control is the
/// worst category of setting to hide: a user who believes the push is on and a
/// user who believes it is off behave identically until the disk dies.
pub(crate) const AUTO_BACKUP_ROW_LABEL: &str = "Auto-backup:";

// trace:TASK-320 | ai:claude
/// The label of the read-only `log_path` row. Doubles as the answer to "where
/// do I look when something degraded?" — until now that answer lived only in
/// `OVERVIEW.md` prose, which is not where someone mid-session looks.
pub(crate) const LOG_PATH_ROW_LABEL: &str = "Diagnostics:";

// trace:TASK-320 | ai:claude
/// The values `/settings` DISPLAYS but cannot change — each one resolved
/// through its own env > settings > default chain, each one consequential
/// enough that an invisible value is a bug (which graph am I reading? will my
/// work be pushed? where does a failure get recorded?).
///
/// Passed in rather than resolved by the renderers so both surfaces — the
/// headless value list and the TUI panel — draw the SAME rows from ONE
/// computation, and so the rendering stays pure and testable without the
/// ambient environment leaking in (the TASK-262 pattern, widened).
#[derive(Debug, Clone)]
pub(crate) struct ReadOnlyRows {
    /// The resolved domain-graph repo (`dolt_path`).
    pub(crate) dolt_path: PathBuf,
    /// Whether a writing session pushes to its backup on the way out
    /// (`auto_backup`).
    pub(crate) auto_backup: bool,
    /// The resolved diagnostic log (`log_path`).
    pub(crate) log_path: PathBuf,
}

impl ReadOnlyRows {
    /// Resolve all three through the live env > settings > default chains.
    ///
    // trace:TASK-306 | ai:claude
    /// **Under `cfg(test)` this returns [`ReadOnlyRows::hermetic`] without
    /// reading the environment or the disk** — the TASK-266 pattern that
    /// `load_or_seed` / `save` already run on, applied to the last reader of the
    /// real user config left in the crate. [`Settings::render_list`] calls this,
    /// and `render_list` is on the `/settings` path the front-end tests drive, so
    /// the developer's own `~/.config/quizdom/settings.toml` decided what those
    /// tests saw: a `dolt_path` line on one machine and not another. The
    /// resolution itself stays covered — the chain tests drive `dolt_path_from` /
    /// `tiered_path` directly, which is where the behaviour lives.
    pub(crate) fn resolved() -> Self {
        if cfg!(test) {
            return Self::hermetic();
        }
        Self {
            dolt_path: resolve_dolt_path(),
            auto_backup: resolve_auto_backup(),
            log_path: resolve_log_path(),
        }
    }

    // trace:TASK-306 | ai:claude
    /// The fixed rows [`ReadOnlyRows::resolved`] hands back under `cfg(test)`,
    /// and the fixture the row tests render. Deliberately recognisable: a value
    /// from here showing up in real output is a bug someone can name on sight.
    pub(crate) fn hermetic() -> Self {
        Self {
            dolt_path: PathBuf::from("/home/someone/graphs/quizdom"),
            auto_backup: true,
            log_path: PathBuf::from("/home/someone/logs/quizdom.log"),
        }
    }

    /// The rows as `label` + `value` pairs, in display order. The TUI draws
    /// them as `Line`s and the headless list joins them, so the two surfaces
    /// cannot drift in content or order.
    pub(crate) fn rows(&self) -> [(&'static str, String); 3] {
        [
            (DOLT_PATH_ROW_LABEL, self.dolt_path.display().to_string()),
            (AUTO_BACKUP_ROW_LABEL, on_off(self.auto_backup).to_string()),
            (LOG_PATH_ROW_LABEL, self.log_path.display().to_string()),
        ]
    }

    /// The rows rendered one per line, at the same column as the toggle rows
    /// above them.
    pub(crate) fn lines(&self) -> Vec<String> {
        self.rows()
            .into_iter()
            .map(|(label, value)| format!("  {label:<14}{value}"))
            .collect()
    }
}

/// The four settings the `/settings` panel rows toggle/cycle in place.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum SettingKey {
    Editor,
    Mouse,
    Score,
    Mode,
}

impl SettingKey {
    /// The panel rows in display order.
    pub(crate) fn order() -> [SettingKey; 4] {
        [
            SettingKey::Editor,
            SettingKey::Mouse,
            SettingKey::Score,
            SettingKey::Mode,
        ]
    }

    /// The row label shown on the left of the `/settings` panel.
    pub(crate) fn label(self) -> &'static str {
        match self {
            SettingKey::Editor => "Editor mode",
            SettingKey::Mouse => "Mouse",
            SettingKey::Score => "Score gauge",
            SettingKey::Mode => "Session mode",
        }
    }

    /// Parse a `/settings set <key> ...` token to a [`SettingKey`].
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "editor" => Some(SettingKey::Editor),
            "mouse" => Some(SettingKey::Mouse),
            "score" => Some(SettingKey::Score),
            "mode" => Some(SettingKey::Mode),
            _ => None,
        }
    }
}

impl Settings {
    /// The current VALUE label for a setting row (right column of the panel).
    pub(crate) fn value_label(&self, key: SettingKey) -> String {
        match key {
            SettingKey::Editor => self.editor.label().to_string(),
            SettingKey::Mouse => on_off(self.mouse).to_string(),
            SettingKey::Score => on_off(self.score).to_string(),
            SettingKey::Mode => mode_label(self.mode).to_string(),
        }
    }

    /// CYCLE/TOGGLE a setting in place (the panel's Enter/Space on a row, and the
    /// shortcut commands route through this too). Editor and Mode cycle through
    /// their variants; Mouse and Score flip.
    pub(crate) fn cycle(&mut self, key: SettingKey) {
        match key {
            SettingKey::Editor => self.editor = self.editor.cycle(),
            SettingKey::Mouse => self.mouse = !self.mouse,
            SettingKey::Score => self.score = !self.score,
            SettingKey::Mode => {
                self.mode = match self.mode {
                    SessionMode::Socratic => SessionMode::Debate,
                    SessionMode::Debate => SessionMode::Socratic,
                }
            }
        }
    }

    /// Set a setting from a `/settings set <key> <value>` token (the headless
    /// line path). Returns `false` for an unparseable value so the caller can
    /// surface a usage hint.
    pub(crate) fn set_from_token(&mut self, key: SettingKey, value: &str) -> bool {
        match key {
            SettingKey::Editor => match EditorChoice::parse(value) {
                Some(choice) => {
                    self.editor = choice;
                    true
                }
                None => false,
            },
            SettingKey::Mouse => match parse_on_off(value) {
                Some(on) => {
                    self.mouse = on;
                    true
                }
                None => false,
            },
            SettingKey::Score => match parse_on_off(value) {
                Some(on) => {
                    self.score = on;
                    true
                }
                None => false,
            },
            SettingKey::Mode => match SessionMode::parse(value) {
                Some(mode) => {
                    self.mode = mode;
                    true
                }
                None => false,
            },
        }
    }

    // trace:STORY-367 | ai:claude
    /// Take ONE key's value from `live` — the front-end's live/display copy —
    /// into `self`, the copy that gets written to `settings.toml`.
    ///
    /// This is the only door into the persisted struct, and it is deliberately
    /// one key wide. The two front-ends mirror the engine's live `score`/`mode`
    /// into their display copy so the panel shows the truth; before STORY-367
    /// they persisted that same copy WHOLE, so a `/settings set editor vim`
    /// under `--mode debate` wrote `mode = "debate"` over the user's saved
    /// default as a side effect of changing the editor. Copying only the key the
    /// user actually changed makes that impossible to write by accident.
    pub(crate) fn adopt(&mut self, key: SettingKey, live: Settings) {
        match key {
            SettingKey::Editor => self.editor = live.editor,
            SettingKey::Mouse => self.mouse = live.mouse,
            SettingKey::Score => self.score = live.score,
            SettingKey::Mode => self.mode = live.mode,
        }
    }

    /// Render the panel as a printed list of `label: value` rows (the HEADLESS
    /// degradation of the TUI panel, and the body of the `/settings` line echo),
    /// followed by the read-only [`ReadOnlyRows`] (TASK-262, TASK-320).
    pub(crate) fn render_list(&self) -> String {
        self.render_list_showing(&ReadOnlyRows::resolved())
    }

    // trace:TASK-262 | ai:claude — widened to all three read-only rows by
    // TASK-320.
    /// [`Settings::render_list`] with the read-only values passed IN, so the
    /// rendering stays pure and testable while the public entry point resolves
    /// the live ones.
    fn render_list_showing(&self, read_only: &ReadOnlyRows) -> String {
        let mut out = String::from("Settings\n");
        for key in SettingKey::order() {
            out.push_str(&format!(
                "  {:<14}{}\n",
                format!("{}:", key.label()),
                self.value_label(key)
            ));
        }
        for line in read_only.lines() {
            out.push_str(&line);
            out.push('\n');
        }
        out.push_str(
            "  (toggle with /editor, /mouse, /score, /mode — or /settings set <key> <value>;\n   \
             the three rows above are read-only here — set dolt_path / auto_backup / log_path\n   \
             in settings.toml, or $QUIZDOM_DOLT_PATH / $QUIZDOM_AUTO_BACKUP / $QUIZDOM_LOG_PATH;\n   \
             read the diagnostics with `quizdom logs`)\n",
        );
        out
    }

    /// Serialize to the small flat config schema (`key = value` per line) as a
    /// FRESH file. Only the modelled keys are written — use
    /// [`Settings::to_toml_merged`] when a file already exists, or the keys it
    /// carries that this struct does not model are lost.
    /// Takes `self` by value ([`Settings`] is `Copy`).
    pub(crate) fn to_toml(self) -> String {
        let mut out = String::from(CONFIG_HEADER);
        for key in MODELLED_KEYS {
            out.push_str(&self.modelled_line(key));
        }
        out
    }

    // trace:TASK-218 | ai:claude
    /// Serialize OVER the `existing` file text: each modelled key is rewritten
    /// IN PLACE with the current value, and every other line — comments, blanks,
    /// and foreign keys like `dolt_path` — is kept verbatim. Modelled keys the
    /// file omits are appended. An empty (or absent) file degrades to
    /// [`Settings::to_toml`].
    ///
    /// This is what keeps `/settings` from silently dropping a hand-added
    /// `dolt_path` line: STORY-194's schema promises unknown keys are ignored on
    /// LOAD, and the save path has to honour the other half of that bargain.
    pub(crate) fn to_toml_merged(self, existing: &str) -> String {
        if existing.trim().is_empty() {
            return self.to_toml();
        }
        let mut rewritten: Vec<String> = Vec::with_capacity(MODELLED_KEYS.len());
        let mut out = String::new();
        for line in existing.lines() {
            let modelled = config_entry(line)
                .map(|(key, _)| key)
                .filter(|key| MODELLED_KEYS.contains(&key.as_str()));
            match modelled {
                // A modelled key: rewrite the first occurrence, drop any later
                // duplicate — `from_toml` is last-wins, so leaving a stale
                // duplicate behind would resurrect it on the next load.
                Some(key) => {
                    if !rewritten.contains(&key) {
                        out.push_str(&self.modelled_line(&key));
                        rewritten.push(key);
                    }
                }
                None => {
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
        for key in MODELLED_KEYS {
            if !rewritten.iter().any(|written| written == key) {
                out.push_str(&self.modelled_line(key));
            }
        }
        out
    }

    /// The `key = value` line for one modelled key — the single renderer behind
    /// both [`Settings::to_toml`] and [`Settings::to_toml_merged`], so a fresh
    /// write and an in-place rewrite can never format a value differently.
    ///
    /// `None` for a key this function does not know how to render. Every entry
    /// in [`MODELLED_KEYS`] must yield `Some`; see [`Settings::modelled_line`].
    fn rendered_line(self, key: &str) -> Option<String> {
        match key {
            "editor" => Some(format!("editor = \"{}\"\n", self.editor.as_str())),
            "mouse" => Some(format!("mouse = {}\n", self.mouse)),
            "score" => Some(format!("score = {}\n", self.score)),
            "mode" => Some(format!("mode = \"{}\"\n", self.mode.as_str())),
            _ => None,
        }
    }

    // trace:TASK-267 | ai:claude
    /// [`Settings::rendered_line`] for a key that is KNOWN to be modelled — the
    /// form both serializers call.
    ///
    /// The `None` arm is the maintenance trap TASK-267 was filed against: before
    /// this split, `rendered_line`'s catch-all returned an empty string, so
    /// adding a fifth entry to [`MODELLED_KEYS`] and forgetting the matching arm
    /// would DROP that key from every file quizdom ever wrote — no compile
    /// error, no panic, no log line. Now it trips a debug assertion here and
    /// fails `every_modelled_key_renders_a_line` in release too, which is the
    /// acceptance STORY-290 asked for: the omission fails a test rather than
    /// losing data.
    fn modelled_line(self, key: &str) -> String {
        match self.rendered_line(key) {
            Some(line) => line,
            None => {
                debug_assert!(
                    false,
                    "MODELLED_KEYS entry `{key}` has no `rendered_line` arm — it would be \
                     silently dropped from every saved settings file"
                );
                String::new()
            }
        }
    }

    /// Parse the config schema, IGNORING unknown keys and unparseable values
    /// (forward-compatible: a newer file with extra keys still loads). Any key the
    /// file omits keeps the [`Default`] value, so a partial/old file round-trips.
    /// A repeated key is LAST-wins — [`config_value`] matches that.
    pub(crate) fn from_toml(text: &str) -> Self {
        let mut settings = Settings::default();
        for line in text.lines() {
            let Some((key, value)) = config_entry(line) else {
                continue;
            };
            match key.as_str() {
                "editor" => {
                    if let Some(choice) = EditorChoice::parse(&value) {
                        settings.editor = choice;
                    }
                }
                "mouse" => {
                    if let Some(on) = parse_on_off(&value) {
                        settings.mouse = on;
                    }
                }
                "score" => {
                    if let Some(on) = parse_on_off(&value) {
                        settings.score = on;
                    }
                }
                "mode" => {
                    if let Some(mode) = SessionMode::parse(&value) {
                        settings.mode = mode;
                    }
                }
                // Unknown key — ignore (forward-compatible schema).
                _ => {}
            }
        }
        settings
    }
}

// trace:TASK-222 | ai:claude
/// Parse ONE line of the flat config schema into a normalised
/// `(key, value)` — key lowercased, value read through [`parse_value`] (quotes
/// off, inline comment off). Comments, blank lines and lines without an `=`
/// yield `None`.
///
/// The single line-parser for this file: both [`Settings::from_toml`] and
/// [`config_value`] go through it, so `Store = "dolt"` and `store = dolt`
/// cannot resolve differently depending on which reader looks. (Before
/// TASK-222 the two readers disagreed on case AND on quote stripping —
/// `config_value` used `trim_matches('"')`, which also ate unmatched and
/// repeated quotes.)
fn config_entry(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (raw_key, raw_value) = line.split_once('=')?;
    Some((raw_key.trim().to_ascii_lowercase(), parse_value(raw_value)))
}

// trace:TASK-222 | ai:claude
/// Read one key out of the flat config schema, matching [`Settings::from_toml`]
/// exactly — both go through [`config_entry`], so both get case-insensitive
/// keys and the same [`parse_value`] reading of the value (TOML quoting, inline
/// comments, escapes). A repeated key is LAST occurrence wins. Returns `None`
/// when the key is absent.
//
// trace:TASK-305 | ai:claude — this used to advertise "matched-pair unquoting",
// which TASK-265 replaced with a TOML-legal parse; the stale line described a
// degrade path (`strip_matched_double_quotes`) as if it were the rule.
pub(crate) fn config_value(text: &str, key: &str) -> Option<String> {
    let key = key.to_ascii_lowercase();
    text.lines()
        .filter_map(config_entry)
        .rfind(|(name, _)| *name == key)
        .map(|(_, value)| value)
}

// trace:TASK-228 | ai:claude
/// THE resolution chain for the Dolt domain-graph repo path:
/// `QUIZDOM_DOLT_PATH` (env) > `dolt_path` (settings.toml) >
/// [`DEFAULT_DOLT_DB_PATH`]. Blank values fall through to the next tier.
///
/// One helper, three callers — the runtime store
/// ([`crate::domain_store_from_config`]) plus the `db-init` / `db-migrate`
/// subcommands. The CLI `--path` flag sits ON TOP: each subcommand's arg parser
/// takes this as its default and lets `--path` override it, so the full
/// precedence is flag > env > settings > default. Before TASK-228 the two
/// subcommands skipped the middle two tiers entirely, so
/// `QUIZDOM_DOLT_PATH=/tmp/x quizdom db-init` bootstrapped `data/dolt` and the
/// next session read `/tmp/x` and found nothing.
pub(crate) fn resolve_dolt_path() -> PathBuf {
    dolt_path_from(
        env::var("QUIZDOM_DOLT_PATH").ok().as_deref(),
        &config_text(),
        config_dir().as_deref(),
    )
}

/// The pure tier-selection behind [`resolve_dolt_path`], split from the env and
/// file reads so it is testable without touching the process environment (the
/// same pattern as [`EditorChoice::resolve`]).
fn dolt_path_from(env_path: Option<&str>, config: &str, config_dir: Option<&Path>) -> PathBuf {
    tiered_path(
        env_path,
        config,
        config_dir,
        "dolt_path",
        PathBuf::from(DEFAULT_DOLT_DB_PATH),
    )
}

// trace:STORY-261 | ai:claude — TASK-243's backup directory, same chain shape.
/// THE resolution chain for the Dolt BACKUP directory (the file remote
/// `quizdom db-backup` pushes to and `db-restore` clones from):
/// `QUIZDOM_DOLT_BACKUP_PATH` (env) > `dolt_backup_path` (settings.toml) >
/// [`default_dolt_backup_path`]. `--to` / `--from` sit on top, exactly as
/// `--path` does over [`resolve_dolt_path`].
pub(crate) fn resolve_dolt_backup_path() -> PathBuf {
    tiered_path(
        env::var("QUIZDOM_DOLT_BACKUP_PATH").ok().as_deref(),
        &config_text(),
        config_dir().as_deref(),
        "dolt_backup_path",
        default_dolt_backup_path(),
    )
}

/// The platform default backup directory: `$XDG_DATA_HOME/quizdom/dolt-backup`,
/// else `$HOME/.local/share/quizdom/dolt-backup`. Deliberately OUTSIDE the
/// project tree — a backup that lives under `data/` alongside the repo it
/// protects is no backup at all against the common `rm -rf data/` accident.
/// With neither var set (no home to speak of) it degrades to a sibling of the
/// repo, which still survives deleting `data/dolt` itself.
fn default_dolt_backup_path() -> PathBuf {
    user_data_dir()
        .map(|base| base.join("dolt-backup"))
        .unwrap_or_else(|| PathBuf::from("data/dolt-backup"))
}

// trace:STORY-299 | ai:claude
/// THE resolution chain for the diagnostic log [`crate::diagnostics`] appends
/// to: `QUIZDOM_LOG_PATH` (env) > `log_path` (settings.toml) >
/// [`default_log_path`].
///
/// Same shape as the two Dolt paths on purpose, so the one rule a user has to
/// learn covers every path quizdom resolves — including TASK-263's anchoring,
/// which matters here for the same reason: a relative `log_path` in the
/// per-user settings file would otherwise name a different log from every
/// worktree.
pub(crate) fn resolve_log_path() -> PathBuf {
    tiered_path(
        env::var("QUIZDOM_LOG_PATH").ok().as_deref(),
        &config_text(),
        config_dir().as_deref(),
        "log_path",
        default_log_path(),
    )
}

/// `$XDG_DATA_HOME/quizdom/quizdom.log`, else
/// `$HOME/.local/share/quizdom/quizdom.log` — the SAME user data directory the
/// default backup lives under, so everything quizdom keeps outside the project
/// tree is in one place to find (and one place to clear).
fn default_log_path() -> PathBuf {
    user_data_dir()
        .map(|base| base.join("quizdom.log"))
        .unwrap_or_else(|| PathBuf::from("data/quizdom.log"))
}

/// `$XDG_DATA_HOME/quizdom`, else `$HOME/.local/share/quizdom` — the user data
/// directory shared by the default backup path and the default log path.
/// `None` when neither var is set (no home to speak of); each caller then picks
/// its own in-project degrade.
fn user_data_dir() -> Option<PathBuf> {
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| {
            env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("share"))
        })
        .map(|base| base.join("quizdom"))
}

// trace:STORY-384 | ai:claude — the session-history durability path, given the
// same env > settings > default shape as the graph's backup directory.
/// THE resolution chain for the per-user SESSION-HISTORY backup directory (the
/// plain-filesystem mirror `quizdom db-backup` writes and `db-restore` reads):
/// `QUIZDOM_USERS_BACKUP_PATH` (env) > `users_backup_path` (settings.toml) >
/// [`default_users_backup_path`]. `--users-to` / `--users-from` sit on top,
/// exactly as `--to` / `--from` do over [`resolve_dolt_backup_path`].
///
/// A SIBLING of the Dolt backup directory, never inside it: that directory is a
/// Dolt file-remote with its own manifest, and dropping a tree of JSONL beside
/// its objects would corrupt it. Session history is flat JSONL, not Dolt, so
/// the mirror is a directory copy rather than a push.
pub(crate) fn resolve_users_backup_path() -> PathBuf {
    tiered_path(
        env::var("QUIZDOM_USERS_BACKUP_PATH").ok().as_deref(),
        &config_text(),
        config_dir().as_deref(),
        "users_backup_path",
        default_users_backup_path(),
    )
}

/// The platform default session-history backup directory:
/// `$XDG_DATA_HOME/quizdom/users-backup`, else
/// `$HOME/.local/share/quizdom/users-backup` — a SIBLING of
/// [`default_dolt_backup_path`], the same reasoning: outside the project tree so
/// an `rm -rf data/` cannot take the backup with it, and beside the graph backup
/// so one place holds everything quizdom keeps off to the side.
fn default_users_backup_path() -> PathBuf {
    user_data_dir()
        .map(|base| base.join("users-backup"))
        .unwrap_or_else(|| PathBuf::from("data/users-backup"))
}

// trace:STORY-384 | ai:claude
/// The SOURCE session tree `db-backup` mirrors and `db-restore` restores into:
/// `data/users`, cwd-relative. This is not env/settings-configurable, because it
/// is not resolved anywhere else either — [`crate::session`] and
/// [`crate::transcript`] both write session logs to a literal `data/users`
/// relative to the process cwd, so the backup source has to name that exact
/// tree or it would carry nothing.
pub(crate) fn resolve_users_dir() -> PathBuf {
    PathBuf::from("data").join("users")
}

// trace:STORY-299 | ai:claude — TASK-273's opt-in half.
/// Whether a session that WROTE to the domain graph should push to the backup
/// remote on its way out: `QUIZDOM_AUTO_BACKUP` (env) > `auto_backup`
/// (settings.toml) > **off**.
///
/// Off is the default, and that is the decision rather than an oversight
/// (STORY-299). A push is seconds of `dolt` spawns against a directory that may
/// be a removable disk or a synced folder, so performing one implicitly at the
/// end of every writing session spends the user's time and can fail in ways
/// that muddy the end of a session. `quizdom db-backup` stays THE primitive.
/// What the default costs — a backup nobody remembers to run — is answered by
/// the reminder ([`crate::db_backup::session_end_durability`]), not by flipping
/// this on for people.
///
/// Anyone who wants the push does opt in, with one line in `settings.toml`
/// (`auto_backup = true`) or `QUIZDOM_AUTO_BACKUP=1` for one shell.
pub(crate) fn resolve_auto_backup() -> bool {
    auto_backup_from(
        env::var("QUIZDOM_AUTO_BACKUP").ok().as_deref(),
        &config_text(),
    )
}

// trace:TASK-324 | ai:claude
/// THE resolution chain for the NAME of the Dolt remote `db-backup` pushes to:
/// `QUIZDOM_BACKUP_REMOTE` (env) > `backup_remote` (settings.toml) >
/// [`crate::db_backup::BACKUP_REMOTE_NAME`]. `--remote` sits on top, exactly as
/// `--path` does over [`resolve_dolt_path`].
///
/// The chain exists because the PROBE and the PUSH have to agree. `db-backup`
/// has always accepted `--remote <name>`, but the end-of-session probe read the
/// tracking ref for the hardcoded default — so an operator who backs up under
/// another name never populates `remotes/backup/main`, the probe reads the
/// missing ref as "never backed up" (the right call for a default-named
/// remote), and the reminder then fires after every writing session including
/// seconds after a successful backup. A reminder that is always wrong is worse
/// than no reminder: it trains the user to skip the line that matters.
pub(crate) fn resolve_backup_remote() -> String {
    backup_remote_from(
        env::var("QUIZDOM_BACKUP_REMOTE").ok().as_deref(),
        &config_text(),
    )
}

/// The pure tier selection behind [`resolve_backup_remote`]. Blank values fall
/// through to the next tier, matching [`tiered_path`] — an exported-but-empty
/// variable must not name the empty remote.
fn backup_remote_from(env_value: Option<&str>, config: &str) -> String {
    fn non_blank(value: &str) -> Option<String> {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }
    env_value
        .and_then(non_blank)
        .or_else(|| {
            config_value(config, "backup_remote")
                .as_deref()
                .and_then(non_blank)
        })
        .unwrap_or_else(|| crate::db_backup::BACKUP_REMOTE_NAME.to_string())
}

/// The pure tier selection behind [`resolve_auto_backup`], split from the env
/// and file reads exactly as [`dolt_path_from`] is. An UNPARSEABLE value at a
/// tier falls through to the next rather than reading as `false`, so a typo
/// (`auto_backup = ture`) cannot silently disable a backup the user asked for
/// in the environment.
fn auto_backup_from(env_value: Option<&str>, config: &str) -> bool {
    env_value
        .and_then(parse_on_off)
        .or_else(|| {
            config_value(config, "auto_backup")
                .as_deref()
                .and_then(parse_on_off)
        })
        .unwrap_or(false)
}

/// The shared env > settings > default selection both Dolt paths use. Blank
/// values fall through to the next tier so an exported-but-empty variable
/// cannot silently repoint the app at `""`. Only the SETTINGS tier is anchored
/// ([`anchor_to_config_dir`]) — the env tier and the compiled default are
/// per-invocation / per-checkout and stay cwd-relative. Both WRITTEN tiers get
/// [`expand_tilde`]; the compiled default has no `~` to expand.
fn tiered_path(
    env_path: Option<&str>,
    config: &str,
    config_dir: Option<&Path>,
    key: &str,
    default: PathBuf,
) -> PathBuf {
    fn non_blank(value: &str) -> Option<PathBuf> {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| expand_tilde(trimmed))
    }
    if let Some(from_env) = env_path.and_then(non_blank) {
        return from_env;
    }
    match config_value(config, key).as_deref().and_then(non_blank) {
        Some(from_config) => anchor_to_config_dir(from_config, config_dir),
        None => default,
    }
}

// trace:TASK-263 | ai:claude
/// ANCHOR a relative path read out of `settings.toml` to the directory that file
/// lives in (`~/.config/quizdom/`), leaving absolute paths alone.
///
/// **The rule and why it is this one.** `settings.toml` is per-USER and GLOBAL —
/// one file shared by every checkout, every sibling worktree, and every shell.
/// Before TASK-263 a relative `dolt_path` came back unanchored and the callers
/// handed it straight to `Command::current_dir` / `Path::join`, so it resolved
/// against the PROCESS cwd: one config line selected a DIFFERENT domain graph
/// from each worktree, silently. Anchoring to the settings file's own directory
/// is the only base that is as global as the file itself; anchoring to "the
/// project root" would have reproduced the bug, since each worktree has its own.
///
/// The other two tiers deliberately stay cwd-relative, because both are named
/// per-invocation by someone who can see their own cwd:
///
/// * `$QUIZDOM_DOLT_PATH` / `$QUIZDOM_DOLT_BACKUP_PATH` (and the `--path` /
///   `--to` / `--from` flags that sit above them) — a shell-local choice.
/// * the compiled default `data/dolt` — deliberately per-checkout: it is the
///   gitignored local graph each worktree gets for free.
fn anchor_to_config_dir(path: PathBuf, config_dir: Option<&Path>) -> PathBuf {
    match config_dir {
        Some(dir) if path.is_relative() => dir.join(path),
        _ => path,
    }
}

// trace:TASK-307 | ai:claude
/// EXPAND a leading `~` to the user's home directory, the way every shell and
/// every other config file the user writes does.
///
/// `~/graphs/main` is exactly the value someone reaches for when they want the
/// graph in their home directory, and before TASK-307 it named a LITERAL `~`
/// directory relative to the settings file — so `db-init` would create
/// `~/.config/quizdom/~/graphs/main` and every later session would read it back
/// without ever mentioning that it was not the path that was asked for.
///
/// Only a leading `~` on its own or followed by a separator expands. `~alice/x`
/// is left alone: resolving another user's home needs the password database, and
/// a value we cannot resolve is better left recognisable than half-translated.
/// With no `$HOME` the value is also left alone — there is nothing to expand to.
fn expand_tilde(value: &str) -> PathBuf {
    expand_tilde_from(value, home_dir().as_deref())
}

/// The pure half of [`expand_tilde`], split from the env read so it is testable
/// without touching the process environment.
fn expand_tilde_from(value: &str, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return PathBuf::from(value);
    };
    match value {
        "~" => home.to_path_buf(),
        _ => match value.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(value),
        },
    }
}

/// `$HOME`, or `None` when it is unset or empty.
fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// The settings file's text, or empty when it is absent / unreadable — the
/// read half of [`resolve_dolt_path`], kept separate so the selection above
/// stays pure.
fn config_text() -> String {
    config_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default()
}

/// The platform config path for the settings file:
/// `$XDG_CONFIG_HOME/quizdom/settings.toml`, else `$HOME/.config/quizdom/...`.
/// Returns `None` only when neither var is set (the settings then stay in-memory
/// for the session and simply do not persist — a graceful, never-fatal degrade).
pub(crate) fn config_path() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("quizdom").join("settings.toml"))
}

// trace:TASK-263 | ai:claude
/// The DIRECTORY holding the settings file — the anchor base a relative
/// `dolt_path` / `dolt_backup_path` resolves against (see
/// [`anchor_to_config_dir`]).
fn config_dir() -> Option<PathBuf> {
    config_path().and_then(|path| path.parent().map(PathBuf::from))
}

// trace:TASK-373 | ai:claude
/// The config file THIS PROCESS persists to — [`config_path`] in a real build,
/// and `None` under `cfg(test)`.
///
/// **The single hermeticity guard.** TASK-266 put it inside `load_or_seed` /
/// `save` as a `cfg(test)` early return, which kept the developer's real
/// `~/.config/quizdom/settings.toml` out of the ~720 in-crate tests — and made
/// the disk path unreachable *from a test at all*. STORY-367's first acceptance
/// criterion was "the persisted value survives a session that overrode it", and
/// no test could assert it literally: the only thing in-crate that could be
/// checked was the model one level in (`persisted_settings`), a faithful proxy
/// but still a proxy. A bug in [`save`] itself would have passed.
///
/// Moving the guard here makes it a question about WHICH PATH rather than about
/// whether IO happens: the front-ends resolve their config path through this
/// once at construction, and [`load_or_seed_at`] / [`save_at`] below do real IO
/// against whatever path they are handed. Tests hand them a temp directory and
/// round-trip a real file (see `a_session_override_never_reaches_the_file`);
/// production hands them the user's config; nothing hands them the user's
/// config *during a test*.
pub(crate) fn process_config_path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    config_path()
}

/// LOAD the persisted settings, or SEED a first run from `$EDITOR`/`$VISUAL`.
///
/// * If the config file EXISTS, parse it (saved value wins, unknown keys ignored).
/// * If it does NOT exist (first run), seed the editor choice from
///   `$VISUAL`/`$EDITOR`: a vi-family editor seeds [`EditorChoice::Vim`],
///   everything else [`EditorChoice::Emacs`] — so the first-run default matches
///   the old STORY-180 startup inference. Thereafter the saved value wins.
///
/// Never fails: an unreadable / missing path degrades to a seeded default.
///
/// Reads whatever [`process_config_path`] resolves — which is nothing at all
/// under `cfg(test)`, so the developer's own file cannot leak in.
pub(crate) fn load_or_seed() -> Settings {
    load_or_seed_at(process_config_path().as_deref())
}

// trace:TASK-373 | ai:claude
/// [`load_or_seed`] against an EXPLICIT path — the injectable half.
///
/// `None` means there is nowhere to load from (no `$HOME`, or a test that
/// injected no file), and the answer is the plain [`Settings::default`]. The
/// `$EDITOR` seed applies only when a config path EXISTS and holds no file yet,
/// which is what "a first run" actually means; with no config path nothing will
/// ever be persisted, and [`EditorChoice::Auto`] — the default — already
/// re-infers from `$EDITOR` every time the editor is built.
pub(crate) fn load_or_seed_at(path: Option<&Path>) -> Settings {
    let Some(path) = path else {
        return Settings::default();
    };
    match std::fs::read_to_string(path) {
        Ok(text) => Settings::from_toml(&text),
        Err(_) => seed_from_env(),
    }
}

// trace:TASK-218 | ai:claude
/// SAVE the settings to `path` (best-effort, creating the parent dir).
/// Returns `Ok(())` when `path` is `None` — there is nowhere to persist to, so
/// nothing failed. An IO error is returned so the caller can SURFACE it
/// (BUG-378); both front-ends do, rather than dropping it on the floor.
///
/// The write MERGES over whatever is already on disk
/// ([`Settings::to_toml_merged`]) rather than round-tripping the file through
/// the four modelled keys — otherwise the first `/settings` toggle of a session
/// would silently delete a hand-added `dolt_path` line and repoint the app at
/// `data/dolt`.
///
// trace:TASK-373 | ai:claude
/// Taking the path as a PARAMETER is what makes the disk half testable: a
/// front-end resolves it once through [`process_config_path`] (which is `None`
/// under `cfg(test)`, so no test can reach the developer's file by accident),
/// and a test that wants to prove what lands on disk injects a temp path
/// instead of having the IO compiled out from under it.
pub(crate) fn save_at(path: Option<&Path>, settings: &Settings) -> std::io::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = merged_body(settings, std::fs::read_to_string(path))?;
    std::fs::write(path, body)
}

// trace:BUG-378 | ai:claude
/// What to say — and record — when a settings save FAILED.
///
/// STORY-367's persist seam swallowed the error (`let _ = save(…)`), which is
/// the one outcome the user needs told: they asked for a new default, the
/// session applied it, and the file did not change. Silence there is
/// indistinguishable from success, and the next run quietly disagrees with what
/// they were shown.
///
/// The note goes to the diagnostic log here — the one seam for a survivable
/// failure — and is RETURNED so the caller can also put it on whichever surface
/// it owns (the line front-end writes it out; the TUI pushes it into the
/// transcript, since it owns the alternate screen).
pub(crate) fn save_failure_note(path: Option<&Path>, error: &std::io::Error) -> String {
    let where_to = match path {
        Some(path) => path.display().to_string(),
        None => "the settings file".to_string(),
    };
    let note = format!(
        "Could not save settings to {where_to}: {error}. \
         The change applies for this session only."
    );
    crate::diagnostics::record(&note);
    note
}

// trace:TASK-268 | ai:claude
/// Decide WHAT [`save`] writes, given the result of reading the existing file.
/// Split out from the IO so the refuse-vs-preserve decision is testable without
/// having to manufacture an unreadable file (which is not portable, and is not
/// even possible when the suite runs as root).
///
/// Three cases, and the middle one is the bug fix:
///
/// * **Read OK** → merge over it, preserving every foreign line (TASK-218).
/// * **Not found** → no file yet, so a fresh write loses nothing.
/// * **Any other error** (permissions, a directory in the way, an IO fault) →
///   the file EXISTS but we cannot see inside it. Before TASK-268 this fell
///   through `unwrap_or_default()` to an empty string, which
///   [`Settings::to_toml_merged`] treats as "no file" — so the very next
///   `/settings` toggle would OVERWRITE the unreadable file with the four
///   modelled keys and drop exactly the `dolt_path` line TASK-218 was fixed to
///   preserve. REFUSE instead: the caller treats persistence as best-effort, so
///   the setting still applies for the session; it just does not eat the file.
fn merged_body(settings: &Settings, existing: std::io::Result<String>) -> std::io::Result<String> {
    match existing {
        Ok(text) => Ok(settings.to_toml_merged(&text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(settings.to_toml()),
        Err(err) => Err(err),
    }
}

/// First-run seed: editor choice inferred from `$VISUAL`/`$EDITOR`, everything
/// else the [`Default`]. Mirrors STORY-180's startup inference so an existing
/// `$EDITOR=vim` user still gets Vim on their first STORY-194 run.
fn seed_from_env() -> Settings {
    let editor = env::var("VISUAL")
        .ok()
        .or_else(|| env::var("EDITOR").ok())
        .unwrap_or_default();
    let choice = match editor_model_from_editor(&editor) {
        EditorModel::Vim => EditorChoice::Vim,
        EditorModel::Emacs => EditorChoice::Emacs,
    };
    Settings {
        editor: choice,
        ..Settings::default()
    }
}

/// `"On"` / `"Off"` for a boolean setting value label.
fn on_off(on: bool) -> &'static str {
    if on {
        "On"
    } else {
        "Off"
    }
}

/// `"Socratic"` / `"Debate"` for the mode value label.
fn mode_label(mode: SessionMode) -> &'static str {
    match mode {
        SessionMode::Socratic => "Socratic",
        SessionMode::Debate => "Debate",
    }
}

/// Parse a permissive on/off token for the boolean settings.
pub(crate) fn parse_on_off(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Some(true),
        "off" | "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

// trace:TASK-265 | ai:claude
/// Parse ONE config value the way TOML would read it:
///
/// * a **quoted** string — DOUBLE or SINGLE quotes — yields its bare contents,
///   and anything after the closing quote must be blank or a `# comment`;
/// * an **unquoted** value ends at the first `#` (an inline comment);
/// * surrounding whitespace is trimmed either way.
///
/// The file is `settings.toml` and humans hand-edit it, so
/// `dolt_path = "/mnt/data/dolt"  # the big disk` has to resolve to
/// `/mnt/data/dolt`. Before TASK-265 the matched-pair strip never fired on that
/// line (it does not END in a quote), so the value kept the trailing comment —
/// and since TASK-228 routed `db-init` through this same chain, the blast radius
/// was a Dolt repo CREATED at a path with a comment in its name. Single quotes
/// had the mirror problem: they simply survived into the value.
///
/// MALFORMED input stays TOLERANT rather than fatal — the schema's whole promise
/// is that a file it cannot understand still loads. An unterminated or doubled
/// quote falls back to the pre-TASK-265 matched-pair strip, so `""/tmp/x""`
/// still yields `"/tmp/x"` and `"vim` still yields `"vim` (which then fails
/// [`EditorChoice::parse`] and leaves the setting at its default).
///
// trace:TASK-307 | ai:claude
/// A DOUBLE-quoted value is a TOML *basic* string, so its BACKSLASH ESCAPES are
/// processed ([`unescape_basic`]) and the closing quote is the first UNESCAPED
/// one — `dolt_path = "/mnt/say \"yes\"/dolt"` is one value, not a truncated one.
/// A SINGLE-quoted value is a TOML *literal* string and is taken verbatim, which
/// is the escape hatch for a path full of backslashes.
fn parse_value(value: &str) -> String {
    let trimmed = value.trim();
    match trimmed.chars().next() {
        Some(quote @ ('"' | '\'')) => {
            // The closing quote is the next one of the SAME kind (skipping the
            // escaped ones in a basic string); the remainder has to be blank or a
            // comment for this to be a legal TOML value.
            let body = &trimmed[quote.len_utf8()..];
            if let Some(close) = closing_quote(body, quote) {
                let rest = body[close + quote.len_utf8()..].trim();
                if rest.is_empty() || rest.starts_with('#') {
                    let inner = &body[..close];
                    return match quote {
                        '"' => unescape_basic(inner),
                        _ => inner.to_string(),
                    };
                }
            }
            strip_matched_double_quotes(trimmed)
        }
        // A bare value: an inline comment ends it. (A `#` that is part of the
        // value has to be quoted — that is TOML's rule, not ours.)
        _ => trimmed
            .split_once('#')
            .map_or(trimmed, |(before, _)| before)
            .trim()
            .to_string(),
    }
}

// trace:TASK-307 | ai:claude
/// The byte index (within `body`, the text AFTER the opening quote) of the quote
/// that CLOSES it, or `None` when the value is unterminated.
///
/// A LITERAL string (`'…'`) ends at the next quote, full stop — TOML gives it no
/// escape character at all. A BASIC string (`"…"`) ends at the next quote that a
/// backslash does not escape, so `"say \"yes\""` is one value.
fn closing_quote(body: &str, quote: char) -> Option<usize> {
    if quote != '"' {
        return body.find(quote);
    }
    let mut escaped = false;
    for (index, ch) in body.char_indices() {
        match ch {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return Some(index),
            _ => {}
        }
    }
    None
}

// trace:TASK-307 | ai:claude
/// Process the escape sequences of a TOML BASIC string: `\b \t \n \f \r \" \\`
/// and the `\uXXXX` / `\UXXXXXXXX` code points.
///
/// An UNRECOGNIZED (or malformed) escape is kept VERBATIM, backslash and all,
/// rather than being dropped or treated as fatal — the same tolerance the rest of
/// this parser runs on. That is also the friendly reading of the one case a user
/// hits by accident: `dolt_path = "C:\Users\me"` has no legal `\U` escape after
/// it (TOML wants `\\`), and a path that comes back unchanged is far better than
/// one silently missing characters. `'C:\Users\me'` — a literal string — is the
/// spelling that needs no escaping at all.
fn unescape_basic(inner: &str) -> String {
    if !inner.contains('\\') {
        return inner.to_string();
    }
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let Some(escape) = chars.next() else {
            // A trailing lone backslash: keep it.
            out.push('\\');
            break;
        };
        match escape {
            'b' => out.push('\u{8}'),
            't' => out.push('\t'),
            'n' => out.push('\n'),
            'f' => out.push('\u{c}'),
            'r' => out.push('\r'),
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            'u' | 'U' => {
                let width = if escape == 'u' { 4 } else { 8 };
                match take_code_point(&mut chars, width) {
                    Some(decoded) => out.push(decoded),
                    None => {
                        out.push('\\');
                        out.push(escape);
                    }
                }
            }
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

// trace:TASK-307 | ai:claude
/// Read `width` hex digits off `chars` and decode them as a Unicode scalar.
/// `None` — leaving the iterator untouched — when there are not that many hex
/// digits or they do not name a real character, so the caller can keep the
/// sequence verbatim.
fn take_code_point(chars: &mut std::str::Chars<'_>, width: usize) -> Option<char> {
    let digits: Vec<char> = chars.clone().take(width).collect();
    if digits.len() != width || !digits.iter().all(char::is_ascii_hexdigit) {
        return None;
    }
    let digits: String = digits.into_iter().collect();
    let decoded = char::from_u32(u32::from_str_radix(&digits, 16).ok()?)?;
    // Only now that it decoded do we consume the digits.
    for _ in 0..width {
        chars.next();
    }
    Some(decoded)
}

/// The pre-TASK-265 tolerant strip, kept as the degrade path for values that are
/// not TOML-legal: only a MATCHED pair of surrounding double-quotes comes off, so
/// unmatched and doubled quotes survive intact instead of being half-eaten.
fn strip_matched_double_quotes(trimmed: &str) -> String {
    trimmed
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(trimmed)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // trace:STORY-194 | ai:claude — Auto resolves from $EDITOR (vi-family → Vim,
    // else Emacs); the explicit choices map straight through regardless of env.
    #[test]
    fn editor_choice_resolves_to_a_model() {
        assert_eq!(EditorChoice::Auto.resolve("vim"), EditorModel::Vim);
        assert_eq!(EditorChoice::Auto.resolve("nano"), EditorModel::Emacs);
        assert_eq!(EditorChoice::Auto.resolve(""), EditorModel::Emacs);
        // Explicit choices ignore the env entirely.
        assert_eq!(EditorChoice::Vim.resolve("nano"), EditorModel::Vim);
        assert_eq!(EditorChoice::Emacs.resolve("vim"), EditorModel::Emacs);
    }

    // trace:STORY-194 | ai:claude — the editor choice parses its tokens + friendly
    // aliases and cycles Emacs → Vim → Auto → Emacs.
    #[test]
    fn editor_choice_parses_and_cycles() {
        assert_eq!(EditorChoice::parse("vim"), Some(EditorChoice::Vim));
        assert_eq!(EditorChoice::parse("VI"), Some(EditorChoice::Vim));
        assert_eq!(EditorChoice::parse("readline"), Some(EditorChoice::Emacs));
        assert_eq!(EditorChoice::parse("auto"), Some(EditorChoice::Auto));
        assert_eq!(EditorChoice::parse("nonsense"), None);
        assert_eq!(EditorChoice::Emacs.cycle(), EditorChoice::Vim);
        assert_eq!(EditorChoice::Vim.cycle(), EditorChoice::Auto);
        assert_eq!(EditorChoice::Auto.cycle(), EditorChoice::Emacs);
    }

    // trace:STORY-194 | ai:claude — cycling each panel row mutates the matching
    // field: editor cycles, mouse/score flip, mode toggles.
    #[test]
    fn cycling_a_row_mutates_the_matching_setting() {
        let mut s = Settings {
            editor: EditorChoice::Emacs,
            mouse: true,
            score: false,
            mode: SessionMode::Socratic,
        };
        s.cycle(SettingKey::Editor);
        assert_eq!(s.editor, EditorChoice::Vim);
        s.cycle(SettingKey::Mouse);
        assert!(!s.mouse);
        s.cycle(SettingKey::Score);
        assert!(s.score);
        s.cycle(SettingKey::Mode);
        assert_eq!(s.mode, SessionMode::Debate);
    }

    // trace:STORY-194 | ai:claude — the `/settings set <key> <value>` line path
    // mutates each setting and reports a bad value.
    #[test]
    fn set_from_token_mutates_or_reports() {
        let mut s = Settings::default();
        assert!(s.set_from_token(SettingKey::Editor, "vim"));
        assert_eq!(s.editor, EditorChoice::Vim);
        assert!(s.set_from_token(SettingKey::Mouse, "off"));
        assert!(!s.mouse);
        assert!(s.set_from_token(SettingKey::Score, "on"));
        assert!(s.score);
        assert!(s.set_from_token(SettingKey::Mode, "debate"));
        assert_eq!(s.mode, SessionMode::Debate);
        assert!(!s.set_from_token(SettingKey::Editor, "nonsense"));
    }

    // trace:STORY-194 | ai:claude — a saved setting ROUND-TRIPS through the config
    // schema: serialize then parse recovers every value.
    #[test]
    fn settings_round_trip_through_the_config_schema() {
        let original = Settings {
            editor: EditorChoice::Vim,
            mouse: false,
            score: true,
            mode: SessionMode::Debate,
        };
        let restored = Settings::from_toml(&original.to_toml());
        assert_eq!(restored, original);
    }

    // trace:STORY-194 | ai:claude — the schema is FORWARD-COMPATIBLE: unknown keys
    // are ignored and omitted keys keep their default, so an old / newer file loads.
    #[test]
    fn from_toml_ignores_unknown_keys_and_keeps_defaults() {
        let text = "editor = \"vim\"\n\
                    future_theme = \"solarized\"\n\
                    mouse = off\n";
        let s = Settings::from_toml(text);
        assert_eq!(s.editor, EditorChoice::Vim);
        assert!(!s.mouse);
        // Omitted keys keep the defaults.
        assert!(!s.score);
        assert_eq!(s.mode, SessionMode::default());
    }

    // trace:TASK-218 | ai:claude — the SAVE path preserves every key the settings
    // surface does not model. A hand-added `dolt_path` (plus its comment) must
    // survive a `/settings` toggle, or the next launch silently repoints at
    // `data/dolt`; the modelled keys are still rewritten with current values.
    #[test]
    fn save_preserves_keys_the_settings_surface_does_not_model() {
        let existing = "# hand-edited\n\
                        editor = \"emacs\"\n\
                        # the domain graph lives on the big disk\n\
                        dolt_path = \"/mnt/data/dolt\"\n\
                        store = \"dolt\"\n\
                        mouse = on\n";
        let toggled = Settings {
            editor: EditorChoice::Vim,
            mouse: false,
            score: true,
            mode: SessionMode::Debate,
        };

        let saved = toggled.to_toml_merged(existing);

        // The foreign keys and comments ride through untouched, in place.
        assert!(saved.contains("dolt_path = \"/mnt/data/dolt\""), "{saved}");
        assert!(saved.contains("store = \"dolt\""), "{saved}");
        assert!(saved.contains("# hand-edited"), "{saved}");
        assert!(
            saved.contains("# the domain graph lives on the big disk"),
            "{saved}"
        );
        // ...and the resolver still reads the same path back out of the file.
        assert_eq!(
            dolt_path_from(None, &saved, None),
            PathBuf::from("/mnt/data/dolt")
        );
        // The modelled keys were rewritten, not appended alongside the old ones.
        assert_eq!(Settings::from_toml(&saved), toggled);
        assert!(!saved.contains("editor = \"emacs\""), "{saved}");
        assert_eq!(saved.matches("editor =").count(), 1, "{saved}");

        // A save-after-load round trip is a fixed point: nothing decays on the
        // second write either.
        let again = Settings::from_toml(&saved).to_toml_merged(&saved);
        assert_eq!(again, saved);
    }

    // trace:TASK-218 | ai:claude — with no file yet (or an empty one) the merge
    // degrades to a plain fresh write, header and all.
    #[test]
    fn merged_save_of_an_empty_file_is_a_fresh_write() {
        let settings = Settings::default();
        assert_eq!(settings.to_toml_merged(""), settings.to_toml());
        assert_eq!(settings.to_toml_merged("\n  \n"), settings.to_toml());
        assert!(settings.to_toml().starts_with('#'));
    }

    // trace:TASK-222 | ai:claude — the two readers of this file agree: keys are
    // case-insensitive, only a MATCHED quote pair is stripped, and a repeated key
    // is last-wins. Before TASK-222 `config_value` compared keys exactly and used
    // `trim_matches('"')`, so `Dolt_Path` was invisible to it and `""x""` parsed
    // differently in each reader.
    #[test]
    fn config_value_matches_from_toml_on_case_quotes_and_repeats() {
        let text = "Editor = \"VIM\"\nDOLT_PATH = \"/tmp/graph\"\n";
        assert_eq!(
            config_value(text, "dolt_path").as_deref(),
            Some("/tmp/graph")
        );
        assert_eq!(
            config_value(text, "DOLT_PATH").as_deref(),
            Some("/tmp/graph")
        );
        // Same file, same case-insensitivity in the settings loader.
        assert_eq!(Settings::from_toml(text).editor, EditorChoice::Vim);

        // Unmatched / doubled quotes survive intact in BOTH readers.
        let odd = "editor = \"vim\ndolt_path = \"\"/tmp/x\"\"\n";
        assert_eq!(
            config_value(odd, "dolt_path").as_deref(),
            Some("\"/tmp/x\"")
        );
        assert_eq!(Settings::from_toml(odd).editor, EditorChoice::Auto);

        // A repeated key is last-wins in both.
        let repeated = "dolt_path = /first\ndolt_path = /second\nmouse = on\nmouse = off\n";
        assert_eq!(
            config_value(repeated, "dolt_path").as_deref(),
            Some("/second")
        );
        assert!(!Settings::from_toml(repeated).mouse);

        // Comments and unparseable lines are skipped, not matched.
        assert_eq!(
            config_value("# dolt_path = /nope\nno-equals-here\n", "dolt_path"),
            None
        );
    }

    // trace:TASK-228 | ai:claude — THE resolution chain: env beats the settings
    // key beats the compiled default, and a blank tier falls through instead of
    // resolving to an empty path.
    #[test]
    fn dolt_path_chain_is_env_then_settings_then_default() {
        let config = "editor = \"vim\"\ndolt_path = \"/tmp/graph\"\n";
        assert_eq!(
            dolt_path_from(None, "", None),
            PathBuf::from(DEFAULT_DOLT_DB_PATH)
        );
        assert_eq!(
            dolt_path_from(None, config, None),
            PathBuf::from("/tmp/graph")
        );
        assert_eq!(
            dolt_path_from(Some("/env/path"), config, None),
            PathBuf::from("/env/path")
        );
        // Surrounding whitespace is trimmed off the env value.
        assert_eq!(
            dolt_path_from(Some("  /env/path  "), config, None),
            PathBuf::from("/env/path")
        );
        // A blank tier is not a selection — fall through to the next one.
        assert_eq!(
            dolt_path_from(Some("   "), config, None),
            PathBuf::from("/tmp/graph")
        );
        assert_eq!(
            dolt_path_from(Some(""), "dolt_path = \"\"\n", None),
            PathBuf::from(DEFAULT_DOLT_DB_PATH)
        );
    }

    // trace:STORY-261 | ai:claude — the backup directory rides the SAME chain
    // (TASK-243), so `--to` > env > settings > platform data dir, and a blank
    // tier still falls through rather than pointing the remote at "".
    #[test]
    fn dolt_backup_path_chain_matches_the_repo_path_chain() {
        let default = PathBuf::from("/home/someone/.local/share/quizdom/dolt-backup");
        let config = "editor = \"vim\"\ndolt_backup_path = \"/mnt/usb/quizdom\"\n";
        assert_eq!(
            tiered_path(None, "", None, "dolt_backup_path", default.clone()),
            default
        );
        assert_eq!(
            tiered_path(None, config, None, "dolt_backup_path", default.clone()),
            PathBuf::from("/mnt/usb/quizdom")
        );
        assert_eq!(
            tiered_path(
                Some(" /env/backup "),
                config,
                None,
                "dolt_backup_path",
                default.clone()
            ),
            PathBuf::from("/env/backup")
        );
        assert_eq!(
            tiered_path(
                Some("  "),
                config,
                None,
                "dolt_backup_path",
                default.clone()
            ),
            PathBuf::from("/mnt/usb/quizdom")
        );
        assert_eq!(
            tiered_path(
                Some(""),
                "dolt_backup_path = \"\"\n",
                None,
                "dolt_backup_path",
                default.clone()
            ),
            default
        );
        // The backup default must not sit inside the repo it protects.
        assert!(!default_dolt_backup_path().starts_with(DEFAULT_DOLT_DB_PATH));
    }

    // trace:TASK-265 | ai:claude — the file is `.toml`, so TOML-LEGAL values parse
    // to the bare value: double-quoted, single-quoted, and either of those (or a
    // bare value) followed by an inline `# comment`. The comment case is the one
    // with teeth: before TASK-265 `dolt_path = "/mnt/data/dolt" # big disk` kept
    // the comment in the value, and TASK-228 had just wired that string into
    // `db-init`, which would CREATE a repo at that path.
    #[test]
    fn toml_legal_values_parse_to_the_bare_value() {
        for (line, expected) in [
            ("dolt_path = \"/mnt/data/dolt\"", "/mnt/data/dolt"),
            ("dolt_path = '/mnt/data/dolt'", "/mnt/data/dolt"),
            (
                "dolt_path = \"/mnt/data/dolt\"  # the big disk",
                "/mnt/data/dolt",
            ),
            (
                "dolt_path = '/mnt/data/dolt'  # the big disk",
                "/mnt/data/dolt",
            ),
            (
                "dolt_path = /mnt/data/dolt # the big disk",
                "/mnt/data/dolt",
            ),
            ("dolt_path = /mnt/data/dolt", "/mnt/data/dolt"),
            // A `#` INSIDE quotes is part of the value, not a comment.
            ("dolt_path = \"/mnt/data/dolt#2\"", "/mnt/data/dolt#2"),
            // An empty quoted string stays empty (and so falls through the chain).
            ("dolt_path = \"\"", ""),
        ] {
            assert_eq!(
                config_value(line, "dolt_path").as_deref(),
                Some(expected),
                "{line}"
            );
        }

        // The same rules in the settings loader, not just `config_value`.
        assert_eq!(
            Settings::from_toml("editor = 'vim'  # single-quoted\nmouse = off # inline\n"),
            Settings {
                editor: EditorChoice::Vim,
                mouse: false,
                ..Settings::default()
            }
        );

        // ...and the whole resolution chain, since that is what `db-init` uses.
        assert_eq!(
            dolt_path_from(
                None,
                "dolt_path = \"/mnt/data/dolt\"  # the big disk\n",
                None
            ),
            PathBuf::from("/mnt/data/dolt")
        );
    }

    // trace:TASK-263 | ai:claude — `settings.toml` is per-USER and GLOBAL, so a
    // RELATIVE `dolt_path` in it must name the SAME repo from every worktree.
    // Before TASK-263 it resolved against the process cwd, so one config line
    // selected a different graph from each sibling checkout — silently.
    #[test]
    fn a_relative_settings_path_is_anchored_to_the_settings_dir() {
        let config = "dolt_path = \"graphs/main\"\n";
        let config_dir = PathBuf::from("/home/someone/.config/quizdom");

        // The same config, resolved from two different worktrees, is ONE repo:
        // the anchor is the settings file's own directory, not the cwd.
        let anchored = dolt_path_from(None, config, Some(&config_dir));
        assert_eq!(
            anchored,
            PathBuf::from("/home/someone/.config/quizdom/graphs/main")
        );
        assert!(anchored.is_absolute());

        // An ABSOLUTE settings value is left alone.
        assert_eq!(
            dolt_path_from(None, "dolt_path = \"/mnt/data/dolt\"\n", Some(&config_dir)),
            PathBuf::from("/mnt/data/dolt")
        );

        // The env tier is NOT anchored — it is named per-invocation by someone
        // who can see their own cwd (same for the `--path` flag above it).
        assert_eq!(
            dolt_path_from(Some("local/graph"), config, Some(&config_dir)),
            PathBuf::from("local/graph")
        );

        // Nor is the compiled default: `data/dolt` is deliberately per-checkout.
        assert_eq!(
            dolt_path_from(None, "", Some(&config_dir)),
            PathBuf::from(DEFAULT_DOLT_DB_PATH)
        );

        // The backup directory rides the identical rule.
        assert_eq!(
            tiered_path(
                None,
                "dolt_backup_path = \"backups\"\n",
                Some(&config_dir),
                "dolt_backup_path",
                PathBuf::from("/fallback")
            ),
            PathBuf::from("/home/someone/.config/quizdom/backups")
        );
    }

    // trace:TASK-267 | ai:claude — the maintenance trap made loud: every entry in
    // MODELLED_KEYS must have a `rendered_line` arm. Add a fifth key without the
    // matching arm and THIS fails, instead of the key silently vanishing from
    // every settings file quizdom writes.
    #[test]
    fn every_modelled_key_renders_a_line() {
        let settings = Settings::default();
        for key in MODELLED_KEYS {
            let line = settings
                .rendered_line(key)
                .unwrap_or_else(|| panic!("MODELLED_KEYS entry `{key}` has no rendered_line arm"));
            // The rendered line must also parse back as that key, or a save/load
            // round trip would still lose it.
            assert_eq!(
                config_entry(line.trim_end())
                    .map(|(name, _)| name)
                    .as_deref(),
                Some(key),
                "{line}"
            );
        }
        // A key that is genuinely not modelled is still `None` (the arm exists
        // so the trap can be detected, not so every string renders).
        assert_eq!(settings.rendered_line("dolt_path"), None);
    }

    // trace:TASK-268 | ai:claude — a save over a present-but-UNREADABLE file must
    // refuse, never degrade to a fresh write. The fresh write would drop exactly
    // the foreign keys TASK-218 taught the merge to preserve, and the user would
    // have no idea: the toggle they typed appears to have worked.
    #[test]
    fn save_refuses_rather_than_dropping_keys_when_the_file_is_unreadable() {
        let settings = Settings {
            editor: EditorChoice::Vim,
            mouse: false,
            score: true,
            mode: SessionMode::Debate,
        };

        // Readable → merge, foreign keys preserved.
        let existing = "dolt_path = \"/mnt/data/dolt\"\neditor = \"emacs\"\n";
        let merged = merged_body(&settings, Ok(existing.to_string())).expect("merge");
        assert!(
            merged.contains("dolt_path = \"/mnt/data/dolt\""),
            "{merged}"
        );
        assert!(merged.contains("editor = \"vim\""), "{merged}");

        // Absent → a fresh write loses nothing, so it is allowed.
        let fresh = merged_body(
            &settings,
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "nope")),
        )
        .expect("fresh write on a missing file");
        assert_eq!(fresh, settings.to_toml());

        // Present but unreadable → REFUSE. The error propagates to the caller,
        // which treats persistence as best-effort; the file is left intact.
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::InvalidData,
            std::io::ErrorKind::Other,
        ] {
            let err = merged_body(&settings, Err(std::io::Error::new(kind, "boom")))
                .expect_err("an unreadable existing file must not be overwritten");
            assert_eq!(err.kind(), kind);
        }
    }

    // trace:TASK-262 | ai:claude — `dolt_path` selects WHICH domain graph the
    // session reads, and STORY-258's preserve-foreign-lines option left it with
    // no surface at all. `/settings` now shows it (read-only — the file and the
    // env var stay the way to change it).
    #[test]
    fn settings_list_shows_the_domain_graph_path() {
        let list = Settings::default().render_list_showing(&test_read_only_rows());
        assert!(list.contains(DOLT_PATH_ROW_LABEL), "{list}");
        assert!(list.contains("/home/someone/graphs/quizdom"), "{list}");
        // Read-only is stated, so nobody hunts for a toggle that is not there.
        assert!(list.contains("dolt_path"), "{list}");
        assert!(list.contains("QUIZDOM_DOLT_PATH"), "{list}");
        // The four toggles are still the rows above it.
        assert!(list.contains("Editor mode"), "{list}");
    }

    /// The read-only trio with recognisable values, for the row tests — the SAME
    /// rows `resolved()` hands back under `cfg(test)` (TASK-306), so a test that
    /// renders the live list and one that renders the fixture agree.
    fn test_read_only_rows() -> ReadOnlyRows {
        ReadOnlyRows::hermetic()
    }

    // trace:TASK-320 | ai:claude — STORY-299 added `auto_backup` and `log_path`
    // and gave neither a surface. `auto_backup` is a DURABILITY control (the
    // worst category to hide: believing it is on and believing it is off look
    // identical until the disk dies) and `log_path` is the answer to "where do I
    // look when something degraded?", which otherwise lived only in prose.
    #[test]
    fn settings_list_shows_auto_backup_and_the_diagnostic_log() {
        let list = Settings::default().render_list_showing(&test_read_only_rows());

        assert!(list.contains(AUTO_BACKUP_ROW_LABEL), "{list}");
        assert!(list.contains(LOG_PATH_ROW_LABEL), "{list}");
        assert!(list.contains("/home/someone/logs/quizdom.log"), "{list}");
        // The VALUE is shown, not just the label — On, because these rows exist
        // to answer "is it on?".
        let auto_row = list
            .lines()
            .find(|line| line.contains(AUTO_BACKUP_ROW_LABEL))
            .unwrap_or_default()
            .to_string();
        assert!(auto_row.contains("On"), "{list}");
        // Each row names the settings key and the env var that DO change it,
        // since none of the three is togglable here.
        for lever in [
            "auto_backup",
            "QUIZDOM_AUTO_BACKUP",
            "log_path",
            "QUIZDOM_LOG_PATH",
        ] {
            assert!(list.contains(lever), "list missing {lever}:\n{list}");
        }
    }

    // trace:TASK-320 | ai:claude — an OFF auto-backup must read as Off rather
    // than as an absent row: "no row" and "row saying Off" are the same pixels
    // to a user who does not know the row exists.
    #[test]
    fn an_off_auto_backup_still_gets_a_row() {
        let list = Settings::default().render_list_showing(&ReadOnlyRows {
            auto_backup: false,
            ..test_read_only_rows()
        });

        let auto_row = list
            .lines()
            .find(|line| line.contains(AUTO_BACKUP_ROW_LABEL))
            .expect("the auto-backup row is unconditional");
        assert!(auto_row.contains("Off"), "{auto_row}");
    }

    // trace:TASK-324 | ai:claude — the remote NAME resolves through the same
    // env > settings > default chain as every other quizdom value, so the
    // end-of-session probe and `db-backup`'s push cannot disagree about which
    // remote they mean.
    #[test]
    fn the_backup_remote_name_resolves_env_then_settings_then_default() {
        assert_eq!(
            backup_remote_from(Some("archive"), "backup_remote = \"from-file\"\n"),
            "archive",
            "env wins"
        );
        assert_eq!(
            backup_remote_from(None, "backup_remote = \"from-file\"\n"),
            "from-file",
            "settings is the middle tier"
        );
        assert_eq!(
            backup_remote_from(None, ""),
            crate::db_backup::BACKUP_REMOTE_NAME,
            "the compiled default is the floor"
        );
        // An exported-but-empty variable must not name the empty remote — same
        // fall-through rule as `tiered_path`.
        assert_eq!(
            backup_remote_from(Some("   "), "backup_remote = \"from-file\"\n"),
            "from-file",
        );
        assert_eq!(
            backup_remote_from(Some(""), ""),
            crate::db_backup::BACKUP_REMOTE_NAME,
        );
    }

    // trace:TASK-266 | ai:claude — a persisted `score` survives a load/save round
    // trip. It used to die on the first `/settings`: the engine seeded
    // `score_gauge_on = false` regardless of the file, then `sync_score(false)`
    // wrote that default straight back over the saved `true`. The engine now
    // seeds from `FrontEnd::persisted_score`, so the value that comes back out is
    // the value that went in.
    #[test]
    fn a_persisted_score_survives_a_load_save_round_trip() {
        let on_disk = "# hand-edited\n\
                       dolt_path = \"/mnt/data/dolt\"\n\
                       score = true\n\
                       mode = \"debate\"\n";

        // LOAD: the persisted score is honoured, not ignored.
        let loaded = Settings::from_toml(on_disk);
        assert!(loaded.score, "a persisted `score = true` must load as true");

        // SAVE (with nothing else changed): the value is still true on disk, and
        // the foreign `dolt_path` line rode through untouched.
        let saved = loaded.to_toml_merged(on_disk);
        assert!(saved.contains("score = true"), "{saved}");
        assert!(saved.contains("dolt_path = \"/mnt/data/dolt\""), "{saved}");
        assert_eq!(Settings::from_toml(&saved), loaded);

        // The clobber the engine used to perform, spelled out: writing the
        // ENGINE's hardcoded default over the file is what lost the setting.
        let clobbered = Settings {
            score: false,
            ..loaded
        }
        .to_toml_merged(on_disk);
        assert!(clobbered.contains("score = false"), "{clobbered}");
        assert!(
            !Settings::from_toml(&clobbered).score,
            "sanity: this is the regression the seed prevents"
        );
    }

    // trace:TASK-300 | ai:claude — `mode` is `score`'s twin, and it was left out of
    // the TASK-266 bundle, so it still shipped broken: the engine started every
    // session at the compiled Socratic default and the first `/settings` pushed
    // that default back across `sync_mode` and SAVED it over a `mode = "debate"`
    // the user had written. The engine now seeds from `persisted_settings`, so the
    // value that comes back out is the value that went in.
    #[test]
    fn a_persisted_mode_survives_a_load_save_round_trip() {
        let on_disk = "# hand-edited\n\
                       dolt_path = \"/mnt/data/dolt\"\n\
                       mode = \"debate\"\n";

        // LOAD: the persisted mode is honoured, not ignored.
        let loaded = Settings::from_toml(on_disk);
        assert_eq!(
            loaded.mode,
            SessionMode::Debate,
            "a persisted `mode = \"debate\"` must load as Debate"
        );

        // SAVE (with nothing else changed): still debate on disk, foreign lines
        // intact.
        let saved = loaded.to_toml_merged(on_disk);
        assert!(saved.contains("mode = \"debate\""), "{saved}");
        assert!(saved.contains("dolt_path = \"/mnt/data/dolt\""), "{saved}");
        assert_eq!(Settings::from_toml(&saved), loaded);

        // The clobber the engine used to perform, spelled out: writing the
        // ENGINE's hardcoded default over the file is what lost the setting.
        let clobbered = Settings {
            mode: SessionMode::Socratic,
            ..loaded
        }
        .to_toml_merged(on_disk);
        assert!(clobbered.contains("mode = \"socratic\""), "{clobbered}");
        assert_eq!(
            Settings::from_toml(&clobbered).mode,
            SessionMode::Socratic,
            "sanity: this is the regression the seed prevents"
        );
    }

    // trace:TASK-307 | ai:claude — `~/graphs/main` is exactly the value a user
    // writes for "in my home directory", and it used to name a LITERAL `~`
    // directory, anchored under `~/.config/quizdom/` by TASK-263 — so `db-init`
    // would create the graph somewhere nobody would ever look for it.
    #[test]
    fn a_leading_tilde_expands_to_the_home_directory() {
        let home = PathBuf::from("/home/someone");

        assert_eq!(
            expand_tilde_from("~/graphs/main", Some(&home)),
            PathBuf::from("/home/someone/graphs/main")
        );
        // A bare `~` is the home directory itself.
        assert_eq!(expand_tilde_from("~", Some(&home)), home);
        // Only a LEADING `~` expands, and only as a whole path component: a `~`
        // mid-path is an ordinary character, and `~alice` needs the password
        // database we do not have, so it stays recognisable rather than becoming
        // a half-translated path.
        for untouched in ["~alice/graphs", "/mnt/~/graphs", "graphs/~/main"] {
            assert_eq!(
                expand_tilde_from(untouched, Some(&home)),
                PathBuf::from(untouched),
                "{untouched}"
            );
        }
        // No `$HOME` — nothing to expand to, so the value is left alone rather
        // than resolving under `/`.
        assert_eq!(
            expand_tilde_from("~/graphs/main", None),
            PathBuf::from("~/graphs/main")
        );

        // The whole chain, since that is what `db-init` and the runtime store use
        // — and the expansion makes it ABSOLUTE, so TASK-263's anchoring correctly
        // leaves it alone instead of burying it under the config directory.
        let config_dir = PathBuf::from("/home/someone/.config/quizdom");
        let resolved = tiered_path(
            None,
            "dolt_path = \"~/graphs/main\"\n",
            Some(&config_dir),
            "dolt_path",
            PathBuf::from("data/dolt"),
        );
        let expected = match home_dir() {
            Some(home) => home.join("graphs/main"),
            // No `$HOME` at all (a stripped environment): nothing to expand to,
            // so the value stays literal and TASK-263 anchors it like any other
            // relative path. Stated rather than skipped, so the degrade is
            // covered too.
            None => config_dir.join("~/graphs/main"),
        };
        assert_eq!(resolved, expected);
        assert!(resolved.ends_with("graphs/main"), "{resolved:?}");
    }

    // trace:TASK-307 | ai:claude — a double-quoted value is a TOML BASIC string:
    // its escapes are processed and the closing quote is the first UNESCAPED one.
    // A single-quoted value is a LITERAL string and is taken verbatim.
    #[test]
    fn basic_strings_honour_toml_escapes_and_literal_strings_do_not() {
        for (line, expected) in [
            // The escapes that have a meaning.
            ("dolt_path = \"/mnt/a\\tb\"", "/mnt/a\tb"),
            ("dolt_path = \"/mnt/a\\nb\"", "/mnt/a\nb"),
            ("dolt_path = \"C:\\\\graphs\\\\main\"", "C:\\graphs\\main"),
            ("dolt_path = \"\\u0041/graphs\"", "A/graphs"),
            ("dolt_path = \"\\U0001F600\"", "\u{1F600}"),
            // An ESCAPED quote does not end the value...
            (
                "dolt_path = \"/mnt/say \\\"yes\\\"/dolt\"",
                "/mnt/say \"yes\"/dolt",
            ),
            // ...and a trailing comment after the real closing quote still goes.
            ("dolt_path = \"/mnt/a\\tb\"  # tabbed", "/mnt/a\tb"),
            // A LITERAL string escapes nothing — the spelling for a Windows path.
            ("dolt_path = 'C:\\graphs\\main'", "C:\\graphs\\main"),
            // An UNKNOWN escape is kept verbatim rather than dropped: `\U` here
            // wants 8 hex digits and has none, and a path that comes back
            // unchanged beats one silently missing characters.
            ("dolt_path = \"C:\\Users\\me\"", "C:\\Users\\me"),
            ("dolt_path = \"a\\qb\"", "a\\qb"),
            // A malformed code point degrades the same way.
            ("dolt_path = \"\\u00ZZ\"", "\\u00ZZ"),
        ] {
            assert_eq!(
                config_value(line, "dolt_path").as_deref(),
                Some(expected),
                "{line}"
            );
        }
    }

    // trace:TASK-306 | ai:claude — no test reads the REAL user config. `resolved()`
    // is the last reader that did, and `render_list` calls it, so the developer's
    // own `~/.config/quizdom/settings.toml` decided what the `/settings` tests saw
    // — passing on one machine and failing on another. Under `cfg(test)` it now
    // hands back fixed rows (the TASK-266 pattern), which is what makes
    // `render_list` safe to call in a test at all.
    #[test]
    fn the_read_only_rows_are_hermetic_under_test() {
        assert_eq!(
            ReadOnlyRows::resolved().rows(),
            ReadOnlyRows::hermetic().rows(),
            "a test must never see the developer's real settings file"
        );
        // And the live entry point renders exactly what the fixture does, so the
        // tests below can use either and mean the same thing.
        assert_eq!(
            Settings::default().render_list(),
            Settings::default().render_list_showing(&test_read_only_rows())
        );
    }

    // trace:STORY-194 | ai:claude — the printed list (headless panel degrade) shows
    // every setting's current value label.
    #[test]
    fn render_list_shows_every_setting() {
        let s = Settings {
            editor: EditorChoice::Vim,
            mouse: false,
            score: true,
            mode: SessionMode::Debate,
        };
        let list = s.render_list_showing(&test_read_only_rows());
        assert!(list.contains("Editor mode"));
        assert!(list.contains("Vim"));
        assert!(list.contains("Mouse"));
        assert!(list.contains("Off"));
        assert!(list.contains("Score gauge"));
        assert!(list.contains("On"));
        assert!(list.contains("Session mode"));
        assert!(list.contains("Debate"));
    }

    // trace:STORY-194 | ai:claude — the config path follows XDG_CONFIG_HOME first,
    // then $HOME/.config, and ends in quizdom/settings.toml.
    #[test]
    fn config_path_follows_xdg_then_home() {
        // We avoid mutating the process env (other tests read it); instead assert
        // the suffix invariant the resolver guarantees whenever a base is found.
        if let Some(path) = config_path() {
            assert!(path.ends_with("quizdom/settings.toml"));
        }
    }

    // trace:STORY-194 | ai:claude — load_or_seed SEEDS the editor from $EDITOR on a
    // first run (no file): a fresh temp XDG dir yields the env-inferred choice, and
    // saving then loading round-trips an explicit choice (saved value wins).
    #[test]
    fn load_seeds_first_run_then_saved_value_wins() {
        // A private temp config dir so this test never touches the real one.
        let dir = std::env::temp_dir().join(format!(
            "quizdom-settings-test-{}-{}",
            std::process::id(),
            line!()
        ));
        let path = dir.join("quizdom").join("settings.toml");
        let _ = std::fs::remove_dir_all(&dir);

        // First run: no file yet → seed_from_env path (we exercise it directly so
        // the test is independent of the ambient $EDITOR).
        let seeded = seed_from_env();
        // The seed is always a CONCRETE choice (never Auto) — it mirrors STORY-180.
        assert!(matches!(
            seeded.editor,
            EditorChoice::Vim | EditorChoice::Emacs
        ));

        // Save an explicit choice, then load it back: the saved value wins.
        // trace:TASK-373 | ai:claude — through `save_at` / `load_or_seed_at`
        // themselves, not a hand-rolled write-then-parse standing in for them.
        // The IO is only compiled out when there is no path, so a test that
        // supplies one is exercising the real save, parent-directory creation
        // and merge included.
        let saved = Settings {
            editor: EditorChoice::Auto,
            mouse: false,
            score: true,
            mode: SessionMode::Debate,
        };
        assert!(
            !path.exists(),
            "and the parent directory does not exist yet"
        );
        save_at(Some(&path), &saved).expect("the save creates its own parent");
        assert_eq!(load_or_seed_at(Some(&path)), saved);

        // No path at all: nothing to load from, nothing to write to, no failure.
        assert_eq!(load_or_seed_at(None), Settings::default());
        save_at(None, &saved).expect("nowhere to persist is not an error");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:STORY-299 | ai:claude — the decision STORY-299 recorded is that
    // backups stay EXPLICIT, so the absence of the key has to mean off.
    #[test]
    fn auto_backup_is_off_unless_someone_asked_for_it() {
        assert!(!auto_backup_from(None, ""), "an absent key is off");
        assert!(
            !auto_backup_from(None, "dolt_path = \"/mnt/data/dolt\"\n"),
            "a settings file that says nothing about backups is off"
        );
        assert!(!auto_backup_from(None, "auto_backup = false\n"));
        assert!(auto_backup_from(None, "auto_backup = true\n"));
        // The same permissive on/off vocabulary the other boolean settings take.
        assert!(auto_backup_from(None, "auto_backup = \"on\"\n"));
        assert!(
            auto_backup_from(None, "AUTO_BACKUP = 1\n"),
            "keys fold case"
        );
        // Inline comments are TOML-legal here like everywhere else (TASK-265).
        assert!(auto_backup_from(
            None,
            "auto_backup = true  # nightly disk\n"
        ));
    }

    // trace:STORY-299 | ai:claude
    #[test]
    fn the_environment_overrides_the_file_and_a_typo_falls_through() {
        assert!(auto_backup_from(Some("1"), "auto_backup = false\n"));
        assert!(!auto_backup_from(Some("off"), "auto_backup = true\n"));
        // An exported-but-empty variable is not an answer — same shape as
        // `tiered_path`'s blank-falls-through rule.
        assert!(auto_backup_from(Some(""), "auto_backup = true\n"));
        // Nor is a typo: it must not silently DISABLE a backup the file asked
        // for, because the failure would be invisible until a disk died.
        assert!(auto_backup_from(Some("ture"), "auto_backup = true\n"));
        assert!(
            !auto_backup_from(Some("ture"), "auto_backup = nope\n"),
            "with no parseable answer at any tier, the default stands"
        );
    }

    // trace:STORY-299 | ai:claude — the log lives beside the backup, in the user
    // data dir, never in the project tree.
    #[test]
    fn the_default_log_path_sits_next_to_the_default_backup() {
        let default = default_log_path();
        assert_eq!(default.file_name().unwrap(), "quizdom.log");
        assert_eq!(
            default.parent(),
            default_dolt_backup_path().parent(),
            "both defaults hang off the one user data directory"
        );
    }

    // trace:STORY-299 | ai:claude — the log path takes the same chain (and so
    // the same TASK-263 anchoring) as the two Dolt paths.
    #[test]
    fn a_relative_log_path_anchors_to_the_settings_file() {
        let config_dir = Path::new("/home/someone/.config/quizdom");

        assert_eq!(
            tiered_path(
                None,
                "log_path = \"logs/quizdom.log\"\n",
                Some(config_dir),
                "log_path",
                PathBuf::from("unused"),
            ),
            PathBuf::from("/home/someone/.config/quizdom/logs/quizdom.log"),
        );
        // The env tier stays cwd-relative, exactly as it does for dolt_path.
        assert_eq!(
            tiered_path(
                Some("logs/quizdom.log"),
                "log_path = \"/absolute/wins/not.log\"\n",
                Some(config_dir),
                "log_path",
                PathBuf::from("unused"),
            ),
            PathBuf::from("logs/quizdom.log"),
        );
    }
}
