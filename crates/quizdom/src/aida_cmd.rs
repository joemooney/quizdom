// trace:BUG-200 | ai:claude
//! The single choke point for spawning the `aida` CLI.
//!
//! Upstream aida auto-switches to compact TOON output when stdout is not a
//! TTY, which breaks every screen-scraping parser in this crate — they expect
//! the human `ID:` / `Title:` / `Tags:` layout. Every `aida` spawn must be
//! built through [`aida_command`], which pins `AIDA_OUTPUT_FORMAT=human` on
//! the child regardless of the parent environment. A guard test below fails
//! if a raw `Command::new` reappears outside the allowlisted files.

use std::process::Command;

/// The output format every quizdom → `aida` shell-out pins, so the parsers
/// see a stable layout even when the child's stdout is a pipe.
pub(crate) const PINNED_AIDA_FORMAT: &str = "human";

/// Build a [`Command`] for an `aida` invocation with the output format pinned.
pub(crate) fn aida_command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.env("AIDA_OUTPUT_FORMAT", PINNED_AIDA_FORMAT);
    command
}

// trace:BUG-200 | ai:claude
#[cfg(test)]
mod tests {
    use super::{aida_command, PINNED_AIDA_FORMAT};
    use std::ffi::OsStr;

    #[test]
    fn aida_command_pins_the_output_format() {
        let command = aida_command("aida");
        let pinned = command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("AIDA_OUTPUT_FORMAT"));
        assert_eq!(
            pinned,
            Some((
                OsStr::new("AIDA_OUTPUT_FORMAT"),
                Some(OsStr::new(PINNED_AIDA_FORMAT))
            ))
        );
    }

    #[test]
    fn no_raw_command_spawns_outside_the_choke_point() {
        // `editor.rs` spawns `$EDITOR`, not `aida`, so it may build its own
        // Command; everything else must go through `aida_command`.
        const ALLOWED: &[&str] = &["aida_cmd.rs", "editor.rs"];
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&src_dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            let name = path
                .file_name()
                .expect("file name")
                .to_string_lossy()
                .to_string();
            if ALLOWED.contains(&name.as_str()) {
                continue;
            }
            if std::fs::read_to_string(&path)
                .expect("read source file")
                .contains("Command::new")
            {
                offenders.push(name);
            }
        }
        assert!(
            offenders.is_empty(),
            "raw Command::new outside the aida_cmd choke point — route aida \
             spawns through aida_command() so the output format stays pinned \
             (BUG-200): {offenders:?}"
        );
    }
}
