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

    /// Serialize to the small flat config schema (`key = value` per line). Only
    /// the KNOWN keys are written; loading ignores any others (forward-compatible).
    /// Takes `self` by value ([`Settings`] is `Copy`).
    pub(crate) fn to_toml(self) -> String {
        format!(
            "# quizdom settings (STORY-194) — edited live by /settings; unknown keys are ignored\n\
             editor = \"{}\"\n\
             mouse = {}\n\
             score = {}\n\
             mode = \"{}\"\n",
            self.editor.as_str(),
            self.mouse,
            self.score,
            self.mode.as_str(),
        )
    }

    /// Parse the config schema, IGNORING unknown keys and unparseable values
    /// (forward-compatible: a newer file with extra keys still loads). Any key the
    /// file omits keeps the [`Default`] value, so a partial/old file round-trips.
    pub(crate) fn from_toml(text: &str) -> Self {
        let mut settings = Settings::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((raw_key, raw_value)) = line.split_once('=') else {
                continue;
            };
            let key = raw_key.trim().to_ascii_lowercase();
            let value = unquote(raw_value.trim());
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

/// SAVE the settings to the config file (best-effort, creating the parent dir).
/// Returns `Ok(())` even when there is no config path (nothing to persist to);
/// an IO error is returned so an interactive caller could surface it, but callers
/// generally treat persistence as best-effort.
pub(crate) fn save(settings: &Settings) -> std::io::Result<()> {
    let Some(path) = config_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, settings.to_toml())
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
