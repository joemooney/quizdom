// trace:STORY-205 | ai:claude
//! `quizdom db-init` — bootstrap the Dolt repository that will hold the
//! domain graph (EPIC-202 / ADR-201).
//!
//! Creates the repo directory if needed, runs `dolt init` unless the
//! directory is already a Dolt repo, and applies the checked-in DDL
//! (`db/schema.sql`, embedded at compile time). The DDL is idempotent
//! (`CREATE TABLE IF NOT EXISTS` only), so re-running `db-init` is safe.
//!
//! This slice bootstraps the empty database only; the exporter (STORY-206)
//! and the Dolt-backed `DomainStore` (STORY-207) build on it.

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
/// flow can be unit-tested without a dolt binary on PATH.
pub(crate) trait DoltRunner {
    fn run(&self, cwd: &Path, args: &[String]) -> Result<Output>;
}

/// The real runner: spawns the `dolt` binary. `db_init.rs` is allowlisted in
/// the BUG-200 guard test because this spawns dolt, not aida — the pinned
/// aida output format does not apply.
pub(crate) struct SystemDoltRunner {
    command: String,
}

impl DoltRunner for SystemDoltRunner {
    fn run(&self, cwd: &Path, args: &[String]) -> Result<Output> {
        std::process::Command::new(&self.command)
            .current_dir(cwd)
            .args(args)
            .output()
            .map_err(|error| {
                QuizdomError::Dolt(format!(
                    "failed to spawn `{}`: {error}; is dolt installed and on PATH?",
                    self.command
                ))
            })
    }
}

struct DbInitConfig {
    path: PathBuf,
    dolt_command: String,
}

impl DbInitConfig {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut path = PathBuf::from(DEFAULT_DOLT_DB_PATH);
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
    format!("usage: quizdom db-init [--path {DEFAULT_DOLT_DB_PATH}] [--dolt dolt]")
}

/// Entry point for `quizdom db-init`.
pub fn run_db_init(args: impl IntoIterator<Item = String>, output: &mut impl Write) -> Result<()> {
    let config = DbInitConfig::parse(args)?;
    let runner = SystemDoltRunner {
        command: config.dolt_command.clone(),
    };
    db_init(&config.path, &runner, output)
}

/// The bootstrap flow: ensure the directory exists, `dolt init` it unless a
/// `.dolt/` repo is already present, then apply the idempotent schema DDL.
fn db_init(path: &Path, runner: &dyn DoltRunner, output: &mut impl Write) -> Result<()> {
    std::fs::create_dir_all(path)?;

    if path.join(".dolt").exists() {
        writeln!(
            output,
            "Dolt repo already initialised at {} — skipping `dolt init`.",
            path.display()
        )?;
    } else {
        run_dolt(runner, path, &["init"])?;
        writeln!(output, "Initialised Dolt repo at {}.", path.display())?;
    }

    run_dolt(runner, path, &["sql", "-q", DOLT_SCHEMA_SQL])?;
    writeln!(
        output,
        "Applied domain-graph schema (nodes, edges) — DDL is idempotent."
    )?;
    Ok(())
}

fn run_dolt(runner: &dyn DoltRunner, cwd: &Path, args: &[&str]) -> Result<Output> {
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let output = runner.run(cwd, &args)?;
    if !output.status.success() {
        return Err(QuizdomError::Dolt(format!(
            "dolt {} failed: {}",
            args.first().map(String::as_str).unwrap_or(""),
            String::from_utf8_lossy(&output.stderr)
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
        assert_eq!(calls.len(), 2, "init then sql");
        assert_eq!(calls[0].0, dir);
        assert_eq!(calls[0].1, vec!["init".to_string()]);
        assert_eq!(calls[1].1[0..2], ["sql".to_string(), "-q".to_string()]);
        assert_eq!(calls[1].1[2], DOLT_SCHEMA_SQL);
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("Initialised Dolt repo"));
        assert!(rendered.contains("Applied domain-graph schema"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_repo_skips_init_but_still_applies_schema() {
        let dir = temp_dir("existing");
        std::fs::create_dir_all(dir.join(".dolt")).unwrap();
        let runner = RecordingDoltRunner::new(vec![(0, "", "")]);
        let mut output = Vec::new();

        db_init(&dir, &runner, &mut output).expect("re-run should succeed");

        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 1, "schema only — no re-init");
        assert_eq!(calls[0].1[0], "sql");
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("already initialised"));

        let _ = std::fs::remove_dir_all(&dir);
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
        let result = DbInitConfig::parse(["db-init".to_string(), "--bogus".to_string()]);
        assert!(matches!(result, Err(QuizdomError::Usage(_))));
    }

    #[test]
    fn parse_reads_path_and_dolt_overrides() {
        let config = DbInitConfig::parse(
            ["db-init", "--path", "/tmp/x", "--dolt", "dolt2"].map(String::from),
        )
        .unwrap();
        assert_eq!(config.path, PathBuf::from("/tmp/x"));
        assert_eq!(config.dolt_command, "dolt2");
    }

    /// End-to-end acceptance check against a real dolt binary: init a fresh
    /// repo, apply the schema, load the hand-inserted fixture, and walk the
    /// `begets` chain with a recursive CTE. Ignored in CI (no dolt there);
    /// run locally with: cargo test real_dolt -- --ignored
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
