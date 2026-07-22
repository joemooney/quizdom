// trace:STORY-205 | ai:claude
//! `quizdom db-init` — bootstrap the Dolt repository that will hold the
//! domain graph (EPIC-202 / ADR-201).
//!
//! Creates the repo directory if needed, runs `dolt init` unless the
//! directory is already a Dolt repo, applies the checked-in DDL
//! (`db/schema.sql`, embedded at compile time), and commits it. The DDL is
//! idempotent (`CREATE TABLE IF NOT EXISTS` only), so re-running `db-init` is
//! safe — the second run changes nothing and therefore commits nothing.
//!
//! This slice bootstraps the empty database only; the exporter (STORY-206)
//! and the Dolt-backed `DomainStore` (STORY-207) build on it.
//!
//! ## Shared dolt plumbing
//!
//! This module also hosts the pieces every dolt caller needs, because it
//! already owns the [`DoltRunner`] seam they all spawn through: the commit tail
//! ([`commit_tables`] / [`commit_working_set`]), the structured clean-tree probe
//! behind it ([`working_set_is_clean`], TASK-276), the foreign-change pre-flight
//! ([`refuse_on_foreign_changes`], TASK-297), the control-character scrub every
//! error surface applies ([`clean_dolt_message`], TASK-279), and the
//! `#[cfg(test)]` real-path tripwire ([`guard_test_path`], TASK-280). Keeping
//! one copy of each is the point — four hand-copied variants is how they drift.

use crate::error::{QuizdomError, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Output;

/// The checked-in schema DDL applied by `db-init`.
pub const DOLT_SCHEMA_SQL: &str = include_str!("../../../db/schema.sql");

/// Default location of the domain-graph Dolt repo, relative to the project
/// root (sibling of the per-user session logs under `data/`).
pub const DEFAULT_DOLT_DB_PATH: &str = "data/dolt";

/// Runs the `dolt` CLI in a working directory. Abstracted so the bootstrap
/// flow can be unit-tested without a dolt binary on PATH. Public because the
/// Dolt-backed [`crate::DoltDomainStore`] (STORY-207) shares this seam.
pub trait DoltRunner {
    fn run(&self, cwd: &Path, args: &[String]) -> Result<Output>;
}

/// The real runner: spawns the `dolt` binary. `db_init.rs` is allowlisted in
/// the BUG-200 guard test because this spawns dolt, not aida — the pinned
/// aida output format does not apply.
pub struct SystemDoltRunner {
    command: String,
}

impl SystemDoltRunner {
    pub(crate) fn new(command: String) -> Self {
        Self { command }
    }
}

impl DoltRunner for SystemDoltRunner {
    fn run(&self, cwd: &Path, args: &[String]) -> Result<Output> {
        std::process::Command::new(&self.command)
            .current_dir(cwd)
            .args(args)
            .output()
            // trace:TASK-304 | ai:claude
            .map_err(|error| QuizdomError::Dolt(spawn_failure(&self.command, cwd, &error)))
    }
}

// trace:TASK-304 | ai:claude
/// Why a `dolt` spawn failed, named after whichever thing was actually missing.
///
/// `Command::output` reports one `NotFound` for two unrelated causes: no `dolt`
/// on `PATH`, and a `current_dir` that does not exist. The message this replaces
/// asserted the first unconditionally, so a `dolt_path` pointing at a directory
/// nobody had created yet sent the user to audit a dolt installation that was
/// fine. A confident wrong diagnosis costs more than no diagnosis: it decides
/// where someone looks next.
fn spawn_failure(command: &str, cwd: &Path, error: &std::io::Error) -> String {
    if !cwd.exists() {
        return format!(
            "cannot run `{command}` in {cwd}: no such directory \
             (create the repo with `quizdom db-init --path {cwd}`)",
            cwd = cwd.display()
        );
    }
    if !cwd.is_dir() {
        return format!(
            "cannot run `{command}` in {}: that path is not a directory",
            cwd.display()
        );
    }
    format!("failed to spawn `{command}`: {error}; is dolt installed and on PATH?")
}

struct DbInitConfig {
    path: PathBuf,
    dolt_command: String,
}

impl DbInitConfig {
    // trace:TASK-228 | ai:claude
    /// Parse the argv tail over a `default_path` the caller resolved through
    /// [`crate::settings::resolve_dolt_path`] (env > settings > compiled
    /// default) — so `--path` stays the top-priority override while an unflagged
    /// run targets the SAME repo the runtime store will later read. Taking the
    /// default as a parameter keeps this pure: the tests pin argument handling
    /// without the ambient environment leaking in.
    fn parse(args: impl IntoIterator<Item = String>, default_path: PathBuf) -> Result<Self> {
        let mut path = default_path;
        let mut dolt_command = "dolt".to_string();
        let mut args = args.into_iter().peekable();

        if matches!(args.peek().map(String::as_str), Some("db-init")) {
            args.next();
        }

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--path" => path = PathBuf::from(next_arg(&mut args, "--path")?),
                "--dolt" => dolt_command = next_arg(&mut args, "--dolt")?,
                "--help" | "-h" => return Err(QuizdomError::Usage(usage())),
                other => {
                    return Err(QuizdomError::Usage(format!(
                        "unknown argument: {other}\n{}",
                        usage()
                    )))
                }
            }
        }

        Ok(Self { path, dolt_command })
    }
}

fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| QuizdomError::Usage(format!("{name} requires a value")))
}

fn usage() -> String {
    format!(
        "usage: quizdom db-init [--path <dir>] [--dolt dolt]\n\
         (--path defaults to $QUIZDOM_DOLT_PATH, else dolt_path in settings.toml, \
         else {DEFAULT_DOLT_DB_PATH})"
    )
}

/// Entry point for `quizdom db-init`.
pub fn run_db_init(args: impl IntoIterator<Item = String>, output: &mut impl Write) -> Result<()> {
    let config = DbInitConfig::parse(args, crate::settings::resolve_dolt_path())?;
    let runner = SystemDoltRunner {
        command: config.dolt_command.clone(),
    };
    db_init(&config.path, &runner, output)
}

// trace:STORY-291 | ai:claude — the DDL is a write like any other, so it
// commits.
/// The commit message `db-init` stamps on the schema it applies.
const SCHEMA_COMMIT_MESSAGE: &str = "quizdom db-init: apply domain-graph schema";

/// The bootstrap flow: ensure the directory exists, `dolt init` it unless a
/// `.dolt/` repo is already present, apply the idempotent schema DDL, and
/// commit it.
fn db_init(path: &Path, runner: &dyn DoltRunner, output: &mut impl Write) -> Result<()> {
    // trace:TASK-280 | ai:claude — `db-init` resolves the same settings chain
    // `db-backup` does, so it needs the same tripwire.
    guard_test_path("--path", path);
    // trace:TASK-301 | ai:claude — `create_dir_all`, not `create_dir`: a
    // configured `dolt_path` naming a nested directory (`~/graphs/quizdom/dolt`)
    // must bootstrap in one command, not fail on the parent nobody made. And
    // when it does fail, it names the path — a bare `No such file or directory
    // (os error 2)` is what cost the most time diagnosing this, because the one
    // fact the user needs is which path the process could not create.
    std::fs::create_dir_all(path).map_err(|error| {
        QuizdomError::Dolt(format!(
            "cannot create the domain-graph directory {}: {error}",
            path.display()
        ))
    })?;

    let fresh = !path.join(".dolt").exists();
    if fresh {
        run_dolt(runner, path, &["init"])?;
        writeln!(output, "Initialised Dolt repo at {}.", path.display())?;
    } else {
        writeln!(
            output,
            "Dolt repo already initialised at {} — skipping `dolt init`.",
            path.display()
        )?;
        // trace:TASK-297 | ai:claude — only meaningful on a re-run: a repo
        // `dolt init` just created has no working set to be foreign to.
        refuse_on_foreign_changes(runner, path, "quizdom db-init")?;
    }

    run_dolt(runner, path, &["sql", "-q", DOLT_SCHEMA_SQL])?;
    writeln!(
        output,
        "Applied domain-graph schema (nodes, edges) — DDL is idempotent."
    )?;
    // trace:STORY-291 | ai:claude
    if commit_tables(runner, path, QUIZDOM_TABLES, SCHEMA_COMMIT_MESSAGE)? {
        writeln!(output, "Committed the schema to Dolt history.")?;
    } else {
        writeln!(output, "Schema already committed — nothing new to record.")?;
    }
    Ok(())
}

// trace:STORY-291 | ai:claude
/// Whether a failed `dolt commit` is the benign "there was nothing to commit"
/// case rather than a real failure. An idempotent write (re-running `db-init`,
/// re-running `db-migrate`, an [`crate::store::DomainStore::ensure_edge`] that
/// hits an existing row) leaves the working set clean, and dolt exits non-zero
/// saying so — which is success for every caller here. Matched loosely: dolt
/// says "no changes added to commit" in some paths and "nothing to commit" in
/// others, and neither wording is a stable API.
///
/// **This is the fallback, not the answer** (TASK-276). [`is_clean_tree_refusal`]
/// asks the `dolt_status` system table first and only reaches for this when the
/// probe itself cannot run.
pub(crate) fn is_nothing_to_commit(reported: &str) -> bool {
    let text = reported.to_ascii_lowercase();
    text.contains("nothing to commit") || text.contains("no changes added")
}

// trace:TASK-276 | ai:claude
// trace:TASK-297 | ai:claude — `scope` narrows the question to the tables the
// caller actually staged, so a foreign table sitting in the working set cannot
// make an ordinary clean-tree refusal look like a failure.
/// The SQL behind [`working_set_is_clean`]. `dolt_status` is Dolt's system
/// table of pending changes (staged and unstaged both); an empty one is a clean
/// tree, by definition rather than by wording. An empty `scope` asks about the
/// whole working set.
pub(crate) fn pending_changes_sql(scope: &[&str]) -> String {
    match table_name_filter(scope) {
        Some(filter) => format!("SELECT COUNT(*) AS pending FROM dolt_status WHERE {filter}"),
        None => "SELECT COUNT(*) AS pending FROM dolt_status".to_string(),
    }
}

// trace:TASK-297 | ai:claude
/// The names in `scope` that currently carry uncommitted changes.
fn pending_tables_sql(scope: &[&str]) -> String {
    match table_name_filter(scope) {
        Some(filter) => format!("SELECT DISTINCT table_name FROM dolt_status WHERE {filter}"),
        None => "SELECT DISTINCT table_name FROM dolt_status".to_string(),
    }
}

/// A `table_name IN (...)` predicate over `scope`, or `None` for "every table".
/// The names are quizdom's own compile-time constants, so the literals here are
/// not a quoting surface.
fn table_name_filter(scope: &[&str]) -> Option<String> {
    if scope.is_empty() {
        return None;
    }
    let names: Vec<String> = scope.iter().map(|name| format!("'{name}'")).collect();
    Some(format!("table_name IN ({})", names.join(", ")))
}

/// Run `sql` through `runner` in `repo` and hand back the parsed JSON result.
fn probe_json(runner: &dyn DoltRunner, repo: &Path, sql: &str) -> Result<serde_json::Value> {
    let args = ["sql", "-r", "json", "-q", sql].map(String::from);
    let output = runner.run(repo, &args)?;
    if !output.status.success() {
        return Err(QuizdomError::Dolt(format!(
            "dolt sql {sql} failed: {}",
            clean_dolt_message(&String::from_utf8_lossy(&output.stderr))
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    serde_json::from_str(stdout.trim())
        .map_err(|error| QuizdomError::Parse(format!("dolt_status probe was not JSON: {error}")))
}

// trace:TASK-276 | ai:claude
/// Whether `repo`'s working set holds nothing to commit **in `scope`**, answered
/// from Dolt's `dolt_status` SYSTEM TABLE. An empty `scope` asks about the whole
/// working set.
///
/// The predicate this replaces read dolt's prose ("no changes added to
/// commit"). CI pins dolt 2.2.1 so the pipeline was stable, but a developer on
/// a dolt whose wording had moved would have seen an ordinary clean-tree
/// `db-backup` fail hard instead of pushing. A system table is an interface;
/// a diagnostic sentence is not.
fn working_set_is_clean(runner: &dyn DoltRunner, repo: &Path, scope: &[&str]) -> Result<bool> {
    let sql = pending_changes_sql(scope);
    let value = probe_json(runner, repo, &sql)?;
    value
        .get("rows")
        .and_then(serde_json::Value::as_array)
        .and_then(|rows| rows.first())
        .and_then(|row| row.get("pending"))
        .and_then(|pending| {
            pending
                .as_u64()
                .or_else(|| pending.as_str().and_then(|text| text.parse().ok()))
        })
        .map(|pending| pending == 0)
        .ok_or_else(|| QuizdomError::Parse(format!("dolt_status probe had no count: {value}")))
}

// trace:TASK-276 | ai:claude
/// Whether a failed `dolt commit` was the ordinary clean-tree refusal, asked
/// only of the tables the caller staged.
///
/// Structured first: after staging, a genuinely empty working set leaves
/// `dolt_status` empty for those tables, while a commit that failed for a real
/// reason (an unknown author identity, say) leaves the staged rows sitting
/// there. The prose match survives only for the case where the probe itself
/// cannot run — then the old behaviour is strictly better than declaring a real
/// failure.
fn is_clean_tree_refusal(
    runner: &dyn DoltRunner,
    repo: &Path,
    scope: &[&str],
    reported: &str,
) -> bool {
    match working_set_is_clean(runner, repo, scope) {
        Ok(clean) => clean,
        Err(_) => is_nothing_to_commit(reported),
    }
}

// trace:TASK-297 | ai:claude
/// The tables quizdom owns, and therefore the only ones a quizdom-labelled
/// commit may stage. Everything `db-init` creates and everything `db-migrate`,
/// the store, and `question add` write lives in these two.
pub(crate) const QUIZDOM_TABLES: &[&str] = &["nodes", "edges"];

// trace:TASK-297 | ai:claude
/// Refuse to proceed when a table quizdom is about to stage ALREADY carries
/// changes quizdom did not make.
///
/// `db-backup`'s own documentation invites the user to run `dolt sql` against
/// `data/dolt` by hand, so a pending `UPDATE nodes …` is a supported state, not
/// a corrupt one. But staging is table-granular — `dolt add nodes` takes every
/// pending row in `nodes`, not just ours — so once quizdom writes on top of that
/// edit the two are indistinguishable, and the commit tail would file the user's
/// work in Dolt history under a message describing only quizdom's.
///
/// Hence a PRE-FLIGHT: asked before quizdom writes anything, while the two are
/// still separable. Refusing is the honest option — the alternatives are to
/// mislabel their commit or to silently drop their edit, and quizdom cannot
/// author a message for a change it did not make.
pub(crate) fn refuse_on_foreign_changes(
    runner: &dyn DoltRunner,
    repo: &Path,
    command: &str,
) -> Result<()> {
    let pending = pending_tables(runner, repo, QUIZDOM_TABLES)?;
    if pending.is_empty() {
        return Ok(());
    }
    Err(QuizdomError::Dolt(format!(
        "{repo} has uncommitted changes to {tables} that quizdom did not make.\n\
         `{command}` commits what it writes to those tables, and staging is \
         table-granular — so those edits would land in Dolt history under a \
         quizdom message that does not describe them.\n\
         Settle them first: `quizdom db-backup` commits them under their own \
         snapshot message, or `cd {repo} && dolt add -A && dolt commit -m '…'` \
         records them in your words (`dolt reset --hard` discards them).",
        repo = repo.display(),
        tables = pending.join(", ")
    )))
}

// trace:TASK-297 | ai:claude
/// The tables of `scope` with uncommitted changes, from `dolt_status`. A repo
/// whose working set is clean answers with no `rows` key at all, which is an
/// empty list rather than a parse failure.
fn pending_tables(runner: &dyn DoltRunner, repo: &Path, scope: &[&str]) -> Result<Vec<String>> {
    let value = probe_json(runner, repo, &pending_tables_sql(scope))?;
    Ok(value
        .get("rows")
        .and_then(serde_json::Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get("table_name")?.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

// trace:TASK-279 | ai:claude
/// Dolt decorates its terminal output with backspace runs that erase the
/// `- Uploading...` spinner in place, and `\x08` is not whitespace — so
/// `str::trim` leaves it behind and a forwarded stream trails a line of
/// control characters through the error. Strip the control characters, drop
/// the lines that were nothing but spinner, and join what is left.
///
/// Every dolt (and aida) error surface in the crate runs its raw stream through
/// this. BUG-277 fixed the three surfaces in `db_backup.rs`; TASK-279 lifted the
/// helper here and applied it to the rest rather than copying it four times.
pub(crate) fn clean_dolt_message(raw: &str) -> String {
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

// trace:TASK-280 | ai:claude
/// A best-effort absolute path that does NOT require the path to exist
/// (`canonicalize` does). Used for clone targets, which by definition don't,
/// and by [`guard_test_path`], which must resolve a path it refuses to touch.
pub(crate) fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

// trace:BUG-277 | ai:claude
// trace:TASK-280 | ai:claude
/// A test that writes to a REAL resolved path destroys the developer's working
/// data. That is not hypothetical — it is BUG-277: a verification run with no
/// `--to` pushed a throwaway fixture to the default backup path, and every
/// later backup of the real graph failed.
///
/// So under `cargo test` every path a dolt command is aimed at must live under
/// the system temp directory. A whitelist, deliberately, not a blacklist of
/// known-real paths: a test that forgets to pin cannot reach `data/dolt`, the
/// platform data dir, or anywhere else that matters by any route.
///
/// BUG-277 guarded `db-backup` / `db-restore` only, which was the command that
/// caused the incident. TASK-280 extended it to `db-init`, `db-migrate` and the
/// store constructor — a `db-migrate` test that forgot to pin would import over
/// the real `data/dolt`, which is strictly worse than the backup poisoning.
///
/// It compiles out entirely in a real build: the CLI is *supposed* to write to
/// the real paths.
#[cfg(test)]
pub(crate) fn guard_test_path(flag: &str, path: &Path) {
    let resolved = absolute(path);
    let temp = std::env::temp_dir();
    let under_temp = resolved.starts_with(&temp)
        || std::fs::canonicalize(&temp)
            .map(|canonical| resolved.starts_with(canonical))
            .unwrap_or(false);
    assert!(
        under_temp,
        "BUG-277 tripwire: a test aimed {flag} at {}, outside {}. Tests must \
         pin every dolt path into a temp directory — writing to the resolved \
         real paths poisons the developer's actual domain graph and its backup.",
        resolved.display(),
        temp.display()
    );
}

#[cfg(not(test))]
#[inline]
pub(crate) fn guard_test_path(_flag: &str, _path: &Path) {}

// trace:STORY-291 | ai:claude
// trace:TASK-297 | ai:claude
/// Stage the tables quizdom owns and commit them, returning whether a commit
/// was actually created (`false` = those tables were already clean).
///
/// The commit tail every quizdom WRITER shares. `DoltDomainStore` commits each
/// write (STORY-208), and since STORY-291 `db-init` and `db-migrate` commit
/// theirs too — before that their writes sat untracked, so a freshly
/// bootstrapped-and-migrated repo had one commit and a working set holding the
/// entire graph, and the first `db-backup` pushed an empty history.
///
/// It stages `tables` BY NAME rather than `-A` (TASK-297): a message that says
/// "quizdom db-migrate: import N nodes" must not carry a table quizdom has never
/// heard of. The narrower half of that promise — a hand edit to `nodes` itself —
/// is [`refuse_on_foreign_changes`]'s job, because by the time this runs the two
/// are one diff.
pub(crate) fn commit_tables(
    runner: &dyn DoltRunner,
    repo: &Path,
    tables: &[&str],
    message: &str,
) -> Result<bool> {
    let mut args = vec!["add"];
    args.extend_from_slice(tables);
    run_dolt(runner, repo, &args)?;
    commit_staged(runner, repo, tables, message)
}

// trace:STORY-291 | ai:claude
/// Stage and commit EVERYTHING in `repo`'s working set.
///
/// The one caller is `db-backup`'s pre-push snapshot, and breadth is the whole
/// point there: what it exists to rescue is precisely the change quizdom did not
/// make, under a message ("snapshot working set") that claims nothing about
/// authorship. Every other writer uses [`commit_tables`].
pub(crate) fn commit_working_set(
    runner: &dyn DoltRunner,
    repo: &Path,
    message: &str,
) -> Result<bool> {
    run_dolt(runner, repo, &["add", "-A"])?;
    commit_staged(runner, repo, &[], message)
}

/// The shared half: commit what is staged, treating dolt's clean-tree refusal
/// as success. `scope` is the set of tables just staged — an empty slice means
/// the whole working set.
fn commit_staged(
    runner: &dyn DoltRunner,
    repo: &Path,
    scope: &[&str],
    message: &str,
) -> Result<bool> {
    let args = ["commit", "-m", message].map(String::from);
    let output = runner.run(repo, &args)?;
    if output.status.success() {
        return Ok(true);
    }
    // trace:TASK-284 | ai:claude — separate the streams before anything reads
    // across them; concatenated edge-to-edge, a marker can be formed at the seam.
    let reported = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // trace:TASK-276 | ai:claude — the system table decides, not dolt's prose.
    if is_clean_tree_refusal(runner, repo, scope, &reported) {
        return Ok(false);
    }
    Err(QuizdomError::Dolt(format!(
        "dolt commit failed: {}",
        clean_dolt_message(&reported)
    )))
}

fn run_dolt(runner: &dyn DoltRunner, cwd: &Path, args: &[&str]) -> Result<Output> {
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let output = runner.run(cwd, &args)?;
    if !output.status.success() {
        return Err(QuizdomError::Dolt(format!(
            "dolt {} failed: {}",
            args.first().map(String::as_str).unwrap_or(""),
            // trace:TASK-279 | ai:claude
            clean_dolt_message(&String::from_utf8_lossy(&output.stderr))
        )));
    }
    Ok(output)
}

// trace:STORY-205 | ai:claude
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    /// Records every dolt invocation (cwd + args) and replays canned
    /// `(raw_status, stdout, stderr)` responses in FIFO order.
    struct RecordingDoltRunner {
        calls: RefCell<Vec<(PathBuf, Vec<String>)>>,
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
    }

    impl DoltRunner for RecordingDoltRunner {
        fn run(&self, cwd: &Path, args: &[String]) -> Result<Output> {
            self.calls
                .borrow_mut()
                .push((cwd.to_path_buf(), args.to_vec()));
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

    // trace:TASK-297 | ai:claude
    /// What the pre-flight probe sees in a repo whose working set is clean:
    /// `dolt sql -r json` omits the `rows` key entirely for an empty result.
    const NO_FOREIGN_CHANGES: (i32, &str, &str) = (0, "{}", "");

    /// A repo that already exists, so `db_init` runs its pre-flight — every
    /// such test scripts that probe first.
    fn existing_repo(label: &str) -> PathBuf {
        let dir = temp_dir(label);
        std::fs::create_dir_all(dir.join(".dolt")).unwrap();
        dir
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("quizdom-db-init-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn fresh_directory_inits_then_applies_schema() {
        let dir = temp_dir("fresh");
        let runner = RecordingDoltRunner::new(vec![(0, "", ""), (0, "", "")]);
        let mut output = Vec::new();

        db_init(&dir, &runner, &mut output).expect("bootstrap should succeed");

        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 4, "init, sql, then the commit tail");
        assert_eq!(calls[0].0, dir);
        assert_eq!(calls[0].1, vec!["init".to_string()]);
        assert_eq!(calls[1].1[0..2], ["sql".to_string(), "-q".to_string()]);
        assert_eq!(calls[1].1[2], DOLT_SCHEMA_SQL);
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("Initialised Dolt repo"));
        assert!(rendered.contains("Applied domain-graph schema"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-301 | ai:claude
    /// A nested `dolt_path` bootstraps in one command. Someone who configures
    /// `dolt_path = "~/graphs/quizdom/dolt"` has named a directory whose parents
    /// do not exist yet, and `db-init` is the command whose entire job is to
    /// bring that path into being — stopping at the first missing parent would
    /// hand them a `mkdir -p` to run before the bootstrap command works.
    #[test]
    fn db_init_creates_the_missing_parents_of_a_nested_path() {
        let root = temp_dir("nested");
        let nested = root.join("graphs").join("quizdom").join("dolt");
        assert!(!nested.exists(), "the whole chain starts missing");
        let runner = RecordingDoltRunner::new(vec![(0, "", ""), (0, "", "")]);

        db_init(&nested, &runner, &mut Vec::new()).expect("a nested path should bootstrap");

        assert!(nested.is_dir(), "db-init created {}", nested.display());
        assert_eq!(
            runner.calls.borrow()[0].0,
            nested,
            "and dolt ran IN the directory it created"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // trace:TASK-301 | ai:claude
    /// The other half of the same task: when the directory genuinely cannot be
    /// created, the error names it. A bare `No such file or directory (os error
    /// 2)` withholds the single fact the user needs — WHICH path — and the path
    /// came from a resolution chain (env > settings.toml > compiled default)
    /// they cannot see from the message either.
    #[test]
    fn a_directory_that_cannot_be_created_is_named_in_the_error() {
        let root = temp_dir("blocked");
        std::fs::create_dir_all(&root).unwrap();
        // A parent that is a FILE: `create_dir_all` cannot descend through it.
        let blocker = root.join("not-a-dir");
        std::fs::write(&blocker, b"").unwrap();
        let doomed = blocker.join("dolt");
        let runner = RecordingDoltRunner::new(vec![]);

        match db_init(&doomed, &runner, &mut Vec::new()) {
            Err(QuizdomError::Dolt(message)) => {
                assert!(
                    message.contains(&doomed.display().to_string()),
                    "the error names the path it could not create: {message}"
                );
                assert!(
                    message.contains("domain-graph directory"),
                    "and what that path was for: {message}"
                );
            }
            other => panic!("expected a named IO failure, got {other:?}"),
        }
        assert!(
            runner.calls.borrow().is_empty(),
            "no dolt ran against a directory that does not exist"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn existing_repo_skips_init_but_still_applies_schema() {
        let dir = existing_repo("existing");
        let runner = RecordingDoltRunner::new(vec![NO_FOREIGN_CHANGES, (0, "", "")]);
        let mut output = Vec::new();

        db_init(&dir, &runner, &mut output).expect("re-run should succeed");

        let calls = runner.calls.borrow();
        assert_eq!(
            calls.len(),
            4,
            "the pre-flight probe, the schema, then its commit — no re-init"
        );
        assert_eq!(calls[1].1[0], "sql");
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("already initialised"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:STORY-291 | ai:claude
    #[test]
    fn the_schema_is_committed_not_left_in_the_working_set() {
        let dir = temp_dir("commits");
        let runner = RecordingDoltRunner::new(vec![(0, "", ""), (0, "", "")]);
        let mut output = Vec::new();

        db_init(&dir, &runner, &mut output).expect("bootstrap should succeed");

        let calls = runner.calls.borrow();
        // trace:TASK-297 | ai:claude — by name, never `-A`.
        assert_eq!(
            calls[2].1,
            ["add", "nodes", "edges"].map(String::from),
            "a quizdom-labelled commit stages only the tables quizdom owns"
        );
        assert_eq!(
            calls[3].1,
            [
                "commit".to_string(),
                "-m".to_string(),
                SCHEMA_COMMIT_MESSAGE.to_string()
            ]
        );
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("Committed the schema to Dolt history."));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:STORY-291 | ai:claude — a second db-init changes nothing, so dolt
    // refuses the commit; that refusal is the idempotent path, not a failure,
    // and it must not leave an empty commit behind either.
    #[test]
    fn a_re_run_with_nothing_to_commit_is_not_a_failure() {
        let dir = existing_repo("idempotent");
        let runner = RecordingDoltRunner::new(vec![
            NO_FOREIGN_CHANGES,                         // pre-flight
            (0, "", ""),                                // schema DDL
            (0, "", ""),                                // add nodes edges
            (1 << 8, "no changes added to commit", ""), // commit refuses
        ]);
        let mut output = Vec::new();

        db_init(&dir, &runner, &mut output).expect("a no-op re-run should succeed");

        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("Schema already committed"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:STORY-291 | ai:claude — a commit that fails for a real reason still
    // surfaces, so the clean-tree tolerance cannot swallow a broken repo.
    #[test]
    fn a_genuine_commit_failure_still_surfaces() {
        let dir = existing_repo("commit-fails");
        let runner = RecordingDoltRunner::new(vec![
            NO_FOREIGN_CHANGES,
            (0, "", ""),
            (0, "", ""),
            (1 << 8, "", "author identity unknown"),
        ]);
        let mut output = Vec::new();

        match db_init(&dir, &runner, &mut output) {
            Err(QuizdomError::Dolt(message)) => {
                assert!(message.contains("dolt commit failed"), "{message}");
                assert!(message.contains("author identity unknown"), "{message}");
            }
            other => panic!("expected a Dolt error, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-276 | ai:claude
    /// The clean-tree case decided by the `dolt_status` system table, in
    /// wording no prose matcher would recognise. This is the regression the
    /// task was filed for: a developer on a dolt whose message had moved would
    /// have seen an ordinary no-op re-run fail hard.
    #[test]
    fn a_reworded_clean_tree_refusal_is_still_not_a_failure() {
        let dir = existing_repo("reworded");
        let runner = RecordingDoltRunner::new(vec![
            NO_FOREIGN_CHANGES, // pre-flight
            (0, "", ""),        // schema DDL
            (0, "", ""),        // add nodes edges
            (1 << 8, "", "commit aborted: the working set is up to date"),
            (0, r#"{"rows":[{"pending":0}]}"#, ""), // dolt_status: clean
        ]);
        let mut output = Vec::new();

        db_init(&dir, &runner, &mut output).expect("a clean tree is not a failure");

        let calls = runner.calls.borrow();
        assert_eq!(
            calls[4].1,
            [
                "sql",
                "-r",
                "json",
                "-q",
                &pending_changes_sql(QUIZDOM_TABLES)
            ]
            .map(String::from),
            "the system table is what answers the question — scoped to what was staged"
        );
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("Schema already committed"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-276 | ai:claude
    /// ...and the probe cannot swallow a real failure: pending rows are still
    /// sitting there after the refused commit, so it surfaces.
    #[test]
    fn a_commit_failure_with_pending_changes_still_surfaces() {
        let dir = existing_repo("still-dirty");
        let runner = RecordingDoltRunner::new(vec![
            NO_FOREIGN_CHANGES,
            (0, "", ""),
            (0, "", ""),
            (1 << 8, "", "author identity unknown"),
            (0, r#"{"rows":[{"pending":3}]}"#, ""),
        ]);

        match db_init(&dir, &runner, &mut Vec::new()) {
            Err(QuizdomError::Dolt(message)) => {
                assert!(message.contains("author identity unknown"), "{message}")
            }
            other => panic!("expected a Dolt error, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-276 | ai:claude
    /// When the probe itself cannot run, the prose match is still there as a
    /// fallback — strictly better than declaring a real failure.
    #[test]
    fn an_unusable_probe_falls_back_to_the_prose_match() {
        let dir = existing_repo("no-probe");
        let runner = RecordingDoltRunner::new(vec![
            NO_FOREIGN_CHANGES,
            (0, "", ""),
            (0, "", ""),
            (1 << 8, "no changes added to commit", ""),
            (1 << 8, "", "unknown system table dolt_status"),
        ]);

        db_init(&dir, &runner, &mut Vec::new()).expect("the fallback still recognises it");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-297 | ai:claude
    /// The refusal, on `db-init`. A hand-run `UPDATE nodes …` left in the
    /// working set would otherwise be staged by the schema commit and filed
    /// under "quizdom db-init: apply domain-graph schema" — a sentence with
    /// quizdom's name on it describing a change quizdom did not make.
    #[test]
    fn db_init_refuses_rather_than_absorbing_a_foreign_working_set_edit() {
        let dir = existing_repo("foreign");
        let runner =
            RecordingDoltRunner::new(vec![(0, r#"{"rows":[{"table_name":"nodes"}]}"#, "")]);

        match db_init(&dir, &runner, &mut Vec::new()) {
            Err(QuizdomError::Dolt(message)) => {
                assert!(message.contains("quizdom did not make"), "{message}");
                assert!(message.contains("nodes"), "it names the table: {message}");
                assert!(
                    message.contains("quizdom db-backup"),
                    "and a way out: {message}"
                );
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        let calls = runner.calls.borrow();
        assert_eq!(
            calls.len(),
            1,
            "it refuses on the probe alone — no DDL, no add, no commit: {calls:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-297 | ai:claude
    /// The pre-flight is scoped to the tables quizdom stages, so a hand-made
    /// table it will never touch is not grounds to refuse — that edit was
    /// already safe, because staging by name leaves it behind.
    #[test]
    fn a_pending_table_quizdom_never_stages_is_not_grounds_to_refuse() {
        let dir = existing_repo("foreign-other");
        let runner = RecordingDoltRunner::new(vec![
            (0, "{}", ""), // scoped to nodes/edges: nothing pending
            (0, "", ""),   // schema DDL
            (0, "", ""),   // add nodes edges
            (0, "", ""),   // commit
        ]);

        db_init(&dir, &runner, &mut Vec::new()).expect("an untouched table is not our business");

        assert!(
            runner.calls.borrow()[0].1[4].contains("table_name IN ('nodes', 'edges')"),
            "the probe asks only about the tables about to be staged"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // trace:TASK-304 | ai:claude
    /// The wrong-diagnosis regression: one `NotFound` from `Command::output`,
    /// two unrelated causes. A missing repo directory must be reported as a
    /// missing repo directory — the message it replaces told the user to go and
    /// check a dolt installation that was working perfectly.
    #[test]
    fn a_missing_directory_is_diagnosed_as_a_missing_directory_not_a_missing_dolt() {
        let missing = temp_dir("never-created");
        let error = std::io::Error::from(std::io::ErrorKind::NotFound);

        let message = spawn_failure("dolt", &missing, &error);
        assert!(message.contains("no such directory"), "{message}");
        assert!(
            message.contains(&missing.display().to_string()),
            "{message}"
        );
        assert!(
            !message.contains("on PATH"),
            "the PATH theory is exactly the wrong one here: {message}"
        );
        assert!(
            message.contains("quizdom db-init"),
            "and it names the command that creates it: {message}"
        );

        // A directory that DOES exist leaves the PATH diagnosis intact — that
        // one is right whenever the cwd is not the problem.
        std::fs::create_dir_all(&missing).unwrap();
        let present = spawn_failure("dolt", &missing, &error);
        assert!(
            present.contains("is dolt installed and on PATH?"),
            "{present}"
        );

        // A path that exists but is a file is neither of those stories.
        let file = missing.join("not-a-dir");
        std::fs::write(&file, b"").unwrap();
        let wrong_kind = spawn_failure("dolt", &file, &error);
        assert!(wrong_kind.contains("not a directory"), "{wrong_kind}");

        let _ = std::fs::remove_dir_all(&missing);
    }

    // trace:TASK-304 | ai:claude
    /// ...and the real runner routes through it. No `#[ignore]`: the spawn fails
    /// on the missing `current_dir` whether or not dolt is installed, which is
    /// precisely why the old message could not tell the two apart.
    #[test]
    fn the_real_runner_reports_the_missing_directory_it_was_pointed_at() {
        let missing = temp_dir("runner-missing");
        let runner = SystemDoltRunner::new("dolt".to_string());

        match runner.run(&missing, &["version".to_string()]) {
            Err(QuizdomError::Dolt(message)) => {
                assert!(message.contains("no such directory"), "{message}");
                assert!(
                    message.contains(&missing.display().to_string()),
                    "{message}"
                );
            }
            other => panic!("expected a spawn error, got {other:?}"),
        }
    }

    // trace:TASK-279 | ai:claude
    /// Dolt pads stderr with backspace runs to erase its spinner in place, and
    /// `\x08` is not whitespace — so an unscrubbed stream drags control
    /// characters through the middle of the error the user reads.
    #[test]
    fn dolt_spinner_control_characters_never_reach_a_message() {
        let cleaned = clean_dolt_message("- Uploading...\x08\x08\x08\x08\ncannot open remote\n");
        assert_eq!(cleaned, "- Uploading...; cannot open remote");
        assert!(!cleaned.chars().any(char::is_control));
    }

    // trace:TASK-280 | ai:claude
    /// The tripwire, live on `db-init`: BUG-277 guarded `db-backup` only, but a
    /// `db-init` that resolved the real path would `dolt init` over the
    /// developer's actual graph directory.
    #[test]
    #[should_panic(expected = "BUG-277 tripwire")]
    fn aiming_db_init_outside_the_temp_directory_trips_the_guard() {
        let runner = RecordingDoltRunner::new(vec![]);
        let _ = db_init(
            Path::new("/var/lib/quizdom-must-never-be-touched"),
            &runner,
            &mut Vec::new(),
        );
    }

    #[test]
    fn dolt_failure_surfaces_with_stderr() {
        let dir = temp_dir("failing");
        let runner = RecordingDoltRunner::new(vec![(1 << 8, "", "no dolt for you")]);
        let mut output = Vec::new();

        let result = db_init(&dir, &runner, &mut output);
        match result {
            Err(QuizdomError::Dolt(message)) => {
                assert!(message.contains("dolt init failed"));
                assert!(message.contains("no dolt for you"));
            }
            other => panic!("expected Dolt error, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn schema_is_idempotent_and_matches_the_graph_vocabulary() {
        // Idempotency: table creation must tolerate re-runs.
        assert!(DOLT_SCHEMA_SQL.contains("CREATE TABLE IF NOT EXISTS nodes"));
        assert!(DOLT_SCHEMA_SQL.contains("CREATE TABLE IF NOT EXISTS edges"));
        assert!(!DOLT_SCHEMA_SQL.contains("DROP TABLE"));
        // The node kinds and the six custom edge kinds of graph-schema.md.
        assert!(DOLT_SCHEMA_SQL.contains("ENUM('question', 'term', 'belief')"));
        assert!(DOLT_SCHEMA_SQL
            .contains("ENUM('begets', 'probes', 'refines', 'contradicts', 'agrees', 'disagrees')"));
        // ADR-22's weight:N tag becomes a real numeric column.
        assert!(DOLT_SCHEMA_SQL.contains("weight     INT"));
    }

    #[test]
    fn unknown_argument_is_a_usage_error() {
        let result = DbInitConfig::parse(
            ["db-init".to_string(), "--bogus".to_string()],
            PathBuf::from(DEFAULT_DOLT_DB_PATH),
        );
        assert!(matches!(result, Err(QuizdomError::Usage(_))));
    }

    // trace:TASK-228 | ai:claude — `--path` overrides the resolved default; an
    // unflagged run keeps whatever the env/settings chain handed the parser.
    #[test]
    fn parse_reads_path_and_dolt_overrides() {
        let config = DbInitConfig::parse(
            ["db-init", "--path", "/tmp/x", "--dolt", "dolt2"].map(String::from),
            PathBuf::from("/from/env"),
        )
        .unwrap();
        assert_eq!(config.path, PathBuf::from("/tmp/x"), "--path wins over env");
        assert_eq!(config.dolt_command, "dolt2");

        let defaulted =
            DbInitConfig::parse(["db-init".to_string()], PathBuf::from("/from/env")).unwrap();
        assert_eq!(defaulted.path, PathBuf::from("/from/env"));
    }

    // trace:TASK-276 | ai:claude
    /// The clean-tree probe against the real engine. The mock tests above pin
    /// the decision; this one pins the *interface* — that `dolt_status` still
    /// exists, still answers `dolt sql -r json`, and still discriminates a
    /// clean tree from a dirty one. Without it a dolt release could move the
    /// system table and the whole suite would stay green on the prose fallback.
    #[test]
    #[ignore = "requires the dolt binary on PATH"]
    fn real_dolt_status_probe_discriminates_clean_from_dirty() {
        let dir = temp_dir("probe");
        let runner = SystemDoltRunner::new("dolt".to_string());
        db_init(&dir, &runner, &mut Vec::new()).expect("bootstrap should succeed");

        assert!(
            working_set_is_clean(&runner, &dir, QUIZDOM_TABLES).expect("the probe should run"),
            "db-init commits its own schema, so it leaves a clean tree"
        );

        run_dolt(
            &runner,
            &dir,
            &[
                "sql",
                "-q",
                "INSERT INTO nodes (id, kind, title, body, tags, weight) \
                 VALUES ('Q-1', 'question', 'by hand', '', '', 0)",
            ],
        )
        .expect("hand-run write should land");
        assert!(
            !working_set_is_clean(&runner, &dir, QUIZDOM_TABLES).expect("the probe should run"),
            "an uncommitted hand-run write is exactly what the snapshot rescues"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end acceptance check against a real dolt binary: init a fresh
    /// repo, apply the schema, load the hand-inserted fixture, and walk the
    /// `begets` chain with a recursive CTE. `#[ignore]`d so a plain `cargo
    /// test` never needs a dolt binary; CI installs dolt and runs the whole
    /// `real_dolt` family explicitly (TASK-219), as does:
    /// cargo test real_dolt -- --ignored
    #[test]
    #[ignore = "requires the dolt binary on PATH"]
    fn real_dolt_bootstrap_fixture_and_recursive_cte() {
        let dir = temp_dir("real");
        let runner = SystemDoltRunner {
            command: "dolt".to_string(),
        };
        let mut output = Vec::new();
        db_init(&dir, &runner, &mut output).expect("real bootstrap should succeed");
        // Re-run to prove the DDL is idempotent end-to-end.
        db_init(&dir, &runner, &mut output).expect("re-run should be a no-op");

        let fixture = include_str!("../../../db/fixtures/traversal_fixture.sql");
        run_dolt(&runner, &dir, &["sql", "-q", fixture]).expect("fixture should load");

        let check = include_str!("../../../db/fixtures/traversal_check.sql");
        let result = run_dolt(&runner, &dir, &["sql", "-q", check]).expect("CTE should run");
        let rendered = String::from_utf8_lossy(&result.stdout).to_string();
        for expected in ["Q-1", "Q-2", "Q-3"] {
            assert!(
                rendered.contains(expected),
                "missing {expected}: {rendered}"
            );
        }
        assert!(
            !rendered.contains("TERM-1"),
            "probes edge must not be walked"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
