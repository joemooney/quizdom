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
//! selects the Dolt domain-graph repo and `dolt_backup_path` its file-remote
//! backup directory (STORY-261). Two consequences, both handled here so no
//! second reader/writer of the file can drift from this one:
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

use crate::db_init::DEFAULT_DOLT_DB_PATH;
use crate::editor::{editor_model_from_editor, EditorModel};
use crate::strategy::SessionMode;
use std::env;
use std::path::PathBuf;

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

/// The config keys [`Settings`] models, in write order. Every OTHER key in the
/// file is foreign — read by someone else (`dolt_path`) or written by a future
/// version — and [`Settings::to_toml_merged`] preserves it untouched.
const MODELLED_KEYS: [&str; 4] = ["editor", "mouse", "score", "mode"];

/// The comment line at the top of a freshly written config file.
const CONFIG_HEADER: &str =
    "# quizdom settings (STORY-194) — edited live by /settings; other keys are preserved\n";

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

    /// Render the panel as a printed list of `label: value` rows (the HEADLESS
    /// degradation of the TUI panel, and the body of the `/settings` line echo).
    pub(crate) fn render_list(&self) -> String {
        let mut out = String::from("Settings\n");
        for key in SettingKey::order() {
            out.push_str(&format!(
                "  {:<14}{}\n",
                format!("{}:", key.label()),
                self.value_label(key)
            ));
        }
        out.push_str(
            "  (toggle with /editor, /mouse, /score, /mode — or /settings set <key> <value>)\n",
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
            out.push_str(&self.rendered_line(key));
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
                        out.push_str(&self.rendered_line(&key));
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
                out.push_str(&self.rendered_line(key));
            }
        }
        out
    }

    /// The `key = value` line for one modelled key — the single renderer behind
    /// both [`Settings::to_toml`] and [`Settings::to_toml_merged`], so a fresh
    /// write and an in-place rewrite can never format a value differently.
    fn rendered_line(self, key: &str) -> String {
        match key {
            "editor" => format!("editor = \"{}\"\n", self.editor.as_str()),
            "mouse" => format!("mouse = {}\n", self.mouse),
            "score" => format!("score = {}\n", self.score),
            "mode" => format!("mode = \"{}\"\n", self.mode.as_str()),
            // Unreachable for MODELLED_KEYS; a nothing-line is the safe degrade.
            _ => String::new(),
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
/// `(key, value)` — key lowercased, value unquoted. Comments, blank lines and
/// lines without an `=` yield `None`.
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
    Some((
        raw_key.trim().to_ascii_lowercase(),
        unquote(raw_value.trim()),
    ))
}

// trace:TASK-222 | ai:claude
/// Read one key out of the flat config schema, matching [`Settings::from_toml`]
/// exactly: case-insensitive keys, matched-pair unquoting, and LAST occurrence
/// wins on a repeated key. Returns `None` when the key is absent.
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
    )
}

/// The pure tier-selection behind [`resolve_dolt_path`], split from the env and
/// file reads so it is testable without touching the process environment (the
/// same pattern as [`EditorChoice::resolve`]).
fn dolt_path_from(env_path: Option<&str>, config: &str) -> PathBuf {
    tiered_path(
        env_path,
        config,
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
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| {
            env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("share"))
        })
        .map(|base| base.join("quizdom").join("dolt-backup"))
        .unwrap_or_else(|| PathBuf::from("data/dolt-backup"))
}

/// The shared env > settings > default selection both Dolt paths use. Blank
/// values fall through to the next tier so an exported-but-empty variable
/// cannot silently repoint the app at `""`.
fn tiered_path(env_path: Option<&str>, config: &str, key: &str, default: PathBuf) -> PathBuf {
    fn non_blank(value: &str) -> Option<PathBuf> {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
    }
    env_path
        .and_then(non_blank)
        .or_else(|| config_value(config, key).as_deref().and_then(non_blank))
        .unwrap_or(default)
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

/// LOAD the persisted settings, or SEED a first run from `$EDITOR`/`$VISUAL`.
///
/// * If the config file EXISTS, parse it (saved value wins, unknown keys ignored).
/// * If it does NOT exist (first run), seed the editor choice from
///   `$VISUAL`/`$EDITOR`: a vi-family editor seeds [`EditorChoice::Vim`],
///   everything else [`EditorChoice::Emacs`] — so the first-run default matches
///   the old STORY-180 startup inference. Thereafter the saved value wins.
///
/// Never fails: an unreadable / missing path degrades to a seeded default.
pub(crate) fn load_or_seed() -> Settings {
    match config_path().filter(|p| p.exists()) {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(text) => Settings::from_toml(&text),
            Err(_) => seed_from_env(),
        },
        None => seed_from_env(),
    }
}

// trace:TASK-218 | ai:claude
/// SAVE the settings to the config file (best-effort, creating the parent dir).
/// Returns `Ok(())` even when there is no config path (nothing to persist to);
/// an IO error is returned so an interactive caller could surface it, but callers
/// generally treat persistence as best-effort.
///
/// The write MERGES over whatever is already on disk
/// ([`Settings::to_toml_merged`]) rather than round-tripping the file through
/// the four modelled keys — otherwise the first `/settings` toggle of a session
/// would silently delete a hand-added `dolt_path` line and repoint the app at
/// `data/dolt`.
pub(crate) fn save(settings: &Settings) -> std::io::Result<()> {
    let Some(path) = config_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    std::fs::write(path, settings.to_toml_merged(&existing))
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

/// Strip surrounding double-quotes from a config value (the string settings are
/// quoted; the booleans are not). Tolerant — an unquoted value passes through.
fn unquote(value: &str) -> String {
    let trimmed = value.trim();
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
            dolt_path_from(None, &saved),
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
            dolt_path_from(None, ""),
            PathBuf::from(DEFAULT_DOLT_DB_PATH)
        );
        assert_eq!(dolt_path_from(None, config), PathBuf::from("/tmp/graph"));
        assert_eq!(
            dolt_path_from(Some("/env/path"), config),
            PathBuf::from("/env/path")
        );
        // Surrounding whitespace is trimmed off the env value.
        assert_eq!(
            dolt_path_from(Some("  /env/path  "), config),
            PathBuf::from("/env/path")
        );
        // A blank tier is not a selection — fall through to the next one.
        assert_eq!(
            dolt_path_from(Some("   "), config),
            PathBuf::from("/tmp/graph")
        );
        assert_eq!(
            dolt_path_from(Some(""), "dolt_path = \"\"\n"),
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
            tiered_path(None, "", "dolt_backup_path", default.clone()),
            default
        );
        assert_eq!(
            tiered_path(None, config, "dolt_backup_path", default.clone()),
            PathBuf::from("/mnt/usb/quizdom")
        );
        assert_eq!(
            tiered_path(
                Some(" /env/backup "),
                config,
                "dolt_backup_path",
                default.clone()
            ),
            PathBuf::from("/env/backup")
        );
        assert_eq!(
            tiered_path(Some("  "), config, "dolt_backup_path", default.clone()),
            PathBuf::from("/mnt/usb/quizdom")
        );
        assert_eq!(
            tiered_path(
                Some(""),
                "dolt_backup_path = \"\"\n",
                "dolt_backup_path",
                default.clone()
            ),
            default
        );
        // The backup default must not sit inside the repo it protects.
        assert!(!default_dolt_backup_path().starts_with(DEFAULT_DOLT_DB_PATH));
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
        let list = s.render_list();
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
        let saved = Settings {
            editor: EditorChoice::Auto,
            mouse: false,
            score: true,
            mode: SessionMode::Debate,
        };
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, saved.to_toml()).unwrap();
        let loaded = Settings::from_toml(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(loaded, saved);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
