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
//! first ([`snapshot_working_set`]). Every quizdom writer now commits its own
//! writes — the store per write (STORY-208), `db-init` its schema and
//! `db-migrate` its import (STORY-291) — so on a repo only quizdom has touched
//! the snapshot finds nothing to do. It stays because the repo is a database a
//! user can also write to directly: a hand-run `dolt sql -q 'UPDATE nodes …'`
//! leaves changes in the working set, and pushing without a snapshot would
//! silently leave them out of the backup.
//!
//! ## A backup directory belongs to exactly one lineage (BUG-277)
//!
//! A file remote is a directory, and a directory has no idea which repo is
//! entitled to it. Push two repos with unrelated roots at the same directory
//! and the second is refused — dolt has no ancestor to reconcile against. That
//! is the correct engine behaviour and a terrible user experience, so two
//! guards live here:
//!
//! * [`guard_test_paths`] — the `#[cfg(test)]` tripwire
//!   ([`crate::db_init::guard_test_path`]). No test may aim `db-backup` /
//!   `db-restore` at anything outside the system temp directory.
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
//! pre-existing foreign remote.
//!
//! ## Guarding the vector that actually fired (TASK-283)
//!
//! The tripwire above guards `cargo test`. The incident was a `cargo run`, and
//! for a while the only defence there was a documentation line asking people to
//! remember `--to`. Two guards now close it:
//!
//! * [`DbBackupConfig::parse`] REFUSES a `db-backup` that names a non-default
//!   `--path` without also naming `--to`. That mismatch — a source repo the
//!   settings chain did not choose, aimed at the backup directory it did — is
//!   the scratch-run signature exactly, and it is the one shape where claiming
//!   the default backup directory is never what was meant.
//! * `--force` ([`retire_foreign_backup`]) makes the documented way out of a
//!   foreign-lineage refusal executable rather than a prose recipe (TASK-278).
//!   It MOVES the foreign copy aside; nothing here ever deletes a backup.

use crate::db_init::{absolute, clean_dolt_message, DoltRunner, SystemDoltRunner};
use crate::db_migrate::sql_quote;
use crate::error::{QuizdomError, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicU64, Ordering};

/// Distinguishes concurrent restores that share a parent directory.
static RESTORE_SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// The remote name `db-backup` manages inside the domain-graph repo. Named
/// (not `origin`) so it cannot collide with the `origin` that `dolt clone`
/// writes during a restore — after a restore both names point at the same
/// backup directory, and re-running `db-backup` stays a no-op reconfigure.
pub const BACKUP_REMOTE_NAME: &str = "backup";

/// The branch pushed / restored. The domain graph only ever lives on Dolt's
/// default branch; `db-init` never creates another.
const BACKUP_BRANCH: &str = "main";

#[derive(Debug)]
struct DbBackupConfig {
    /// The domain-graph repo to push FROM (`db-restore`: to restore INTO).
    path: PathBuf,
    /// The backup directory serving as the file remote.
    backup: PathBuf,
    remote: String,
    dolt_command: String,
    /// `db-backup --force`: on a foreign-lineage refusal, retire the copy
    /// already in the backup directory (a move, never a delete) and push.
    force: bool,
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
        default_remote: String,
    ) -> Result<Self> {
        let mut path = default_path.clone();
        let mut backup = default_backup;
        // trace:TASK-324 | ai:claude — the remote name is a resolved default
        // now, not a constant, so a `backup_remote` in settings.toml reaches
        // both this command and the end-of-session probe.
        let mut remote = default_remote;
        let mut dolt_command = "dolt".to_string();
        let mut force = false;
        let mut path_is_explicit = false;
        let mut backup_is_explicit = false;
        let mut args = args.into_iter().peekable();

        if args.peek().map(String::as_str) == Some(subcommand) {
            args.next();
        }

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--path" => {
                    path = PathBuf::from(next_arg(&mut args, "--path")?);
                    path_is_explicit = true;
                }
                "--to" | "--from" => {
                    backup = PathBuf::from(next_arg(&mut args, &arg)?);
                    backup_is_explicit = true;
                }
                "--remote" => remote = next_arg(&mut args, "--remote")?,
                "--dolt" => dolt_command = next_arg(&mut args, "--dolt")?,
                "--force" if subcommand == "db-backup" => force = true,
                "--help" | "-h" => return Err(QuizdomError::Usage(usage(subcommand))),
                other => {
                    return Err(QuizdomError::Usage(format!(
                        "unknown argument: {other}\n{}",
                        usage(subcommand)
                    )))
                }
            }
        }

        // trace:TASK-283 | ai:claude
        if subcommand == "db-backup"
            && path_is_explicit
            && !backup_is_explicit
            && absolute(&path) != absolute(&default_path)
        {
            return Err(QuizdomError::Usage(unpinned_backup_message(
                &path,
                &backup,
                &default_path,
            )));
        }

        Ok(Self {
            path,
            backup,
            remote,
            dolt_command,
            force,
        })
    }
}

// trace:TASK-283 | ai:claude
/// BUG-277's actual vector, refused at the argument parser.
///
/// A `--path` the settings chain did NOT choose, paired with the backup
/// directory it DID, is a scratch run every time: a throwaway fixture about to
/// claim the directory holding the real graph's only off-machine copy. A
/// backup directory belongs to one lineage, and the first push is what claims
/// it — so this has to be caught before the push, not translated after it.
///
/// The escape is `--to`, which is what the run meant anyway. Pointing `--path`
/// at the resolved default is untouched, and so is a bare `db-backup`.
fn unpinned_backup_message(path: &Path, backup: &Path, default_path: &Path) -> String {
    format!(
        "refusing to back up {} to the DEFAULT backup directory {}.\n\
         \n\
         `--path` names a repo the settings chain did not choose (it resolves \
         {}), so this looks like a scratch or verification run — and the first \
         push into a backup directory CLAIMS it for that repo's lineage. That \
         is exactly how BUG-277 poisoned a real backup: every later backup of \
         the genuine graph was then refused.\n\
         \n\
         Pin the destination too:\n\
         \x20   quizdom db-backup --path {} --to <scratch-backup-directory>\n\
         \n\
         (backing up the resolved default repo needs no flags at all: \
         `quizdom db-backup`)",
        shell_quote(path),
        shell_quote(backup),
        shell_quote(default_path),
        shell_quote(path),
    )
}

fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| QuizdomError::Usage(format!("{name} requires a value")))
}

fn usage(subcommand: &str) -> String {
    let (direction, force) = if subcommand == "db-restore" {
        ("--from <backup-dir>", "")
    } else {
        ("--to <backup-dir>", " [--force]")
    };
    format!(
        "usage: quizdom {subcommand} [--path <dir>] [{direction}] \
         [--remote <name>] [--dolt dolt]{force}\n\
         (--path defaults to $QUIZDOM_DOLT_PATH, else dolt_path in settings.toml;\n\
         the backup directory defaults to $QUIZDOM_DOLT_BACKUP_PATH, else \
         dolt_backup_path in settings.toml, else the platform data dir;\n\
         --remote defaults to $QUIZDOM_BACKUP_REMOTE, else backup_remote in \
         settings.toml, else `{BACKUP_REMOTE_NAME}`;\n\
         --path away from that default requires an explicit --to;\n\
         --force retires a foreign lineage already in the backup directory \
         (a move, never a delete))"
    )
}

// trace:TASK-275 | ai:claude
/// POSIX-quote a path for the copy-paste command hints these two commands
/// print. `OVERVIEW.md`'s durability section recommends a synced folder or a
/// removable disk as the backup directory — "Google Drive", "My Passport" —
/// and an unquoted path with a space in it makes the printed recovery command
/// silently wrong at exactly the moment someone needs it to work.
///
/// Single quotes, because they suppress every other shell metacharacter; an
/// embedded `'` closes, escapes, and reopens. Paths made of safe characters
/// stay bare, so the common case reads the way it always did.
fn shell_quote(path: &Path) -> String {
    let rendered = path.display().to_string();
    let is_safe =
        |character: char| character.is_ascii_alphanumeric() || "._-/=:+,@".contains(character);
    if !rendered.is_empty() && rendered.chars().all(is_safe) {
        return rendered;
    }
    format!("'{}'", rendered.replace('\'', r"'\''"))
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
        crate::settings::resolve_backup_remote(),
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
        crate::settings::resolve_backup_remote(),
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

    push_to_backup(runner, config, output)?;
    writeln!(
        output,
        "Pushed {} ({BACKUP_BRANCH}) to {url}.\n\
         Restore with: quizdom db-restore --path {} --from {}",
        config.path.display(),
        // trace:TASK-275 | ai:claude — a printed command has to be runnable.
        shell_quote(&config.path),
        shell_quote(&config.backup)
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
    //
    // The name must be unique per CALL, not per process: two restores sharing
    // a parent (every `#[test]` under /tmp) would otherwise agree on one
    // scratch path, and the first to finish deletes it out from under the
    // other's running clone — which then dies on `getwd: no such file or
    // directory`.
    let scratch = parent.join(format!(
        ".quizdom-restore-{}-{}",
        std::process::id(),
        RESTORE_SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
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
        // trace:TASK-275 | ai:claude
        shell_quote(&config.path)
    )?;
    Ok(())
}

/// The commit message for the snapshot `db-backup` takes before pushing.
const SNAPSHOT_MESSAGE: &str = "quizdom db-backup: snapshot working set";

/// Commit whatever sits in the working set, returning whether anything was
/// actually committed.
///
/// A push carries COMMITTED data only. Every quizdom writer commits its own
/// writes (STORY-208 for the store, STORY-291 for `db-init` / `db-migrate`), so
/// this is a backstop rather than the main event: what it catches is a change
/// made outside quizdom — a `dolt sql -q` run by hand in the repo — which would
/// otherwise be pushed-around rather than pushed.
///
/// `dolt commit` exits non-zero on a clean tree; that is the ordinary case
/// (nothing new since the last backup), not a failure. Since TASK-276 the
/// shared tail settles that question against the `dolt_status` system table
/// rather than by reading dolt's prose, so a reworded dolt release no longer
/// turns an ordinary backup into a hard failure.
///
// trace:STORY-291 | ai:claude — one commit tail, shared with db-init /
// db-migrate / the store.
fn snapshot_working_set(runner: &dyn DoltRunner, repo: &Path) -> Result<bool> {
    crate::db_init::commit_all(runner, repo, SNAPSHOT_MESSAGE)
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
fn push_to_backup(
    runner: &dyn DoltRunner,
    config: &DbBackupConfig,
    output: &mut impl Write,
) -> Result<()> {
    let args = ["push", config.remote.as_str(), BACKUP_BRANCH].map(String::from);
    let pushed = runner.run(&config.path, &args)?;
    if pushed.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&pushed.stderr).to_string();
    let stdout = String::from_utf8_lossy(&pushed.stdout).to_string();
    // Dolt puts the diagnosis on stderr and an `- Uploading...` spinner on
    // stdout. Scan BOTH for the marker (a future release could move it), but
    // report only the stream carrying the message, so the spinner never lands
    // in a user-facing error.
    let reported = if stderr.trim().is_empty() {
        clean_dolt_message(&stdout)
    } else {
        clean_dolt_message(&stderr)
    };

    // trace:TASK-284 | ai:claude — separate the streams before matching. Joined
    // edge-to-edge, a stderr ending `...no common` and a stdout opening
    // `ancestor...` would form a marker that neither stream contains.
    if is_unrelated_history(&format!("{stderr}\n{stdout}")) {
        if !config.force {
            return Err(QuizdomError::Dolt(unrelated_history_message(
                config, &reported,
            )));
        }
        return force_push_over_foreign_lineage(runner, config, &args, output);
    }
    Err(QuizdomError::Dolt(format!(
        "dolt {} failed: {reported}",
        args.join(" ")
    )))
}

// trace:TASK-278 | ai:claude
// trace:TASK-283 | ai:claude
/// `--force`: retire the foreign lineage sitting in the backup directory and
/// push again.
///
/// BUG-277 shipped this as a two-line `mv` recipe inside the error message,
/// which meant the one documented way out of a poisoned backup directory was
/// something the tool described but could not do. It is a flag now — with the
/// same semantics the advisor applied by hand when BUG-277 was found, and the
/// same ones the message always promised: the displaced copy is MOVED to a
/// suffixed sibling and stays recoverable. Nothing on this path deletes a
/// backup, so a `--force` aimed at the wrong directory costs a rename.
fn force_push_over_foreign_lineage(
    runner: &dyn DoltRunner,
    config: &DbBackupConfig,
    args: &[String],
    output: &mut impl Write,
) -> Result<()> {
    let retired = retire_foreign_backup(&config.backup)?;
    writeln!(
        output,
        "--force: retired the foreign lineage in {} to {} (moved, not deleted).",
        shell_quote(&config.backup),
        shell_quote(&retired)
    )?;
    std::fs::create_dir_all(&config.backup)?;

    let retried = runner.run(&config.path, args)?;
    if retried.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&retried.stderr).to_string();
    let stdout = String::from_utf8_lossy(&retried.stdout).to_string();
    let reported = if stderr.trim().is_empty() {
        clean_dolt_message(&stdout)
    } else {
        clean_dolt_message(&stderr)
    };
    Err(QuizdomError::Dolt(format!(
        "dolt {} still failed after --force retired the foreign lineage to {}: \
         {reported}",
        args.join(" "),
        retired.display()
    )))
}

// trace:TASK-278 | ai:claude
/// Move a backup directory aside to a free `.foreign-lineage` sibling and
/// report where it went. Never overwrites an earlier retirement — a second
/// `--force` against the same directory lands on `.foreign-lineage.2`, so no
/// sequence of forced backups can destroy a lineage.
fn retire_foreign_backup(backup: &Path) -> Result<PathBuf> {
    let base = format!("{}.foreign-lineage", backup.display());
    let mut retired = PathBuf::from(&base);
    let mut suffix = 2;
    while retired.exists() {
        retired = PathBuf::from(format!("{base}.{suffix}"));
        suffix += 1;
    }
    std::fs::rename(backup, &retired)?;
    Ok(retired)
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
/// the advisor did by hand when BUG-277 was found — and since TASK-278 it is
/// `--force`, a thing the tool does, rather than a recipe the user retypes.
///
/// Every suggested command spells out `--path` / `--to` even when they were not
/// typed. This message is reached from a run that may have pinned either one,
/// and a hint that silently means "the defaults" would send a user who did pin
/// them at a different pair of directories than the failure they are reading
/// about.
fn unrelated_history_message(config: &DbBackupConfig, reported: &str) -> String {
    let backup = shell_quote(&config.backup);
    let repo = shell_quote(&config.path);
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
         \x20     quizdom db-backup --path {repo} --to <fresh-empty-directory>\n\
         \x20 * see what is in there first (clones it out, writes nothing):\n\
         \x20     quizdom db-restore --path /tmp/quizdom-foreign-check --from {backup}\n\
         \x20 * retire the foreign copy and back up here anyway — it is moved \
         to\n\
         \x20   {backup}.foreign-lineage, never deleted, so that lineage stays \
         recoverable:\n\
         \x20     quizdom db-backup --path {repo} --to {backup} --force\n\
         \n\
         (dolt reported: {reported})"
    )
}

// trace:BUG-277 | ai:claude
// trace:TASK-280 | ai:claude
/// Both of this command's paths, through the shared tripwire
/// ([`crate::db_init::guard_test_path`] — which `db-init`, `db-migrate` and the
/// store constructor now call too, so no test in the workspace can reach a
/// resolved real path by any route).
fn guard_test_paths(config: &DbBackupConfig) {
    crate::db_init::guard_test_path("--path", &config.path);
    crate::db_init::guard_test_path("--to/--from", &config.backup);
}

/// The `file://` URL for a backup directory. Dolt requires an ABSOLUTE path
/// after the scheme, so a relative `--to data/backup` is resolved against the
/// current directory first.
fn file_remote_url(backup: &Path) -> Result<String> {
    let absolute = std::fs::canonicalize(backup).unwrap_or_else(|_| absolute(backup));
    Ok(format!("file://{}", absolute.display()))
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

// ---------------------------------------------------------------------------
// trace:STORY-299 | ai:claude — TASK-273: "explicit" must not come to mean
// "forgotten".
//
// The decision STORY-299 recorded is that `db-backup` STAYS the primitive: an
// implicit network/disk write at the end of every session is surprising, costs
// seconds of dolt spawns, and can fail in ways that muddy the end of a session.
// What that leaves is the real gap — a user who writes to the graph for weeks
// and never runs the command, with the working copy drifting further from the
// backup every session and nothing anywhere saying so.
//
// So the ergonomics close instead of the default: a session that MOVED the
// graph and sits ahead of its backup remote says so in one line on the way out,
// naming the exact command; and `auto_backup = true` (off by default) is there
// for anyone who would rather have the push than the reminder.
// ---------------------------------------------------------------------------

/// Where the working copy stands relative to its backup remote.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum BackupPosition {
    /// `main` matches the last successful push and nothing is uncommitted —
    /// a `db-backup` now would transfer nothing.
    UpToDate,
    /// The working copy holds something the backup does not: commits since the
    /// last push, an uncommitted working set, or no push has ever happened.
    Ahead,
    /// The question could not be answered — no repo, no dolt, an older dolt
    /// without the system tables, unparseable output. Deliberately distinct
    /// from [`Self::Ahead`]: a probe that failed is not evidence of drift, so
    /// what the user is told is "could not tell", never "you have changes to
    /// push".
    ///
    /// Both halves of the session end now act on it (TASK-325, TASK-328):
    /// an opted-in `auto_backup` PUSHES anyway (a redundant push is cheaper
    /// than a skipped one for someone who asked for the push), and a session
    /// without `auto_backup` SAYS it could not tell. Either way the blind probe
    /// is recorded.
    Unknown,
}

/// The one query behind [`backup_position`].
///
/// Local-only and read-only: `dolt_remote_branches` is the remote-TRACKING ref,
/// updated by push/fetch, so this never touches the backup directory — which
/// matters because the backup may be a removable disk or a synced folder that
/// is not mounted right now. quizdom's own `db-backup` is the only thing that
/// pushes here, so the tracking ref is an accurate answer rather than a stale
/// approximation.
///
/// `dolt_status` joins in because [`snapshot_working_set`] would commit an
/// uncommitted working set on the way out: a hand-run `dolt sql -q 'UPDATE
/// nodes …'` leaves the graph ahead of its backup even though no commit moved.
fn backup_position_sql(remote: &str) -> String {
    format!(
        "SELECT \
         (SELECT hash FROM dolt_branches WHERE name = {branch}) AS local_hash, \
         (SELECT hash FROM dolt_remote_branches WHERE name = {tracking}) AS backup_hash, \
         (SELECT COUNT(*) FROM dolt_status) AS pending",
        branch = sql_quote(BACKUP_BRANCH),
        tracking = sql_quote(&format!("remotes/{remote}/{BACKUP_BRANCH}")),
    )
}

/// Read [`backup_position_sql`]'s single row. Split out so the interpretation
/// is testable without a dolt to spawn.
///
/// An ABSENT `backup_hash` (no such remote-tracking ref) reads as [`Ahead`],
/// not [`Unknown`]: "you have never backed this graph up" is the case the
/// reminder exists for. An absent `local_hash` reads as [`Unknown`] — a repo
/// with no `main` is not a repo this command understands.
///
/// [`Ahead`]: BackupPosition::Ahead
/// [`Unknown`]: BackupPosition::Unknown
fn backup_position_from_row(
    row: Option<&serde_json::Map<String, serde_json::Value>>,
) -> BackupPosition {
    let Some(row) = row else {
        return BackupPosition::Unknown;
    };
    let text = |key: &str| row.get(key).and_then(serde_json::Value::as_str);
    let Some(local) = text("local_hash") else {
        return BackupPosition::Unknown;
    };
    // dolt's JSON renders COUNT(*) as a number in some releases and a string in
    // others; accept either rather than losing the dirty-tree signal to a
    // formatting change.
    let pending = row
        .get("pending")
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
        .unwrap_or(0);
    if pending > 0 || text("backup_hash") != Some(local) {
        return BackupPosition::Ahead;
    }
    BackupPosition::UpToDate
}

/// Ask the repo where it stands relative to its backup. Never fails: every
/// error is [`BackupPosition::Unknown`], because this runs on the way out of a
/// session and must not turn a completed session into an error.
fn backup_position(runner: &dyn DoltRunner, repo: &Path, remote: &str) -> BackupPosition {
    if !repo.join(".dolt").exists() {
        return BackupPosition::Unknown;
    }
    let args = ["sql", "-r", "json", "-q", &backup_position_sql(remote)].map(String::from);
    let Ok(output) = runner.run(repo, &args) else {
        return BackupPosition::Unknown;
    };
    if !output.status.success() {
        return BackupPosition::Unknown;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout.trim()) else {
        return BackupPosition::Unknown;
    };
    backup_position_from_row(
        value
            .get("rows")
            .and_then(serde_json::Value::as_array)
            .and_then(|rows| rows.first())
            .and_then(serde_json::Value::as_object),
    )
}

/// The one-line reminder. Names the exact command — a bare `quizdom db-backup`
/// resolves the same env > settings > default chain this session did, so it
/// needs no flags to mean the right pair of directories — and names the
/// destination, because the whole point is that the user has not thought about
/// this directory recently.
pub(crate) fn backup_reminder(backup: &Path) -> String {
    format!(
        "Domain graph has changes not in its backup — run `quizdom db-backup` to push them to {}.",
        backup.display()
    )
}

// trace:TASK-328 | ai:claude
/// The reminder's honest sibling: what a session says when the probe could not
/// answer at all.
///
/// It is deliberately a DIFFERENT claim from [`backup_reminder`]. The reminder
/// asserts drift; this one asserts only ignorance, and names `quizdom logs`
/// because the reason the probe failed is already in the diagnostic log and is
/// the actionable part — a missing dolt, a repo that is not there, a dolt too
/// old for the system tables.
fn blind_probe_notice(backup: &Path) -> String {
    format!(
        "Could not tell whether the domain graph is backed up — run `quizdom db-backup` \
         to be sure it reaches {}, and `quizdom logs` for why the check failed.",
        backup.display()
    )
}

/// The end-of-session decision, with the push passed in so the branching is
/// testable without spawning dolt.
///
/// `Ahead` + auto-backup OFF is the reminder. `Ahead` + auto-backup ON attempts
/// the push and reports it — and on failure DEGRADES to the reminder rather
/// than surfacing an error, which is the STORY-299 rule: a backup that did not
/// happen must never be the thing that ends a session badly. The user who opted
/// in still learns the graph is unbacked-up, and still gets the command.
///
/// # `Unknown` — both halves are honest about not knowing (TASK-325, TASK-328)
///
/// A probe that could not answer is not evidence of drift, so `Unknown` must
/// never produce the reminder's claim ("you have changes to push"). For a while
/// that was implemented as SILENCE, and silence turned out to be the wrong
/// reading of it twice over.
///
/// **The push (TASK-325).** Reading `Unknown` as "nothing to do" turned an
/// `auto_backup = true` the user explicitly opted into into a silent no-op —
/// the exact failure `auto_backup` exists to prevent. So **`Unknown` +
/// auto-backup ON attempts the push anyway**: a redundant push costs seconds
/// against a directory the user already told us to push to, and someone who set
/// `auto_backup = true` has said which way they want that traded.
///
/// **The reminder (TASK-328).** The other half stayed silent, which left the
/// user WITHOUT `auto_backup` — the default — getting nothing at all from a
/// probe that could not tell. Nothing is what a backed-up graph looks like, so
/// the failure was invisible in exactly the configuration most people run. So
/// **`Unknown` + auto-backup OFF says it could not tell**
/// ([`blind_probe_notice`]).
///
/// The original worry — that nagging on a broken probe trains users to ignore
/// the line — is answered by two things rather than by silence. The footer only
/// fires at all when THIS session wrote to the graph
/// ([`session_end_durability`]), so it is feedback on what you just did rather
/// than ambient nagging; and the line makes a weaker, accurate claim
/// ("could not tell") instead of the reminder's assertion of drift, so it
/// cannot be wrong in the way that would spend the reminder's credibility.
/// Both branches also RECORD the blind probe, so neither is a path through this
/// function that leaves no trace.
fn durability_footer(
    position: BackupPosition,
    auto_backup: bool,
    backup: &Path,
    push: impl FnOnce() -> Result<()>,
) -> Option<String> {
    // trace:TASK-325 | ai:claude
    let report = |result: Result<()>, blind: bool| match result {
        Ok(()) if blind => Some(format!(
            "Could not tell whether the domain graph was backed up, so pushed it to {} anyway.",
            backup.display()
        )),
        Ok(()) => Some(format!(
            "Backed up the domain graph to {}.",
            backup.display()
        )),
        Err(error) => {
            // The cause goes where diagnostics go. The footer stays one line:
            // the user's next move is the same either way, and a dolt stack of
            // prose at the end of a session is not it.
            crate::diagnostics::record(&format!("auto_backup push failed: {error}"));
            Some(format!(
                "Auto-backup failed; the graph is still unbacked-up — run \
                 `quizdom db-backup` to push it to {}.",
                backup.display()
            ))
        }
    };

    match position {
        BackupPosition::UpToDate => None,
        // trace:TASK-325 | ai:claude — the blind probe never silently cancels
        // an opted-in push, and never passes without a breadcrumb.
        // trace:TASK-328 | ai:claude — …and never passes without telling the
        // user either: silence here reads exactly like "backed up".
        BackupPosition::Unknown if !auto_backup => {
            crate::diagnostics::record(
                "backup position unknown: could not determine whether the domain graph is \
                 backed up; auto_backup is off, so nothing was pushed and the session said \
                 it could not tell",
            );
            Some(blind_probe_notice(backup))
        }
        BackupPosition::Unknown => {
            crate::diagnostics::record(
                "backup position unknown: could not determine whether the domain graph is \
                 backed up; auto_backup is on, so pushing anyway rather than skipping a \
                 backup the user opted into",
            );
            report(push(), true)
        }
        BackupPosition::Ahead if !auto_backup => Some(backup_reminder(backup)),
        BackupPosition::Ahead => report(push(), false),
    }
}

/// The line a finished session prints about durability, or `None` when there is
/// nothing to say.
///
/// Silent unless BOTH halves hold: this process committed a graph write
/// ([`crate::dolt_store::graph_written_this_process`]) AND the working copy is
/// ahead of its backup. The write check is what keeps a read-only session
/// quiet even when the graph has been unbacked-up for a week — the reminder
/// belongs to the session that caused the drift, so it lands as feedback on
/// what you just did rather than as ambient nagging.
///
/// Under `cfg(test)` this is `None` without resolving anything (the TASK-266 /
/// TASK-280 pattern): it would otherwise send the in-crate session tests
/// through the real settings chain at the developer's actual graph. The
/// behaviour is covered by driving [`durability_footer`] and
/// [`backup_position_from_row`] directly, which is where it lives.
pub(crate) fn session_end_durability() -> Option<String> {
    if cfg!(test) || !crate::dolt_store::graph_written_this_process() {
        return None;
    }
    let path = crate::settings::resolve_dolt_path();
    let backup = crate::settings::resolve_dolt_backup_path();
    // trace:TASK-324 | ai:claude — ONE resolution, used by BOTH the probe and
    // the push below. Reading the tracking ref for a remote the push would not
    // use is how the reminder came to fire seconds after a successful
    // `db-backup --remote archive`.
    let remote = crate::settings::resolve_backup_remote();
    let runner = SystemDoltRunner::new("dolt".to_string());
    let position = backup_position(&runner, &path, &remote);
    durability_footer(
        position,
        crate::settings::resolve_auto_backup(),
        &backup,
        || {
            let config = DbBackupConfig {
                path: path.clone(),
                backup: backup.clone(),
                remote: remote.clone(),
                dolt_command: "dolt".to_string(),
                // NEVER force from an automatic path: `--force` retires whatever
                // lineage is in the backup directory, which is a decision an
                // operator makes deliberately, not one a session end makes for
                // them.
                force: false,
            };
            // `db_backup`'s progress lines are for someone who typed the
            // command; an auto-backup reports itself in one line above, so they
            // go to the diagnostic log instead of into the session's tail.
            let mut narration: Vec<u8> = Vec::new();
            let result = db_backup(&config, &runner, &mut narration);
            let narrated = String::from_utf8_lossy(&narration);
            if !narrated.trim().is_empty() {
                crate::diagnostics::record(&format!("auto_backup: {}", narrated.trim()));
            }
            result
        },
    )
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
            force: false,
        }
    }

    // trace:TASK-278 | ai:claude
    fn forcing_config(path: &Path, backup: &Path) -> DbBackupConfig {
        DbBackupConfig {
            force: true,
            ..config(path, backup)
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
            (0, "", ""), // add -A
            // trace:TASK-276 | ai:claude — dolt refuses the commit in wording
            // NO prose matcher would recognise; the `dolt_status` probe below
            // is what settles it, so a reworded dolt release cannot turn an
            // ordinary clean-tree backup into a hard failure.
            (1 << 8, "", "commit aborted: the working set is up to date"),
            (0, r#"{"rows":[{"pending":0}]}"#, ""), // dolt_status: nothing pending
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
                &format!("sql -r json -q {}", crate::db_init::PENDING_CHANGES_SQL),
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
                // trace:TASK-278 | ai:claude — the overwrite option is a flag
                // the tool executes, not an `mv` recipe the user retypes, and
                // the suggested command carries the paths this run used.
                let forced = message
                    .lines()
                    .find(|line| line.contains("--force"))
                    .expect("names the flag");
                assert!(
                    forced.contains(&format!("--path {}", repo.display()))
                        && forced.contains(&format!("--to {}", backup.display())),
                    "the hint is runnable as-is: {forced}"
                );
                assert!(
                    !message.contains("mv "),
                    "no hand-run recipe left in the message: {message}"
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

    // trace:TASK-278 | ai:claude
    // trace:TASK-283 | ai:claude
    /// The other half of BUG-277's promised choice: `--force` retires the
    /// lineage occupying the backup directory and pushes. It MOVES — the
    /// displaced copy is still there afterwards, under a suffix.
    #[test]
    fn force_retires_the_foreign_lineage_then_pushes() {
        let repo = temp_dir("forced");
        std::fs::create_dir_all(repo.join(".dolt")).unwrap();
        let backup = temp_dir("forced-dest");
        std::fs::create_dir_all(&backup).unwrap();
        // Something identifiable belonging to the foreign lineage.
        std::fs::write(backup.join("foreign-marker"), "the other graph").unwrap();
        let runner = RecordingDoltRunner::new(vec![
            (0, "", ""), // remote -v
            (0, "", ""), // remote add
            (0, "", ""), // add -A
            (0, "", ""), // commit
            (1 << 8, "", "unknown push error; no common ancestor"),
            (0, "", ""), // the retried push, into a now-unclaimed directory
        ]);
        let mut output = Vec::new();

        db_backup(&forcing_config(&repo, &backup), &runner, &mut output)
            .expect("--force should push over a retired lineage");

        let retired = PathBuf::from(format!("{}.foreign-lineage", backup.display()));
        assert_eq!(
            std::fs::read_to_string(retired.join("foreign-marker")).unwrap(),
            "the other graph",
            "the displaced lineage is moved, not deleted"
        );
        assert!(
            backup.exists() && !backup.join("foreign-marker").exists(),
            "the backup directory is re-made empty for this graph"
        );
        let pushes = runner
            .call_names()
            .into_iter()
            .filter(|call| call.starts_with("push"))
            .count();
        assert_eq!(pushes, 2, "refused once, then pushed after the retirement");
        let rendered = String::from_utf8(output).unwrap();
        assert!(
            rendered.contains("retired the foreign lineage")
                && rendered.contains("moved, not deleted"),
            "says what it did with the other copy: {rendered}"
        );

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&backup);
        let _ = std::fs::remove_dir_all(&retired);
    }

    // trace:TASK-278 | ai:claude
    /// No sequence of forced backups can destroy a lineage: a second `--force`
    /// against the same directory lands beside the first retirement, never on
    /// top of it.
    #[test]
    fn a_second_force_never_overwrites_the_first_retirement() {
        let repo = temp_dir("forced-twice");
        std::fs::create_dir_all(repo.join(".dolt")).unwrap();
        let backup = temp_dir("forced-twice-dest");
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::write(backup.join("second-lineage"), "newer").unwrap();
        let first = PathBuf::from(format!("{}.foreign-lineage", backup.display()));
        std::fs::create_dir_all(&first).unwrap();
        std::fs::write(first.join("first-lineage"), "older").unwrap();
        let runner = RecordingDoltRunner::new(vec![
            (0, "", ""),
            (0, "", ""),
            (0, "", ""),
            (0, "", ""),
            (1 << 8, "", "unknown push error; no common ancestor"),
            (0, "", ""),
        ]);

        db_backup(&forcing_config(&repo, &backup), &runner, &mut Vec::new())
            .expect("--force should succeed");

        let second = PathBuf::from(format!("{}.foreign-lineage.2", backup.display()));
        assert_eq!(
            std::fs::read_to_string(first.join("first-lineage")).unwrap(),
            "older",
            "the earlier retirement is untouched"
        );
        assert_eq!(
            std::fs::read_to_string(second.join("second-lineage")).unwrap(),
            "newer",
            "the new one lands beside it"
        );

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&backup);
        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }

    // trace:TASK-284 | ai:claude
    /// The marker scan reads stderr and stdout as separate streams. Joined
    /// edge-to-edge they would spell `no common ancestor` between them, and the
    /// push would be mis-diagnosed as a foreign lineage.
    #[test]
    fn a_marker_spanning_the_stream_seam_is_not_a_foreign_lineage() {
        let repo = temp_dir("seam");
        std::fs::create_dir_all(repo.join(".dolt")).unwrap();
        let backup = temp_dir("seam-dest");
        let runner = RecordingDoltRunner::new(vec![
            (0, "", ""),
            (0, "", ""),
            (0, "", ""),
            (0, "", ""),
            (
                1 << 8,
                "ancestor lookup timed out",
                "remote rejected: no common",
            ),
        ]);

        match db_backup(&config(&repo, &backup), &runner, &mut Vec::new()) {
            Err(QuizdomError::Dolt(message)) => assert!(
                !message.contains("UNRELATED"),
                "a marker formed across the seam is not a diagnosis: {message}"
            ),
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
            BACKUP_REMOTE_NAME.to_string(),
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
            BACKUP_REMOTE_NAME.to_string(),
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
            BACKUP_REMOTE_NAME.to_string(),
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
            BACKUP_REMOTE_NAME.to_string(),
        );
        assert!(matches!(result, Err(QuizdomError::Usage(_))));
    }

    // trace:TASK-283 | ai:claude
    /// BUG-277's actual vector, refused before a single dolt spawn: a scratch
    /// `--path` aimed at the DEFAULT backup directory. This is the run that
    /// happened — `cargo run -p quizdom -- db-backup` over a throwaway fixture
    /// — and until now only `cargo test` was guarded against it.
    #[test]
    fn an_unpinned_scratch_path_may_not_claim_the_default_backup() {
        let refused = DbBackupConfig::parse(
            ["db-backup", "--path", "/tmp/throwaway-fixture"].map(String::from),
            "db-backup",
            PathBuf::from("/home/dev/quizdom/data/dolt"),
            PathBuf::from("/home/dev/.local/share/quizdom/dolt-backup"),
            BACKUP_REMOTE_NAME.to_string(),
        );
        match refused {
            Err(QuizdomError::Usage(message)) => {
                assert!(
                    message.contains("/tmp/throwaway-fixture")
                        && message.contains("/home/dev/.local/share/quizdom/dolt-backup"),
                    "names both sides of the mismatch: {message}"
                );
                assert!(message.contains("--to"), "names the escape: {message}");
            }
            other => panic!("expected a Usage error, got {other:?}"),
        }
    }

    // trace:TASK-283 | ai:claude
    /// ...and the shapes that are NOT the vector stay untouched: a bare run, a
    /// `--path` that names the resolved default anyway, and a scratch run that
    /// pinned its destination like it was supposed to.
    #[test]
    fn pinned_and_default_backups_are_left_alone() {
        let default_path = PathBuf::from("/home/dev/quizdom/data/dolt");
        let default_backup = PathBuf::from("/home/dev/.local/share/quizdom/dolt-backup");
        let accepted = |args: &[&str]| {
            DbBackupConfig::parse(
                args.iter().map(|arg| arg.to_string()),
                "db-backup",
                default_path.clone(),
                default_backup.clone(),
                BACKUP_REMOTE_NAME.to_string(),
            )
            .unwrap_or_else(|error| panic!("{args:?} should parse, got {error:?}"))
        };

        assert_eq!(accepted(&["db-backup"]).path, default_path, "bare run");
        assert_eq!(
            accepted(&["db-backup", "--path", "/home/dev/quizdom/data/dolt"]).backup,
            default_backup,
            "--path naming the resolved default is not a mismatch"
        );
        assert_eq!(
            accepted(&[
                "db-backup",
                "--path",
                "/tmp/scratch-graph",
                "--to",
                "/tmp/scratch-backup",
            ])
            .backup,
            PathBuf::from("/tmp/scratch-backup"),
            "a scratch run that pinned its destination"
        );
        // `db-restore` READS the backup, so the same pairing is harmless there.
        assert!(DbBackupConfig::parse(
            ["db-restore", "--path", "/tmp/somewhere-else"].map(String::from),
            "db-restore",
            default_path,
            default_backup,
            BACKUP_REMOTE_NAME.to_string(),
        )
        .is_ok());
    }

    // trace:TASK-278 | ai:claude
    #[test]
    fn force_is_a_backup_flag_only() {
        assert!(
            DbBackupConfig::parse(
                ["db-backup", "--force"].map(String::from),
                "db-backup",
                PathBuf::from("/tmp/x"),
                PathBuf::from("/tmp/b"),
                BACKUP_REMOTE_NAME.to_string(),
            )
            .unwrap()
            .force
        );
        // Nothing to force on the read side — restore refuses an occupied
        // target for reasons `--force` must not be able to wave away.
        assert!(matches!(
            DbBackupConfig::parse(
                ["db-restore", "--force"].map(String::from),
                "db-restore",
                PathBuf::from("/tmp/x"),
                PathBuf::from("/tmp/b"),
                BACKUP_REMOTE_NAME.to_string(),
            ),
            Err(QuizdomError::Usage(_))
        ));
    }

    // trace:TASK-275 | ai:claude
    /// The durability doc recommends a synced folder or a removable disk for
    /// the backup — "Google Drive", "My Passport". An unquoted path makes the
    /// printed recovery command silently wrong at the moment it is needed.
    #[test]
    fn printed_paths_survive_a_space() {
        assert_eq!(shell_quote(Path::new("/tmp/plain-path")), "/tmp/plain-path");
        let quoted = shell_quote(Path::new("/tmp/My Drive/quizdom bk"));
        assert_eq!(quoted, "'/tmp/My Drive/quizdom bk'");
        assert_eq!(
            unquote(&quoted),
            "/tmp/My Drive/quizdom bk",
            "round-trips back through a shell's quote removal"
        );
        // An embedded apostrophe closes, escapes, and reopens.
        let apostrophe = shell_quote(Path::new("/tmp/joe's backup"));
        assert_eq!(apostrophe, r"'/tmp/joe'\''s backup'");
        assert_eq!(unquote(&apostrophe), "/tmp/joe's backup");
    }

    // trace:TASK-275 | ai:claude
    /// A shell's quote removal, enough of it to prove the round trip: strip
    /// single-quote pairs, honour `\'` outside them.
    fn unquote(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        let mut inside = false;
        while let Some(character) = chars.next() {
            match character {
                '\'' => inside = !inside,
                '\\' if !inside => out.push(chars.next().expect("escaped character")),
                other => out.push(other),
            }
        }
        assert!(!inside, "unbalanced quotes in {text}");
        out
    }

    // trace:TASK-275 | ai:claude
    #[test]
    fn the_hints_quote_the_paths_they_interpolate() {
        let repo = temp_dir("spaced graph");
        std::fs::create_dir_all(repo.join(".dolt")).unwrap();
        let backup = temp_dir("spaced backup");
        let runner = RecordingDoltRunner::new(vec![(0, "", "")]);
        let mut output = Vec::new();

        db_backup(&config(&repo, &backup), &runner, &mut output).expect("backup should succeed");

        let rendered = String::from_utf8(output).unwrap();
        let hint = rendered
            .lines()
            .find(|line| line.contains("Restore with:"))
            .expect("prints the recovery command");
        assert!(
            hint.contains(&shell_quote(&repo)) && hint.contains(&shell_quote(&backup)),
            "both paths quoted: {hint}"
        );

        let mut restored = Vec::new();
        let empty = temp_dir("spaced restored");
        db_restore(&config(&empty, &backup), &runner, &mut restored)
            .expect("restore should succeed");
        let rendered = String::from_utf8(restored).unwrap();
        assert!(
            rendered.contains(&format!("cd {}", shell_quote(&empty))),
            "the verify hint is runnable: {rendered}"
        );

        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&backup);
        let _ = std::fs::remove_dir_all(&empty);
    }

    // trace:TASK-282 | ai:claude
    /// The tripwire is `#[cfg(test)]`, so it exists only while the lib is
    /// compiled AS a test target. An integration test under `tests/` links the
    /// lib WITHOUT `cfg(test)` — the guard compiles to its no-op and any
    /// `db-backup` call from there resolves the real settings chain unguarded.
    /// That is the shape of test most likely to drive the CLI end-to-end
    /// without pinning `--to`, i.e. the BUG-277 vector exactly.
    ///
    /// So the invariant the guard's own doc comment asserts — every test in
    /// this crate is an in-crate unit test — is checked rather than assumed.
    /// If this fails, the fix is not to delete the assert: it is to make the
    /// tripwire survive without `cfg(test)` before adding the directory.
    #[test]
    fn no_out_of_crate_test_target_can_silently_disable_the_tripwire() {
        let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        for kind in ["tests", "benches", "examples"] {
            assert!(
                !crate_root.join(kind).exists(),
                "crates/quizdom/{kind}/ links the lib WITHOUT cfg(test), so the \
                 BUG-277 tripwire compiles to its no-op there and those targets \
                 can reach the real domain graph and its backup. Make \
                 guard_test_path work outside cfg(test) before adding {kind}/."
            );
        }
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
        // trace:STORY-291 | ai:claude — deliberately NOT committed here. Every
        // quizdom writer commits its own writes now, so what the snapshot step
        // still has to rescue is a hand-run `dolt sql` like this one, and that
        // is what this fixture stands for.

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

    // trace:STORY-299 | ai:claude
    /// The reminder's whole correctness rests on two dolt system tables —
    /// `dolt_remote_branches` (the tracking ref a push updates) and
    /// `dolt_status` — and the mock tests above pin only how quizdom READS the
    /// rows. This one pins that the rows say what quizdom thinks they say, so a
    /// dolt release that renames a column or a ref cannot silently turn the
    /// reminder into permanent silence with every unit test green.
    #[test]
    #[ignore = "requires the dolt binary on PATH"]
    fn real_dolt_backup_position_tracks_pushes_commits_and_a_dirty_tree() {
        let repo = temp_dir("position");
        let backup = temp_dir("position-dest");
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

        // Never pushed anywhere: the case the reminder exists for.
        assert_eq!(
            backup_position(&runner, &repo, BACKUP_REMOTE_NAME),
            BackupPosition::Ahead,
            "a graph with no backup at all is ahead of one"
        );

        let config = config(&repo, &backup);
        db_backup(&config, &runner, &mut Vec::new()).expect("backup should succeed");
        assert_eq!(
            backup_position(&runner, &repo, BACKUP_REMOTE_NAME),
            BackupPosition::UpToDate,
            "straight after a push there is nothing to remind anyone about"
        );

        // A hand-run `dolt sql` — the working-set case `snapshot_working_set`
        // exists to rescue. The branch hash has NOT moved, so only dolt_status
        // can see this.
        run_dolt(
            &runner,
            &repo,
            &[
                "sql",
                "-q",
                "INSERT INTO nodes (id, kind, title, body, tags, weight) VALUES \
                 ('Q-1', 'question', 'seed question', 'body', '', 70);",
            ],
        )
        .expect("fixture should load");
        assert_eq!(
            backup_position(&runner, &repo, BACKUP_REMOTE_NAME),
            BackupPosition::Ahead,
            "an uncommitted change is unbacked-up even at an unchanged hash"
        );

        // And a committed write, which is what every quizdom writer produces.
        db_backup(&config, &runner, &mut Vec::new()).expect("re-backup should succeed");
        assert_eq!(
            backup_position(&runner, &repo, BACKUP_REMOTE_NAME),
            BackupPosition::UpToDate
        );
        crate::db_init::commit_all(&runner, &repo, "a later write").ok();
        run_dolt(
            &runner,
            &repo,
            &[
                "sql",
                "-q",
                "INSERT INTO nodes (id, kind, title, body, tags, weight) VALUES \
                 ('Q-2', 'question', 'another', 'body', '', 70);",
            ],
        )
        .expect("fixture should load");
        crate::db_init::commit_all(&runner, &repo, "a later write")
            .expect("the write should commit");
        assert_eq!(
            backup_position(&runner, &repo, BACKUP_REMOTE_NAME),
            BackupPosition::Ahead,
            "a commit since the last push is what a writing session leaves behind"
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

        // trace:TASK-278 | ai:claude — ...until `--force`, which retires the
        // throwaway lineage and lets the genuine graph through. Against the
        // real engine, because the whole point is that the documented way out
        // is now executable rather than a recipe.
        let mut rendered = Vec::new();
        db_backup(&forcing_config(&genuine, &backup), &runner, &mut rendered)
            .expect("--force should push over the retired lineage");
        let retired = PathBuf::from(format!("{}.foreign-lineage", backup.display()));
        assert!(
            retired.join("manifest").exists() || retired.exists(),
            "the throwaway lineage is still on disk at {}",
            retired.display()
        );
        assert!(String::from_utf8(rendered)
            .unwrap()
            .contains("retired the foreign lineage"));

        // And the graph really is in there now: restore it and count the rows.
        let restored = temp_dir("lineage-restored");
        db_restore(&config(&restored, &backup), &runner, &mut Vec::new())
            .expect("the forced backup should restore");
        assert!(restored.join(".dolt").exists(), "a real repo came back");

        let _ = std::fs::remove_dir_all(&claimed);
        let _ = std::fs::remove_dir_all(&genuine);
        let _ = std::fs::remove_dir_all(&backup);
        let _ = std::fs::remove_dir_all(&retired);
        let _ = std::fs::remove_dir_all(&restored);
    }

    // -----------------------------------------------------------------------
    // trace:STORY-299 | ai:claude — the durability ergonomics.
    // -----------------------------------------------------------------------

    fn position_row(json: &str) -> BackupPosition {
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        backup_position_from_row(value.as_object())
    }

    #[test]
    fn a_repo_matching_its_backup_with_a_clean_tree_is_up_to_date() {
        assert_eq!(
            position_row(r#"{"local_hash":"abc","backup_hash":"abc","pending":0}"#),
            BackupPosition::UpToDate
        );
    }

    #[test]
    fn a_commit_since_the_last_push_is_ahead() {
        assert_eq!(
            position_row(r#"{"local_hash":"def","backup_hash":"abc","pending":0}"#),
            BackupPosition::Ahead
        );
    }

    // The graph is a database a user can write to directly, and `db-backup`
    // snapshots the working set before pushing — so an uncommitted change is
    // just as unbacked-up as a commit, even though the branch hash has not
    // moved.
    #[test]
    fn an_uncommitted_working_set_is_ahead_even_at_the_same_hash() {
        assert_eq!(
            position_row(r#"{"local_hash":"abc","backup_hash":"abc","pending":2}"#),
            BackupPosition::Ahead
        );
        // dolt has rendered COUNT(*) as a string in some releases; the
        // dirty-tree signal must not depend on which.
        assert_eq!(
            position_row(r#"{"local_hash":"abc","backup_hash":"abc","pending":"2"}"#),
            BackupPosition::Ahead
        );
    }

    // "You have never backed this graph up" is the single most important case
    // for the reminder to catch, and it looks like a missing tracking ref.
    #[test]
    fn a_graph_never_pushed_anywhere_is_ahead_not_unknown() {
        assert_eq!(
            position_row(r#"{"local_hash":"abc","pending":0}"#),
            BackupPosition::Ahead
        );
    }

    // A probe that could not answer is not evidence of drift, so it is read as
    // `Unknown` rather than folded into `Ahead` — what the user is told is
    // "could not tell", never "you have changes to push".
    #[test]
    fn an_unanswerable_probe_is_unknown_rather_than_ahead() {
        assert_eq!(position_row("{}"), BackupPosition::Unknown);
        assert_eq!(
            backup_position_from_row(None),
            BackupPosition::Unknown,
            "no row at all — an older dolt without the system tables"
        );
    }

    // trace:TASK-325 | ai:claude
    // trace:TASK-328 | ai:claude
    /// The reminder half of the `Unknown` rule. A blind probe is not evidence
    /// of drift, so this is NOT the reminder — but it is not silence either:
    /// silence is what a backed-up graph looks like, so an auto-backup-OFF
    /// session (the default configuration) learnt nothing at all from a probe
    /// that had failed. It now says what it actually knows, and records why.
    #[test]
    fn a_blind_probe_says_it_could_not_tell_and_records_why() {
        crate::diagnostics::clear_captured();

        let footer = durability_footer(
            BackupPosition::Unknown,
            false,
            Path::new("/backups/qz"),
            || panic!("auto_backup is off; nothing may push"),
        )
        .expect("a probe that could not answer must not read as `backed up`");

        assert!(
            footer.contains("Could not tell"),
            "the claim is ignorance, not drift: {footer}"
        );
        assert!(
            !footer.contains("has changes not in its backup"),
            "…so it must not borrow the reminder's assertion: {footer}"
        );
        assert!(footer.contains("quizdom db-backup"), "{footer}");
        assert!(footer.contains("/backups/qz"), "{footer}");
        assert!(
            footer.contains("quizdom logs"),
            "the reason the probe failed is in the log, and that is the \
             actionable part: {footer}"
        );
        assert_eq!(footer.lines().count(), 1, "one line, not a lecture");

        let recorded = crate::diagnostics::captured();
        assert_eq!(recorded.len(), 1, "{recorded:?}");
        assert!(
            recorded[0].contains("backup position unknown"),
            "{recorded:?}"
        );
        assert!(
            recorded[0].contains("auto_backup is off"),
            "the log says which branch was taken: {recorded:?}"
        );
    }

    // trace:TASK-328 | ai:claude
    /// Both probe outcomes against both settings, in one table, because the
    /// bug TASK-328 was filed about was a HOLE in this matrix: five of the six
    /// cells said something and `Unknown` + auto-backup OFF said nothing, which
    /// is indistinguishable from `UpToDate` — the one cell that is silent on
    /// purpose.
    #[test]
    fn every_probe_outcome_is_answered_under_both_auto_backup_settings() {
        let cases = [
            (BackupPosition::UpToDate, false, None),
            (BackupPosition::UpToDate, true, None),
            (
                BackupPosition::Ahead,
                false,
                Some("has changes not in its backup"),
            ),
            (
                BackupPosition::Ahead,
                true,
                Some("Backed up the domain graph"),
            ),
            (
                BackupPosition::Unknown,
                false,
                Some("Could not tell whether"),
            ),
            (
                BackupPosition::Unknown,
                true,
                Some("Could not tell whether"),
            ),
        ];

        for (position, auto_backup, expected) in cases {
            crate::diagnostics::clear_captured();
            let pushed = RefCell::new(false);
            let footer = durability_footer(position, auto_backup, Path::new("/backups/qz"), || {
                *pushed.borrow_mut() = true;
                Ok(())
            });

            match expected {
                None => assert_eq!(
                    footer, None,
                    "an up-to-date graph is the ONE silent cell: {position:?} / {auto_backup}"
                ),
                Some(fragment) => {
                    let footer = footer.unwrap_or_else(|| {
                        panic!("{position:?} / auto_backup={auto_backup} told the user nothing")
                    });
                    assert!(footer.contains(fragment), "{position:?}: {footer}");
                }
            }
            // The push runs exactly where the user opted into it and the graph
            // might not be safe — never on an up-to-date graph, never without
            // `auto_backup`.
            assert_eq!(
                *pushed.borrow(),
                auto_backup && position != BackupPosition::UpToDate,
                "{position:?} / auto_backup={auto_backup}"
            );
        }
    }

    // trace:TASK-325 | ai:claude
    /// The decided half. `Unknown` used to be matched alongside `UpToDate`
    /// BEFORE `auto_backup` was consulted, so a probe that could not answer
    /// silently cancelled a push the user explicitly opted into — no push, no
    /// reminder, no log line. An opted-in user would rather have a redundant
    /// push than a skipped one, so the push runs.
    #[test]
    fn a_blind_probe_never_cancels_an_opted_in_push() {
        crate::diagnostics::clear_captured();
        let pushed = RefCell::new(false);

        let footer = durability_footer(
            BackupPosition::Unknown,
            true,
            Path::new("/backups/qz"),
            || {
                *pushed.borrow_mut() = true;
                Ok(())
            },
        )
        .expect("an opted-in push reports itself");

        assert!(*pushed.borrow(), "the push ran despite the blind probe");
        assert!(footer.contains("/backups/qz"), "{footer}");
        assert!(
            footer.contains("Could not tell"),
            "the line is honest about WHY it pushed: {footer}"
        );
        assert_eq!(footer.lines().count(), 1, "one line, not a lecture");
        assert!(
            crate::diagnostics::captured()[0].contains("auto_backup is on"),
            "{:?}",
            crate::diagnostics::captured()
        );
    }

    // trace:TASK-325 | ai:claude — and a blind push that FAILS degrades exactly
    // as the `Ahead` one does: the reminder, the command, the cause in the log.
    // A backup that did not happen must never be the thing that ends a session
    // badly (the STORY-299 rule).
    #[test]
    fn a_failed_blind_push_degrades_to_the_reminder() {
        crate::diagnostics::clear_captured();

        let footer = durability_footer(
            BackupPosition::Unknown,
            true,
            Path::new("/backups/qz"),
            || Err(QuizdomError::Dolt("remote unreachable".to_string())),
        )
        .expect("a failed push still tells the user the graph is unbacked-up");

        assert!(footer.contains("quizdom db-backup"), "{footer}");
        assert!(footer.contains("/backups/qz"), "{footer}");
        let recorded = crate::diagnostics::captured();
        assert!(
            recorded
                .iter()
                .any(|entry| entry.contains("remote unreachable")),
            "the cause goes to the log, not the session tail: {recorded:?}"
        );
    }

    #[test]
    fn the_probe_reads_the_tracking_ref_for_the_configured_remote() {
        let sql = backup_position_sql("backup");

        assert!(sql.contains("'remotes/backup/main'"), "{sql}");
        assert!(sql.contains("dolt_remote_branches"), "{sql}");
        assert!(sql.contains("dolt_status"), "{sql}");
        // A remote name is user-supplied (`--remote`), so it is quoted, not
        // interpolated raw.
        assert!(
            backup_position_sql("it's").contains("'remotes/it''s/main'"),
            "{}",
            backup_position_sql("it's")
        );
    }

    // trace:TASK-324 | ai:claude
    /// The probe and the push have to name the SAME remote. When the probe
    /// hardcoded `backup`, an operator working under another name never
    /// populated `remotes/backup/main`; the probe read that missing tracking
    /// ref as "never backed up" (the right call for the default name) and the
    /// reminder then fired after every writing session — including seconds
    /// after a successful `db-backup --remote archive`.
    #[test]
    fn a_non_default_remote_is_probed_under_its_own_name() {
        let repo = temp_dir("position-remote");
        std::fs::create_dir_all(repo.join(".dolt")).unwrap();
        // The tracking ref for `archive` matches, so this graph IS backed up.
        let runner = RecordingDoltRunner::new(vec![(
            0,
            r#"{"rows":[{"local_hash":"abc","backup_hash":"abc","pending":0}]}"#,
            "",
        )]);

        assert_eq!(
            backup_position(&runner, &repo, "archive"),
            BackupPosition::UpToDate
        );
        let calls = runner.call_names();
        assert!(
            calls[0].contains("remotes/archive/main"),
            "the probe reads the CONFIGURED remote's tracking ref: {calls:?}"
        );
        assert!(
            !calls[0].contains(&format!("remotes/{BACKUP_REMOTE_NAME}/main")),
            "and not the default's: {calls:?}"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    // trace:TASK-324 | ai:claude — `--remote` still sits on top of the resolved
    // default, exactly as `--path` sits on top of the resolved graph path.
    #[test]
    fn the_remote_name_defaults_from_the_settings_chain_and_a_flag_overrides_it() {
        let resolved = DbBackupConfig::parse(
            ["db-backup".to_string()],
            "db-backup",
            PathBuf::from("/from/env"),
            PathBuf::from("/from/env-backup"),
            "archive".to_string(),
        )
        .unwrap();
        assert_eq!(
            resolved.remote, "archive",
            "the chain's answer is the default"
        );

        let flagged = DbBackupConfig::parse(
            ["db-backup", "--remote", "offsite"].map(String::from),
            "db-backup",
            PathBuf::from("/from/env"),
            PathBuf::from("/from/env-backup"),
            "archive".to_string(),
        )
        .unwrap();
        assert_eq!(
            flagged.remote, "offsite",
            "--flag > env > settings > default"
        );
    }

    // The probe never touches the backup directory: the backup may be an
    // unmounted removable disk, and a session end must not block on it.
    #[test]
    fn the_probe_runs_one_local_read_only_query() {
        let repo = temp_dir("position-probe");
        std::fs::create_dir_all(repo.join(".dolt")).unwrap();
        let runner = RecordingDoltRunner::new(vec![(
            0,
            r#"{"rows":[{"local_hash":"abc","backup_hash":"abc","pending":0}]}"#,
            "",
        )]);

        assert_eq!(
            backup_position(&runner, &repo, BACKUP_REMOTE_NAME),
            BackupPosition::UpToDate
        );
        let calls = runner.call_names();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].starts_with("sql -r json -q"), "{calls:?}");

        let _ = std::fs::remove_dir_all(&repo);
    }

    // trace:TASK-328 | ai:claude — named for what it asserts: the POSITION is
    // unknown. What the session then says about that is `durability_footer`'s
    // call, and since TASK-328 it is no longer silence.
    #[test]
    fn a_dolt_that_cannot_answer_leaves_the_position_unknown() {
        let repo = temp_dir("position-no-dolt");
        std::fs::create_dir_all(repo.join(".dolt")).unwrap();

        // A failing query (an older dolt without `dolt_remote_branches`).
        let runner = RecordingDoltRunner::new(vec![(1 << 8, "", "table not found")]);
        assert_eq!(
            backup_position(&runner, &repo, BACKUP_REMOTE_NAME),
            BackupPosition::Unknown
        );

        // Output that is not JSON at all.
        let runner = RecordingDoltRunner::new(vec![(0, "not json", "")]);
        assert_eq!(
            backup_position(&runner, &repo, BACKUP_REMOTE_NAME),
            BackupPosition::Unknown
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    // trace:TASK-328 | ai:claude
    #[test]
    fn no_repo_leaves_the_position_unknown_without_spawning_dolt() {
        let runner = RecordingDoltRunner::new(vec![]);
        assert_eq!(
            backup_position(
                &runner,
                Path::new("/nowhere/quizdom-has-no-graph-here"),
                BACKUP_REMOTE_NAME
            ),
            BackupPosition::Unknown
        );
        assert!(runner.call_names().is_empty(), "and nothing was spawned");
    }

    // trace:STORY-299 | ai:claude — the DEFAULT path: explicit backups, plus a
    // line that names the exact command so "explicit" stops meaning "forgotten".
    #[test]
    fn ahead_with_auto_backup_off_names_the_exact_command_and_destination() {
        let footer = durability_footer(
            BackupPosition::Ahead,
            false,
            Path::new("/backups/qz"),
            || panic!("auto_backup is off; nothing may push"),
        )
        .expect("a session that moved the graph past its backup says so");

        assert!(footer.contains("quizdom db-backup"), "{footer}");
        assert!(footer.contains("/backups/qz"), "{footer}");
        assert_eq!(footer.lines().count(), 1, "one line, not a lecture");
    }

    #[test]
    fn an_already_backed_up_graph_says_nothing() {
        assert_eq!(
            durability_footer(
                BackupPosition::UpToDate,
                false,
                Path::new("/backups/qz"),
                || { panic!("nothing to push") }
            ),
            None
        );
        // …and auto-backup does not push what is already there either.
        assert_eq!(
            durability_footer(
                BackupPosition::UpToDate,
                true,
                Path::new("/backups/qz"),
                || { panic!("nothing to push") }
            ),
            None
        );
    }

    #[test]
    fn auto_backup_pushes_and_reports_it_in_one_line() {
        let pushed = RefCell::new(false);

        let footer = durability_footer(
            BackupPosition::Ahead,
            true,
            Path::new("/backups/qz"),
            || {
                *pushed.borrow_mut() = true;
                Ok(())
            },
        )
        .expect("an auto-backup reports what it did");

        assert!(*pushed.borrow(), "the push actually ran");
        assert!(footer.contains("Backed up"), "{footer}");
        assert!(footer.contains("/backups/qz"), "{footer}");
        assert_eq!(footer.lines().count(), 1, "{footer}");
    }

    // The STORY-299 rule: a backup that did not happen must never be the thing
    // that ends a session badly. It degrades to the reminder — the user who
    // opted in still learns the graph is unbacked-up, and still gets the command.
    #[test]
    fn a_failed_auto_backup_degrades_to_the_reminder_rather_than_erroring() {
        let footer = durability_footer(
            BackupPosition::Ahead,
            true,
            Path::new("/backups/qz"),
            || Err(QuizdomError::Dolt("the backup disk is not mounted".into())),
        )
        .expect("a failed auto-backup still has something to say");

        assert!(footer.contains("Auto-backup failed"), "{footer}");
        assert!(footer.contains("quizdom db-backup"), "{footer}");
        assert!(footer.contains("/backups/qz"), "{footer}");
        // The cause is not lost — it goes where diagnostics go, not to the
        // terminal the TUI owns.
        assert!(
            crate::diagnostics::captured()
                .iter()
                .any(|entry| entry.contains("the backup disk is not mounted")),
            "{:?}",
            crate::diagnostics::captured()
        );
    }
}
