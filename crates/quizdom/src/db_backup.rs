// trace:STORY-261 | ai:claude — TASK-243's durability path.
//! `quizdom db-backup` / `quizdom db-restore` — the durability path for the
//! Dolt domain graph (TASK-243, under STORY-261).
//!
//! ## Why this exists
//!
//! After the EPIC-202 cutover and STORY-209's purge of the store-side domain
//! objects, the domain graph lives ONLY in the local, gitignored Dolt repo
//! (`data/dolt`). A lost working copy loses every node and edge; replaying
//! `db-migrate` from AIDA-store history recovers the pre-cutover snapshot and
//! nothing written since, and that gap widens with every session.
//!
//! ## The shape chosen (and why)
//!
//! A **file-based Dolt remote**, not DoltHub and not `dolt backup`:
//!
//! * A file remote needs no hosted account, no credentials, and no network —
//!   the constraint TASK-243 named ("prefer a file-based remote if it avoids a
//!   hosted-account dependency"). Point `QUIZDOM_DOLT_BACKUP_PATH` at a
//!   removable disk / synced folder and the same command covers off-machine.
//! * `dolt push` / `dolt clone` are the two halves of one round trip, so the
//!   recovery path is exercisable — [`real_dolt_backup_restore_round_trip`]
//!   deletes the repo and restores it, which is precisely the STORY-261
//!   acceptance criterion, run in CI (TASK-219).
//! * The remote carries Dolt's own history, so a backup is a point-in-time
//!   snapshot chain, not a single overwritten dump.
//!
//! Backups are **explicit**, not automatic on every session write: a push is
//! seconds of dolt spawns, and silently spending that on every write would tax
//! the interactive loop for a guarantee the user hasn't asked for. Run
//! `quizdom db-backup` after a working session, or from cron / a systemd timer.
//!
//! A push carries COMMITTED data only, so `db-backup` commits the working set
//! first ([`snapshot_working_set`]). The store commits every write it makes
//! (STORY-208), but `db-init`'s schema DDL and `db-migrate`'s bulk import do
//! not — on a freshly migrated repo the whole graph sits untracked in the
//! working set, and pushing it without a snapshot would upload an empty history
//! and call that a backup.
//!
//! ## A backup directory belongs to exactly one lineage (BUG-277)
//!
//! A file remote is a directory, and a directory has no idea which repo is
//! entitled to it. Push two repos with unrelated roots at the same directory
//! and the second is refused — dolt has no ancestor to reconcile against. That
//! is the correct engine behaviour and a terrible user experience, so two
//! guards live here:
//!
//! * [`guard_test_paths`] — a `#[cfg(test)]` tripwire. No test may aim
//!   `db-backup` / `db-restore` at anything outside the system temp directory.
//! * [`unrelated_history_message`] — when dolt *does* refuse, say why in
//!   quizdom's vocabulary and name the ways out, instead of forwarding
//!   `unknown push error; no common ancestor`.
//!
//! Both exist because of one real incident. During the STORY-261 drive an
//! acceptance run executed `db-backup` with no `--to`, so a throwaway
//! two-commit fixture claimed the DEFAULT backup path
//! (`~/.local/share/quizdom/dolt-backup`). Every subsequent backup of the real
//! 195-commit graph then failed on that opaque string, and the round-trip test
//! stayed green throughout — it pins its own fixture and so never observes a
//! pre-existing foreign remote. **Verification runs are the hazard**: when
//! exercising these commands by hand, always pass `--to` / `--from` (or export
//! `QUIZDOM_DOLT_BACKUP_PATH`) into a scratch directory.

use crate::db_init::{DoltRunner, SystemDoltRunner};
use crate::error::{QuizdomError, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Output;

/// The remote name `db-backup` manages inside the domain-graph repo. Named
/// (not `origin`) so it cannot collide with the `origin` that `dolt clone`
/// writes during a restore — after a restore both names point at the same
/// backup directory, and re-running `db-backup` stays a no-op reconfigure.
pub const BACKUP_REMOTE_NAME: &str = "backup";

/// The branch pushed / restored. The domain graph only ever lives on Dolt's
/// default branch; `db-init` never creates another.
const BACKUP_BRANCH: &str = "main";

struct DbBackupConfig {
    /// The domain-graph repo to push FROM (`db-restore`: to restore INTO).
    path: PathBuf,
    /// The backup directory serving as the file remote.
    backup: PathBuf,
    remote: String,
    dolt_command: String,
}

impl DbBackupConfig {
    /// Parse the argv tail over the defaults the caller resolved through the
    /// settings chain (env > settings > default), so `--path` / `--to` stay the
    /// top-priority overrides while an unflagged run targets the SAME repo and
    /// backup directory the rest of the app resolves. Taking both defaults as
    /// parameters keeps this pure — the tests pin argument handling without the
    /// ambient environment leaking in (the TASK-228 pattern).
    fn parse(
        args: impl IntoIterator<Item = String>,
        subcommand: &str,
        default_path: PathBuf,
        default_backup: PathBuf,
    ) -> Result<Self> {
        let mut path = default_path;
        let mut backup = default_backup;
        let mut remote = BACKUP_REMOTE_NAME.to_string();
        let mut dolt_command = "dolt".to_string();
        let mut args = args.into_iter().peekable();

        if args.peek().map(String::as_str) == Some(subcommand) {
            args.next();
        }

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--path" => path = PathBuf::from(next_arg(&mut args, "--path")?),
                "--to" | "--from" => backup = PathBuf::from(next_arg(&mut args, &arg)?),
                "--remote" => remote = next_arg(&mut args, "--remote")?,
                "--dolt" => dolt_command = next_arg(&mut args, "--dolt")?,
                "--help" | "-h" => return Err(QuizdomError::Usage(usage(subcommand))),
                other => {
                    return Err(QuizdomError::Usage(format!(
                        "unknown argument: {other}\n{}",
                        usage(subcommand)
                    )))
                }
            }
        }

        Ok(Self {
            path,
            backup,
            remote,
            dolt_command,
        })
    }
}

fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| QuizdomError::Usage(format!("{name} requires a value")))
}

fn usage(subcommand: &str) -> String {
    let direction = if subcommand == "db-restore" {
        "--from <backup-dir>"
    } else {
        "--to <backup-dir>"
    };
    format!(
        "usage: quizdom {subcommand} [--path <dir>] [{direction}] \
         [--remote {BACKUP_REMOTE_NAME}] [--dolt dolt]\n\
         (--path defaults to $QUIZDOM_DOLT_PATH, else dolt_path in settings.toml;\n\
         the backup directory defaults to $QUIZDOM_DOLT_BACKUP_PATH, else \
         dolt_backup_path in settings.toml, else the platform data dir)"
    )
}

/// Entry point for `quizdom db-backup`.
pub fn run_db_backup(
    args: impl IntoIterator<Item = String>,
    output: &mut impl Write,
) -> Result<()> {
    let config = DbBackupConfig::parse(
        args,
        "db-backup",
        crate::settings::resolve_dolt_path(),
        crate::settings::resolve_dolt_backup_path(),
    )?;
    let runner = SystemDoltRunner::new(config.dolt_command.clone());
    db_backup(&config, &runner, output)
}

/// Entry point for `quizdom db-restore`.
pub fn run_db_restore(
    args: impl IntoIterator<Item = String>,
    output: &mut impl Write,
) -> Result<()> {
    let config = DbBackupConfig::parse(
        args,
        "db-restore",
        crate::settings::resolve_dolt_path(),
        crate::settings::resolve_dolt_backup_path(),
    )?;
    let runner = SystemDoltRunner::new(config.dolt_command.clone());
    db_restore(&config, &runner, output)
}

/// Push the domain graph to its file remote: ensure the backup directory
/// exists, point the `backup` remote at it (adding or re-pointing as needed),
/// then push `main`.
fn db_backup(
    config: &DbBackupConfig,
    runner: &dyn DoltRunner,
    output: &mut impl Write,
) -> Result<()> {
    guard_test_paths(config);
    if !config.path.join(".dolt").exists() {
        return Err(QuizdomError::Dolt(format!(
            "no Dolt repo at {} — run `quizdom db-init` first",
            config.path.display()
        )));
    }

    std::fs::create_dir_all(&config.backup)?;
    let url = file_remote_url(&config.backup)?;

    match existing_remote_url(runner, &config.path, &config.remote)? {
        Some(current) if current == url => {
            writeln!(
                output,
                "Remote `{}` already points at {url}.",
                config.remote
            )?;
        }
        Some(current) => {
            // Re-point rather than fail: a moved backup directory is a config
            // change, not an error, and `dolt remote` has no set-url.
            run_dolt(runner, &config.path, &["remote", "remove", &config.remote])?;
            run_dolt(
                runner,
                &config.path,
                &["remote", "add", &config.remote, &url],
            )?;
            writeln!(
                output,
                "Re-pointed remote `{}` from {current} to {url}.",
                config.remote
            )?;
        }
        None => {
            run_dolt(
                runner,
                &config.path,
                &["remote", "add", &config.remote, &url],
            )?;
            writeln!(output, "Added remote `{}` -> {url}.", config.remote)?;
        }
    }

    if snapshot_working_set(runner, &config.path)? {
        writeln!(
            output,
            "Committed pending working-set changes as a backup snapshot."
        )?;
    }

    push_to_backup(runner, config)?;
    writeln!(
        output,
        "Pushed {} ({BACKUP_BRANCH}) to {url}.\n\
         Restore with: quizdom db-restore --path {} --from {}",
        config.path.display(),
        config.path.display(),
        config.backup.display()
    )?;
    Ok(())
}

/// Clone the backup back into the domain-graph path. Refuses to touch an
/// existing repo — recovery must never be the command that destroys the copy
/// you still had.
fn db_restore(
    config: &DbBackupConfig,
    runner: &dyn DoltRunner,
    output: &mut impl Write,
) -> Result<()> {
    guard_test_paths(config);
    if config.path.join(".dolt").exists() {
        return Err(QuizdomError::Dolt(format!(
            "{} is already a Dolt repo — restore refuses to overwrite it; \
             move it aside first",
            config.path.display()
        )));
    }
    if config.path.exists() && std::fs::read_dir(&config.path)?.next().is_some() {
        return Err(QuizdomError::Dolt(format!(
            "{} exists and is not empty — restore refuses to overwrite it",
            config.path.display()
        )));
    }
    if !config.backup.exists() {
        return Err(QuizdomError::Dolt(format!(
            "no backup at {} — nothing to restore from",
            config.backup.display()
        )));
    }

    let url = file_remote_url(&config.backup)?;
    // `dolt clone` creates the target itself and errors on an existing
    // directory, so hand it an absolute target.
    let target = absolute(&config.path);
    let parent = target
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&parent)?;
    let _ = std::fs::remove_dir(&target); // empty-dir case: clone wants it gone

    // Run the clone from an EMPTY scratch directory, not from the target's
    // parent: dolt enumerates its working directory as a set of databases
    // before doing anything, so a parent holding unrelated subdirectories can
    // fail the clone outright ("failed to load database names") — and does,
    // whenever a sibling directory disappears mid-scan.
    let scratch = parent.join(format!(".quizdom-restore-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)?;
    let cloned = run_dolt(
        runner,
        &scratch,
        &["clone", &url, &target.display().to_string()],
    );
    let _ = std::fs::remove_dir(&scratch); // empty by construction
    cloned?;
    writeln!(
        output,
        "Restored {} from {url}.\n\
         Verify with: cd {} && dolt sql -q 'SELECT COUNT(*) FROM nodes'",
        config.path.display(),
        config.path.display()
    )?;
    Ok(())
}

/// The commit message for the snapshot `db-backup` takes before pushing.
const SNAPSHOT_MESSAGE: &str = "quizdom db-backup: snapshot working set";

/// Commit whatever sits in the working set, returning whether anything was
/// actually committed.
///
/// A push carries COMMITTED data only, and not every writer commits: the store
/// commits each write (STORY-208), but `db-init`'s schema DDL and `db-migrate`'s
/// bulk import land in the working set untracked. Pushing such a repo would
/// upload an empty history and report success — a backup that silently contains
/// nothing is worse than no backup at all, so `db-backup` snapshots first.
///
/// `dolt commit` exits non-zero with "no changes added to commit" on a clean
/// tree; that is the ordinary case (nothing new since the last backup), not a
/// failure.
fn snapshot_working_set(runner: &dyn DoltRunner, repo: &Path) -> Result<bool> {
    run_dolt(runner, repo, &["add", "-A"])?;
    let args = ["commit", "-m", SNAPSHOT_MESSAGE].map(String::from);
    let output = runner.run(repo, &args)?;
    if output.status.success() {
        return Ok(true);
    }
    let reported = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if reported.contains("no changes added to commit") || reported.contains("nothing to commit") {
        return Ok(false);
    }
    Err(QuizdomError::Dolt(format!(
        "dolt commit failed: {}",
        clean_dolt_message(&reported)
    )))
}

// trace:BUG-277 | ai:claude
/// What dolt prints when the remote's history shares no commit with the repo
/// being pushed. `no common ancestor` is verbatim what the engine emitted in
/// BUG-277 (`unknown push error; no common ancestor`, dolt 2.2.1, reproduced
/// against two `dolt init` repos sharing one file remote); the other two are
/// the neighbouring spellings, matched so a reworded dolt release degrades to
/// a near-miss rather than silently dropping users back to the raw string.
const UNRELATED_HISTORY_MARKERS: [&str; 3] = [
    "no common ancestor",
    "unrelated histories",
    "refusing to merge unrelated",
];

fn is_unrelated_history(reported: &str) -> bool {
    let lowered = reported.to_ascii_lowercase();
    UNRELATED_HISTORY_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

// trace:BUG-277 | ai:claude
/// Push `main` to the backup remote, translating dolt's unrelated-history
/// refusal into [`unrelated_history_message`]. Every other push failure keeps
/// [`run_dolt`]'s plain shape — this is a targeted translation, not a blanket
/// rewrite of the engine's diagnostics.
fn push_to_backup(runner: &dyn DoltRunner, config: &DbBackupConfig) -> Result<()> {
    let args = ["push", config.remote.as_str(), BACKUP_BRANCH].map(String::from);
    let output = runner.run(&config.path, &args)?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    // Dolt puts the diagnosis on stderr and an `- Uploading...` spinner on
    // stdout. Scan BOTH for the marker (a future release could move it), but
    // report only the stream carrying the message, so the spinner never lands
    // in a user-facing error.
    let reported = if stderr.trim().is_empty() {
        clean_dolt_message(&stdout)
    } else {
        clean_dolt_message(&stderr)
    };

    if is_unrelated_history(&format!("{stderr}{stdout}")) {
        return Err(QuizdomError::Dolt(unrelated_history_message(
            config, &reported,
        )));
    }
    Err(QuizdomError::Dolt(format!(
        "dolt {} failed: {reported}",
        args.join(" ")
    )))
}

// trace:BUG-277 | ai:claude
/// Dolt decorates its terminal output with backspace runs that erase the
/// `- Uploading...` spinner in place, and `\x08` is not whitespace — so
/// `str::trim` leaves it behind and a forwarded stream trails a line of
/// control characters through the error. Strip the control characters, drop
/// the lines that were nothing but spinner, and join what is left.
fn clean_dolt_message(raw: &str) -> String {
    raw.lines()
        .map(|line| {
            line.chars()
                .filter(|character| !character.is_control())
                .collect::<String>()
                .trim()
                .to_string()
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

// trace:BUG-277 | ai:claude
/// The actionable replacement for `unknown push error; no common ancestor`:
/// name the directory, say what is wrong with it in quizdom's vocabulary, and
/// list the ways out. A durability command whose failure mode is an opaque
/// engine string is a durability command people learn to ignore — and the way
/// out is genuinely not obvious, because nothing here is broken: the backup is
/// intact, the graph is intact, they simply belong to different lineages.
///
/// None of the offered options destroy anything. The move-aside preserves the
/// foreign copy under a suffix rather than deleting it, which is exactly what
/// the advisor did by hand when BUG-277 was found.
fn unrelated_history_message(config: &DbBackupConfig, reported: &str) -> String {
    let backup = config.backup.display();
    let repo = config.path.display();
    format!(
        "the backup directory {backup} already holds an UNRELATED Dolt \
         repository: its history shares no commit with {repo}, so the push was \
         refused. These are two separate lineages, not a diverged branch — \
         there is no ancestor to reconcile them against, and quizdom will not \
         overwrite one backup with another to force it.\n\
         \n\
         Nothing reached the backup: {backup} still holds only the other \
         lineage, and {repo} is intact — a push transfers or it does not. The \
         usual cause is that the directory was claimed by a different repo — a \
         throwaway fixture from a test or verification run, or another \
         project's graph.\n\
         \n\
         Pick one:\n\
         \x20 * back this graph up somewhere else:\n\
         \x20     quizdom db-backup --to <fresh-empty-directory>\n\
         \x20 * see what is in there first (clones it out, writes nothing):\n\
         \x20     quizdom db-restore --path /tmp/quizdom-foreign-check --from {backup}\n\
         \x20 * retire the foreign copy, then back up here again (a move, \
         never a delete —\n\
         \x20   the other lineage stays recoverable):\n\
         \x20     mv {backup} {backup}.foreign-lineage\n\
         \x20     quizdom db-backup\n\
         \n\
         (dolt reported: {reported})"
    )
}

// trace:BUG-277 | ai:claude
/// A test that writes to the REAL backup directory destroys the developer's
/// only off-repo copy of the domain graph. That is not hypothetical — it is
/// BUG-277: a verification run with no `--to` pushed a throwaway fixture to
/// the default backup path, and every later backup of the real graph failed.
///
/// So under `cargo test` both directories must live under the system temp
/// directory. A whitelist, deliberately, not a blacklist of known-real paths:
/// a test that forgets to pin cannot reach `data/dolt`, the platform data
/// dir, or anywhere else that matters by any route. Every test in this crate
/// is an in-crate `#[cfg(test)]` unit test — there is no `tests/` directory
/// compiling the lib without `cfg(test)` — so this covers the whole suite.
///
/// It compiles out entirely in a real build: the CLI is *supposed* to write
/// to the real paths.
#[cfg(test)]
fn guard_test_paths(config: &DbBackupConfig) {
    for (flag, path) in [("--path", &config.path), ("--to/--from", &config.backup)] {
        let resolved = absolute(path);
        let temp = std::env::temp_dir();
        let under_temp = resolved.starts_with(&temp)
            || std::fs::canonicalize(&temp)
                .map(|canonical| resolved.starts_with(canonical))
                .unwrap_or(false);
        assert!(
            under_temp,
            "BUG-277 tripwire: a test aimed {flag} at {}, outside {}. Tests \
             must pin every db-backup / db-restore path into a temp directory \
             — writing to the resolved real paths poisons the developer's \
             actual domain graph and its backup.",
            resolved.display(),
            temp.display()
        );
    }
}

#[cfg(not(test))]
#[inline]
fn guard_test_paths(_config: &DbBackupConfig) {}

/// The `file://` URL for a backup directory. Dolt requires an ABSOLUTE path
/// after the scheme, so a relative `--to data/backup` is resolved against the
/// current directory first.
fn file_remote_url(backup: &Path) -> Result<String> {
    let absolute = std::fs::canonicalize(backup).unwrap_or_else(|_| absolute(backup));
    Ok(format!("file://{}", absolute.display()))
}

/// A best-effort absolute path that does NOT require the path to exist
/// (`canonicalize` does). Used for clone targets, which by definition don't.
fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// The URL currently configured for `name`, or `None` when the repo has no
/// such remote. Parses `dolt remote -v` output, one `<name> <url>` per line.
fn existing_remote_url(runner: &dyn DoltRunner, repo: &Path, name: &str) -> Result<Option<String>> {
    let output = run_dolt(runner, repo, &["remote", "-v"])?;
    let listing = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(listing.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next() == Some(name)).then(|| fields.next().unwrap_or_default().to_string())
    }))
}

fn run_dolt(runner: &dyn DoltRunner, cwd: &Path, args: &[&str]) -> Result<Output> {
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let output = runner.run(cwd, &args)?;
    if !output.status.success() {
        return Err(QuizdomError::Dolt(format!(
            "dolt {} failed: {}",
            args.join(" "),
            clean_dolt_message(&String::from_utf8_lossy(&output.stderr))
        )));
    }
    Ok(output)
}

// trace:STORY-261 | ai:claude
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    /// Records every dolt invocation and replays canned `(raw_status, stdout,
    /// stderr)` responses in FIFO order (the `db_init` test-runner shape).
    struct RecordingDoltRunner {
        calls: RefCell<Vec<Vec<String>>>,
        responses: RefCell<Vec<(i32, String, String)>>,
    }

    impl RecordingDoltRunner {
        fn new(responses: Vec<(i32, &str, &str)>) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                responses: RefCell::new(
                    responses
                        .into_iter()
                        .map(|(status, out, err)| (status, out.to_string(), err.to_string()))
                        .collect(),
                ),
            }
        }

        fn call_names(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .map(|call| call.join(" "))
                .collect()
        }
    }

    impl DoltRunner for RecordingDoltRunner {
        fn run(&self, _cwd: &Path, args: &[String]) -> Result<Output> {
            self.calls.borrow_mut().push(args.to_vec());
            let (raw_status, stdout, stderr) = {
                let mut responses = self.responses.borrow_mut();
                if responses.is_empty() {
                    (0, String::new(), String::new())
                } else {
                    responses.remove(0)
                }
            };
            Ok(Output {
                status: ExitStatus::from_raw(raw_status),
                stdout: stdout.into_bytes(),
                stderr: stderr.into_bytes(),
            })
        }
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("quizdom-db-backup-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn config(path: &Path, backup: &Path) -> DbBackupConfig {
        DbBackupConfig {
            path: path.to_path_buf(),
            backup: backup.to_path_buf(),
            remote: BACKUP_REMOTE_NAME.to_string(),
            dolt_command: "dolt".to_string(),
        }
    }

    #[test]
    fn backup_adds_the_remote_then_pushes() {
        let repo = temp_dir("add");
        std::fs::create_dir_all(repo.join(".dolt")).unwrap();
        let backup = temp_dir("add-dest");
        // `remote -v` empty => no remote configured yet.
        let runner = RecordingDoltRunner::new(vec![(0, "", "")]);
        let mut output = Vec::new();

        db_backup(&config(&repo, &backup), &runner, &mut output).expect("backup should succeed");

        let calls = runner.call_names();
        assert_eq!(calls.len(), 5, "remote -v, add, snapshot, push: {calls:?}");
        assert_eq!(calls[0], "remote -v");
        assert!(
            calls[1].starts_with("remote add backup file://"),
            "{calls:?}"
        );
        // The snapshot precedes the push — pushing an uncommitted working set
        // would upload nothing and report success.
        assert_eq!(calls[2], "add -A");
        assert_eq!(calls[3], format!("commit -m {SNAPSHOT_MESSAGE}"));
        assert_eq!(calls[4], "push backup main");
        assert!(backup.exists(), "backup directory is created");
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("Added remote `backup`"));
        assert!(rendered.contains("Committed pending working-set changes"));
        assert!(rendered.contains("quizdom db-restore"), "prints recovery");

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&backup);
    }

    #[test]
    fn backup_leaves_a_matching_remote_alone() {
        let repo = temp_dir("same");
        std::fs::create_dir_all(repo.join(".dolt")).unwrap();
        let backup = temp_dir("same-dest");
        std::fs::create_dir_all(&backup).unwrap();
        let url = file_remote_url(&backup).unwrap();
        let runner = RecordingDoltRunner::new(vec![
            (0, &format!("backup {url} \n"), ""),
            (0, "", ""),                                // add -A
            (1 << 8, "no changes added to commit", ""), // nothing new since last backup
        ]);
        let mut output = Vec::new();

        db_backup(&config(&repo, &backup), &runner, &mut output).expect("backup should succeed");

        let calls = runner.call_names();
        assert_eq!(
            calls,
            [
                "remote -v",
                "add -A",
                &format!("commit -m {SNAPSHOT_MESSAGE}"),
                "push backup main"
            ],
            "no re-add, and a clean tree is not a failure"
        );
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("already points at"));
        assert!(
            !rendered.contains("Committed pending"),
            "nothing to snapshot => no snapshot line: {rendered}"
        );

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&backup);
    }

    #[test]
    fn backup_repoints_a_stale_remote() {
        let repo = temp_dir("moved");
        std::fs::create_dir_all(repo.join(".dolt")).unwrap();
        let backup = temp_dir("moved-dest");
        let runner = RecordingDoltRunner::new(vec![(0, "backup file:///gone/elsewhere \n", "")]);
        let mut output = Vec::new();

        db_backup(&config(&repo, &backup), &runner, &mut output).expect("backup should succeed");

        let calls = runner.call_names();
        assert_eq!(calls[1], "remote remove backup");
        assert!(calls[2].starts_with("remote add backup file://"));
        assert_eq!(calls[5], "push backup main");
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("Re-pointed remote `backup` from file:///gone/elsewhere"));

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&backup);
    }

    #[test]
    fn backup_surfaces_a_real_commit_failure() {
        let repo = temp_dir("badcommit");
        std::fs::create_dir_all(repo.join(".dolt")).unwrap();
        let backup = temp_dir("badcommit-dest");
        let runner = RecordingDoltRunner::new(vec![
            (0, "", ""),                             // remote -v
            (0, "", ""),                             // remote add
            (0, "", ""),                             // add -A
            (1 << 8, "", "author identity unknown"), // commit
        ]);

        match db_backup(&config(&repo, &backup), &runner, &mut Vec::new()) {
            Err(QuizdomError::Dolt(message)) => {
                assert!(message.contains("author identity unknown"), "{message}")
            }
            other => panic!("expected a Dolt error, got {other:?}"),
        }
        assert!(
            !runner
                .call_names()
                .iter()
                .any(|call| call.starts_with("push")),
            "a failed snapshot must not push a half-backup"
        );

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&backup);
    }

    #[test]
    fn backup_without_a_repo_says_run_db_init() {
        let repo = temp_dir("norepo");
        let backup = temp_dir("norepo-dest");
        let runner = RecordingDoltRunner::new(vec![]);

        match db_backup(&config(&repo, &backup), &runner, &mut Vec::new()) {
            Err(QuizdomError::Dolt(message)) => assert!(message.contains("db-init"), "{message}"),
            other => panic!("expected a Dolt error, got {other:?}"),
        }
        assert!(runner.call_names().is_empty(), "no dolt spawned");
    }

    // trace:BUG-277 | ai:claude
    /// BUG-277's second defect: a backup directory holding a foreign lineage
    /// must produce quizdom's guidance, not dolt's `unknown push error; no
    /// common ancestor`. The stderr here is verbatim what dolt 2.2.1 emits.
    #[test]
    fn backup_translates_an_unrelated_history_refusal() {
        let repo = temp_dir("foreign");
        std::fs::create_dir_all(repo.join(".dolt")).unwrap();
        let backup = temp_dir("foreign-dest");
        let runner = RecordingDoltRunner::new(vec![
            (0, "", ""), // remote -v
            (0, "", ""), // remote add
            (0, "", ""), // add -A
            (0, "", ""), // commit
            (
                1 << 8,
                "- Uploading...",
                "unknown push error; no common ancestor",
            ),
        ]);

        match db_backup(&config(&repo, &backup), &runner, &mut Vec::new()) {
            Err(QuizdomError::Dolt(message)) => {
                assert!(
                    message.contains(&backup.display().to_string()),
                    "names the backup directory: {message}"
                );
                assert!(
                    message.contains(&repo.display().to_string()),
                    "names the repo it refused to push: {message}"
                );
                assert!(
                    message.contains("UNRELATED"),
                    "says what is wrong: {message}"
                );
                assert!(
                    message.contains("--to <fresh-empty-directory>")
                        && message.contains(".foreign-lineage"),
                    "offers the ways out: {message}"
                );
                assert!(
                    !message.starts_with("dolt push"),
                    "not the raw engine string: {message}"
                );
                assert!(
                    !message.contains("Uploading"),
                    "the upload spinner is not a diagnosis: {message}"
                );
            }
            other => panic!("expected a Dolt error, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&backup);
    }

    // trace:BUG-277 | ai:claude
    /// The translation is targeted: an ordinary push failure keeps `run_dolt`'s
    /// plain shape rather than being mis-diagnosed as a foreign lineage.
    #[test]
    fn backup_leaves_other_push_failures_in_their_plain_shape() {
        let repo = temp_dir("pushfail");
        std::fs::create_dir_all(repo.join(".dolt")).unwrap();
        let backup = temp_dir("pushfail-dest");
        let runner = RecordingDoltRunner::new(vec![
            (0, "", ""), // remote -v
            (0, "", ""), // remote add
            (0, "", ""), // add -A
            (0, "", ""), // commit
            (1 << 8, "", "permission denied"),
        ]);

        match db_backup(&config(&repo, &backup), &runner, &mut Vec::new()) {
            Err(QuizdomError::Dolt(message)) => assert_eq!(
                message, "dolt push backup main failed: permission denied",
                "unrelated failures are not rewritten"
            ),
            other => panic!("expected a Dolt error, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&backup);
    }

    // trace:BUG-277 | ai:claude
    /// BUG-277's first defect, as a test of the guard itself: any test aiming
    /// these commands outside the temp directory fails loudly instead of
    /// quietly writing to the developer's real graph or its backup.
    #[test]
    #[should_panic(expected = "BUG-277 tripwire")]
    fn aiming_a_test_outside_the_temp_directory_trips_the_guard() {
        let runner = RecordingDoltRunner::new(vec![]);
        let _ = db_backup(
            &config(
                Path::new("/var/lib/quizdom-must-never-be-touched"),
                Path::new("/var/lib/quizdom-must-never-be-touched-backup"),
            ),
            &runner,
            &mut Vec::new(),
        );
    }

    #[test]
    fn restore_clones_into_a_missing_path() {
        let repo = temp_dir("restore");
        let backup = temp_dir("restore-src");
        std::fs::create_dir_all(&backup).unwrap();
        let runner = RecordingDoltRunner::new(vec![(0, "", "")]);
        let mut output = Vec::new();

        db_restore(&config(&repo, &backup), &runner, &mut output).expect("restore should succeed");

        let calls = runner.call_names();
        assert_eq!(calls.len(), 1, "clone only: {calls:?}");
        assert!(calls[0].starts_with("clone file://"), "{calls:?}");
        assert!(calls[0].ends_with(&repo.display().to_string()), "{calls:?}");
        assert!(String::from_utf8(output).unwrap().contains("Restored"));

        let _ = std::fs::remove_dir_all(&backup);
    }

    #[test]
    fn restore_refuses_to_overwrite_an_existing_repo() {
        let repo = temp_dir("occupied");
        std::fs::create_dir_all(repo.join(".dolt")).unwrap();
        let backup = temp_dir("occupied-src");
        std::fs::create_dir_all(&backup).unwrap();
        let runner = RecordingDoltRunner::new(vec![]);

        match db_restore(&config(&repo, &backup), &runner, &mut Vec::new()) {
            Err(QuizdomError::Dolt(message)) => {
                assert!(message.contains("refuses to overwrite"), "{message}")
            }
            other => panic!("expected a Dolt error, got {other:?}"),
        }
        assert!(runner.call_names().is_empty(), "no dolt spawned");

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&backup);
    }

    #[test]
    fn restore_without_a_backup_is_an_error() {
        let repo = temp_dir("nobackup");
        let backup = temp_dir("nobackup-src");
        let runner = RecordingDoltRunner::new(vec![]);

        match db_restore(&config(&repo, &backup), &runner, &mut Vec::new()) {
            Err(QuizdomError::Dolt(message)) => {
                assert!(message.contains("nothing to restore from"), "{message}")
            }
            other => panic!("expected a Dolt error, got {other:?}"),
        }
    }

    #[test]
    fn parse_reads_overrides_and_keeps_resolved_defaults() {
        let config = DbBackupConfig::parse(
            [
                "db-backup",
                "--path",
                "/tmp/x",
                "--to",
                "/tmp/b",
                "--dolt",
                "dolt2",
            ]
            .map(String::from),
            "db-backup",
            PathBuf::from("/from/env"),
            PathBuf::from("/from/env-backup"),
        )
        .unwrap();
        assert_eq!(config.path, PathBuf::from("/tmp/x"));
        assert_eq!(config.backup, PathBuf::from("/tmp/b"));
        assert_eq!(config.dolt_command, "dolt2");

        let defaulted = DbBackupConfig::parse(
            ["db-backup".to_string()],
            "db-backup",
            PathBuf::from("/from/env"),
            PathBuf::from("/from/env-backup"),
        )
        .unwrap();
        assert_eq!(defaulted.path, PathBuf::from("/from/env"));
        assert_eq!(defaulted.backup, PathBuf::from("/from/env-backup"));
        assert_eq!(defaulted.remote, BACKUP_REMOTE_NAME);

        // `--from` is `db-restore`'s spelling of the same directory.
        let restore = DbBackupConfig::parse(
            ["db-restore", "--from", "/tmp/b"].map(String::from),
            "db-restore",
            PathBuf::from("/from/env"),
            PathBuf::from("/from/env-backup"),
        )
        .unwrap();
        assert_eq!(restore.backup, PathBuf::from("/tmp/b"));
    }

    #[test]
    fn unknown_argument_is_a_usage_error() {
        let result = DbBackupConfig::parse(
            ["db-backup".to_string(), "--bogus".to_string()],
            "db-backup",
            PathBuf::from("data/dolt"),
            PathBuf::from("/tmp/backup"),
        );
        assert!(matches!(result, Err(QuizdomError::Usage(_))));
    }

    /// The STORY-261 acceptance criterion as a test: seed a real Dolt repo,
    /// back it up, DELETE the repo, restore it, and prove the rows came back.
    /// Runs in CI now that the pipeline installs dolt (TASK-219); locally with:
    /// cargo test real_dolt -- --ignored
    #[test]
    #[ignore = "requires the dolt binary on PATH"]
    fn real_dolt_backup_restore_round_trip() {
        let repo = temp_dir("real");
        let backup = temp_dir("real-dest");
        let runner = SystemDoltRunner::new("dolt".to_string());
        crate::db_init::run_db_init(
            [
                "db-init".to_string(),
                "--path".to_string(),
                repo.display().to_string(),
            ],
            &mut Vec::new(),
        )
        .expect("bootstrap should succeed");

        run_dolt(
            &runner,
            &repo,
            &[
                "sql",
                "-q",
                "INSERT INTO nodes (id, kind, title, body, tags, weight) VALUES \
                 ('Q-1', 'question', 'seed question', 'body', 'topic:free-will', 70), \
                 ('TERM-1', 'term', 'free will', 'the term', '', 0); \
                 INSERT INTO edges (from_id, to_id, kind) VALUES ('Q-1', 'TERM-1', 'probes');",
            ],
        )
        .expect("fixture should load");
        // Deliberately NOT committed here: `db-init` + `db-migrate` leave the
        // graph in the working set, and the snapshot step is what makes it
        // survive the round trip.

        let config = config(&repo, &backup);
        db_backup(&config, &runner, &mut Vec::new()).expect("backup should succeed");
        // Idempotent: a second backup re-uses the already-pointed remote.
        db_backup(&config, &runner, &mut Vec::new()).expect("re-backup should succeed");

        // The disaster: the only copy of the domain graph is gone.
        std::fs::remove_dir_all(&repo).expect("simulated disk loss");

        db_restore(&config, &runner, &mut Vec::new()).expect("restore should succeed");

        let counted = run_dolt(
            &runner,
            &repo,
            &[
                "sql",
                "-r",
                "json",
                "-q",
                "SELECT (SELECT COUNT(*) FROM nodes) AS nodes, \
                 (SELECT COUNT(*) FROM edges) AS edges",
            ],
        )
        .expect("count query should run");
        let rendered = String::from_utf8_lossy(&counted.stdout).to_string();
        assert!(
            rendered.contains("\"nodes\":2") && rendered.contains("\"edges\":1"),
            "restored graph should carry the seeded rows: {rendered}"
        );

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&backup);
    }

    // trace:BUG-277 | ai:claude
    /// BUG-277's acceptance, replayed against the real engine: a backup
    /// directory pre-populated with an UNRELATED repo must produce quizdom's
    /// guidance, not dolt's raw refusal.
    ///
    /// The mock test above pins the translation; this one pins the trigger —
    /// that dolt still refuses in the way [`UNRELATED_HISTORY_MARKERS`]
    /// detects. Without it a dolt release could reword the message and the
    /// translation would silently stop firing, with every unit test green.
    #[test]
    #[ignore = "requires the dolt binary on PATH"]
    fn real_dolt_backup_refuses_a_foreign_lineage_backup() {
        let claimed = temp_dir("lineage-throwaway"); // the fixture that claims the dir
        let genuine = temp_dir("lineage-genuine"); // the graph we actually want backed up
        let backup = temp_dir("lineage-dest");
        let runner = SystemDoltRunner::new("dolt".to_string());

        for repo in [&claimed, &genuine] {
            crate::db_init::run_db_init(
                [
                    "db-init".to_string(),
                    "--path".to_string(),
                    repo.display().to_string(),
                ],
                &mut Vec::new(),
            )
            .expect("bootstrap should succeed");
        }

        // The BUG-277 sequence, exactly: a throwaway run claims the backup
        // directory first...
        db_backup(&config(&claimed, &backup), &runner, &mut Vec::new())
            .expect("the first push into an empty directory succeeds");

        // ...and now the graph that directory was meant to protect cannot be
        // backed up there at all.
        match db_backup(&config(&genuine, &backup), &runner, &mut Vec::new()) {
            Err(QuizdomError::Dolt(message)) => {
                assert!(
                    message.contains("UNRELATED")
                        && message.contains(&backup.display().to_string()),
                    "expected the actionable foreign-lineage message: {message}"
                );
                assert!(
                    !message.starts_with("dolt push"),
                    "must not forward the raw engine string: {message}"
                );
            }
            other => panic!("a foreign-lineage backup must be refused, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&claimed);
        let _ = std::fs::remove_dir_all(&genuine);
        let _ = std::fs::remove_dir_all(&backup);
    }
}
