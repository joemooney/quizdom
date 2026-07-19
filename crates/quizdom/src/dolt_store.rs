// trace:STORY-207 | ai:claude
//! The Dolt-backed [`DomainStore`] (EPIC-202 / ADR-201) and the config/env
//! backend selector that lets it coexist with the aida backend.
//!
//! Per ADR-203, every operation spawns `dolt sql -r json -q <SQL>` in the
//! domain-graph repo — the same one-spawn-per-query choke-point pattern the
//! aida backend uses; no daemon, no port assignment. Reads parse the JSON row
//! format; the ADR-22 weight rides in the real `weight` column (with the
//! `weight:N` tag synthesized back into fetched records so both backends hand
//! the app the same shape). Every mutation is followed by `dolt add -A` +
//! `dolt commit`, so the domain graph's history lives in Dolt itself.
//! Multi-hop traversal overrides the trait's per-hop default
//! ([`DomainStore::reachable`]) with a single recursive CTE.
//!
//! Contradiction-resolution *decision* nodes and their `references` edges are
//! deliberately unsupported here: ADR-201 keeps decision/intent objects in the
//! AIDA store (the Dolt schema's enums carry neither kind), so
//! `AidaCliContradictionResolutionPersister` stays pinned to
//! [`AidaDomainStore`] regardless of the selected backend.

use crate::db_init::{DoltRunner, SystemDoltRunner, DEFAULT_DOLT_DB_PATH};
use crate::db_migrate::sql_quote;
use crate::error::{QuizdomError, Result};
use crate::store::{AidaDomainStore, DomainStore, EdgeKind, NewNode, NodeKind, NodeRecord};
use std::path::PathBuf;

/// The Dolt backend: every operation spawns `dolt` in the domain-graph repo.
pub struct DoltDomainStore<R = SystemDoltRunner> {
    pub(crate) path: PathBuf,
    runner: R,
}

impl DoltDomainStore<SystemDoltRunner> {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            runner: SystemDoltRunner::new("dolt".to_string()),
        }
    }
}

impl<R> DoltDomainStore<R>
where
    R: DoltRunner,
{
    #[cfg(test)]
    pub(crate) fn with_runner(path: impl Into<PathBuf>, runner: R) -> Self {
        Self {
            path: path.into(),
            runner,
        }
    }

    /// The single query choke point: `dolt sql -r json -q <sql>`, parsed into
    /// the `rows` array of the JSON result format.
    fn sql_json(&self, sql: &str) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
        let args: Vec<String> = ["sql", "-r", "json", "-q", sql]
            .into_iter()
            .map(String::from)
            .collect();
        let output = self.runner.run(&self.path, &args)?;
        if !output.status.success() {
            return Err(QuizdomError::Dolt(format!(
                "dolt sql failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let value: serde_json::Value = serde_json::from_str(trimmed).map_err(|error| {
            QuizdomError::Parse(format!("dolt sql -r json output was not JSON: {error}"))
        })?;
        Ok(value
            .get("rows")
            .and_then(serde_json::Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row.as_object().cloned())
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Stage and commit a completed write. A no-op write (e.g. an idempotent
    /// [`DomainStore::ensure_edge`] hitting an existing row) leaves nothing to
    /// commit — dolt's refusal for that case is success here.
    fn commit(&self, message: &str) -> Result<()> {
        let add: Vec<String> = ["add", "-A"].into_iter().map(String::from).collect();
        let output = self.runner.run(&self.path, &add)?;
        if !output.status.success() {
            return Err(QuizdomError::Dolt(format!(
                "dolt add failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        let commit: Vec<String> = ["commit", "-m", message]
            .into_iter()
            .map(String::from)
            .collect();
        let output = self.runner.run(&self.path, &commit)?;
        if output.status.success() {
            return Ok(());
        }
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .to_ascii_lowercase();
        if text.contains("nothing to commit") || text.contains("no changes added") {
            return Ok(());
        }
        Err(QuizdomError::Dolt(format!("dolt commit failed: {text}")))
    }
}

/// Map a node kind onto the `nodes.kind` enum value and its id prefix.
/// Decision nodes have no Dolt representation — ADR-201 keeps them in AIDA.
fn dolt_kind(kind: NodeKind) -> Result<(&'static str, &'static str)> {
    match kind {
        NodeKind::Question => Ok(("question", "Q-")),
        NodeKind::Term => Ok(("term", "TERM-")),
        NodeKind::Decision => Err(QuizdomError::Dolt(
            "decision nodes live in the AIDA store, not Dolt (ADR-201)".to_string(),
        )),
    }
}

/// The `edges.kind` enum value for a domain edge. `references` edges join
/// AIDA-side decision nodes and have no Dolt representation (ADR-201).
fn domain_edge(edge: EdgeKind) -> Result<&'static str> {
    if edge == EdgeKind::References {
        return Err(QuizdomError::Dolt(
            "references edges live in the AIDA store, not Dolt (ADR-201)".to_string(),
        ));
    }
    Ok(edge.as_str())
}

/// Split the ADR-22 `weight:N` tag out of a tag list: the numeric value goes
/// to the `weight` column, everything else stays in the `tags` column.
fn split_weight_tag(tags: &[String]) -> (Vec<String>, Option<u32>) {
    let mut weight = None;
    let kept = tags
        .iter()
        .filter(|tag| {
            match tag
                .strip_prefix("weight:")
                .and_then(|value| value.parse::<u32>().ok())
            {
                Some(value) => {
                    weight = Some(value);
                    false
                }
                None => true,
            }
        })
        .cloned()
        .collect();
    (kept, weight)
}

fn string_column(row: &serde_json::Map<String, serde_json::Value>, name: &str) -> String {
    row.get(name)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// A numeric column, tolerating dolt versions that render numbers as strings.
fn u32_column(row: &serde_json::Map<String, serde_json::Value>, name: &str) -> u32 {
    row.get(name)
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
        .unwrap_or(0) as u32
}

impl<R> DomainStore for DoltDomainStore<R>
where
    R: DoltRunner,
{
    fn fetch_node(&self, id: &str) -> Result<NodeRecord> {
        let rows = self.sql_json(&format!(
            "SELECT id, title, body, tags, weight FROM nodes WHERE id = {};",
            sql_quote(id)
        ))?;
        let row = rows
            .first()
            .ok_or_else(|| QuizdomError::Dolt(format!("node {id} not found in the Dolt store")))?;
        let weight = u32_column(row, "weight");
        let mut tags: Vec<String> = string_column(row, "tags")
            .split(',')
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
            .map(str::to_string)
            .collect();
        // Both backends hand the app the same shape: the aida backend's tag
        // list carries `weight:N`, so synthesize it back from the column.
        if weight > 0 {
            tags.push(format!("weight:{weight}"));
        }
        Ok(NodeRecord {
            id: string_column(row, "id"),
            title: string_column(row, "title"),
            tags,
            weight,
            body: string_column(row, "body"),
        })
    }

    fn list_node_ids(&self, kind: NodeKind) -> Result<Vec<String>> {
        let (kind_value, _) = dolt_kind(kind)?;
        Ok(self
            .sql_json(&format!(
                "SELECT id FROM nodes WHERE kind = '{kind_value}' ORDER BY id;"
            ))?
            .iter()
            .map(|row| string_column(row, "id"))
            .collect())
    }

    fn neighbors(&self, id: &str, edge: EdgeKind) -> Result<Vec<String>> {
        let kind = domain_edge(edge)?;
        Ok(self
            .sql_json(&format!(
                "SELECT to_id FROM edges WHERE from_id = {} AND kind = '{kind}' \
                 ORDER BY created_at, to_id;",
                sql_quote(id)
            ))?
            .iter()
            .map(|row| string_column(row, "to_id"))
            .collect())
    }

    fn create_node(&self, node: &NewNode) -> Result<String> {
        let (kind_value, prefix) = dolt_kind(node.kind)?;
        // Mint the next id: highest existing numeric suffix for the prefix,
        // plus one. Single-user CLI (ADR-4) — no concurrent minting to race.
        let next = self
            .sql_json(&format!("SELECT id FROM nodes WHERE id LIKE '{prefix}%';"))?
            .iter()
            .filter_map(|row| {
                string_column(row, "id")
                    .strip_prefix(prefix)?
                    .parse::<u64>()
                    .ok()
            })
            .max()
            .unwrap_or(0)
            + 1;
        let id = format!("{prefix}{next}");
        let (tags, weight) = split_weight_tag(&node.tags);
        self.sql_json(&format!(
            "INSERT INTO nodes (id, kind, title, body, tags, weight) \
             VALUES ({}, '{kind_value}', {}, {}, {}, {});",
            sql_quote(&id),
            sql_quote(&node.title),
            sql_quote(&node.description),
            sql_quote(&tags.join(",")),
            weight.unwrap_or(0)
        ))?;
        self.commit(&format!("quizdom: add node {id}"))?;
        Ok(id)
    }

    fn create_edge(&self, from: &str, to: &str, edge: EdgeKind) -> Result<()> {
        let kind = domain_edge(edge)?;
        // The (from_id, to_id, kind) primary key makes a duplicate insert a
        // dolt error — matching the trait contract (existing edge is an error).
        self.sql_json(&format!(
            "INSERT INTO edges (from_id, to_id, kind) VALUES ({}, {}, '{kind}');",
            sql_quote(from),
            sql_quote(to)
        ))?;
        self.commit(&format!("quizdom: add edge {from} -{kind}-> {to}"))
    }

    fn ensure_edge(&self, from: &str, to: &str, edge: EdgeKind) -> Result<()> {
        let kind = domain_edge(edge)?;
        self.sql_json(&format!(
            "INSERT IGNORE INTO edges (from_id, to_id, kind) VALUES ({}, {}, '{kind}');",
            sql_quote(from),
            sql_quote(to)
        ))?;
        self.commit(&format!("quizdom: ensure edge {from} -{kind}-> {to}"))
    }

    fn replace_tags(&self, id: &str, tags: &[String]) -> Result<()> {
        let (tags, weight) = split_weight_tag(tags);
        let weight_clause = weight
            .map(|value| format!(", weight = {value}"))
            .unwrap_or_default();
        self.sql_json(&format!(
            "UPDATE nodes SET tags = {}{weight_clause} WHERE id = {};",
            sql_quote(&tags.join(",")),
            sql_quote(id)
        ))?;
        self.commit(&format!("quizdom: retag {id}"))
    }

    /// The STORY-207 multi-hop read: one recursive CTE instead of the trait
    /// default's per-hop walk. `UNION` (not `UNION ALL`) deduplicates rows, so
    /// the walk terminates on cyclic graphs — the same visited-set semantics
    /// as the default BFS, and the same sorted result order.
    fn reachable(&self, root: &str, edge: EdgeKind) -> Result<Vec<String>> {
        let kind = domain_edge(edge)?;
        Ok(self
            .sql_json(&format!(
                "WITH RECURSIVE reachable (id) AS (\
                 SELECT CAST({root} AS CHAR(64)) \
                 UNION \
                 SELECT e.to_id FROM edges e JOIN reachable r ON e.from_id = r.id \
                 WHERE e.kind = '{kind}') \
                 SELECT id FROM reachable ORDER BY id;",
                root = sql_quote(root)
            ))?
            .iter()
            .map(|row| string_column(row, "id"))
            .collect())
    }
}

/// The runtime-selected backend (ADR-201's coexistence requirement): consumers
/// default to this, and config/env decides which implementation answers.
pub enum SelectedDomainStore {
    Aida(AidaDomainStore),
    Dolt(DoltDomainStore),
}

impl Default for SelectedDomainStore {
    fn default() -> Self {
        domain_store_from_config()
    }
}

impl DomainStore for SelectedDomainStore {
    fn fetch_node(&self, id: &str) -> Result<NodeRecord> {
        match self {
            Self::Aida(store) => store.fetch_node(id),
            Self::Dolt(store) => store.fetch_node(id),
        }
    }

    fn list_node_ids(&self, kind: NodeKind) -> Result<Vec<String>> {
        match self {
            Self::Aida(store) => store.list_node_ids(kind),
            Self::Dolt(store) => store.list_node_ids(kind),
        }
    }

    fn neighbors(&self, id: &str, edge: EdgeKind) -> Result<Vec<String>> {
        match self {
            Self::Aida(store) => store.neighbors(id, edge),
            Self::Dolt(store) => store.neighbors(id, edge),
        }
    }

    fn create_node(&self, node: &NewNode) -> Result<String> {
        match self {
            Self::Aida(store) => store.create_node(node),
            Self::Dolt(store) => store.create_node(node),
        }
    }

    fn create_edge(&self, from: &str, to: &str, edge: EdgeKind) -> Result<()> {
        match self {
            Self::Aida(store) => store.create_edge(from, to, edge),
            Self::Dolt(store) => store.create_edge(from, to, edge),
        }
    }

    fn ensure_edge(&self, from: &str, to: &str, edge: EdgeKind) -> Result<()> {
        match self {
            Self::Aida(store) => store.ensure_edge(from, to, edge),
            Self::Dolt(store) => store.ensure_edge(from, to, edge),
        }
    }

    fn replace_tags(&self, id: &str, tags: &[String]) -> Result<()> {
        match self {
            Self::Aida(store) => store.replace_tags(id, tags),
            Self::Dolt(store) => store.replace_tags(id, tags),
        }
    }

    // Delegated explicitly so the Dolt backend's recursive-CTE override runs
    // instead of the trait's per-hop default.
    fn reachable(&self, root: &str, edge: EdgeKind) -> Result<Vec<String>> {
        match self {
            Self::Aida(store) => store.reachable(root, edge),
            Self::Dolt(store) => store.reachable(root, edge),
        }
    }
}

/// Resolve the backend from the environment and the STORY-194 settings file.
///
/// `QUIZDOM_STORE=dolt` (env) or `store = dolt` (settings.toml) selects Dolt;
/// anything else — including unset and unrecognised values, per the settings
/// file's ignore-unparseable convention — selects aida. The Dolt repo path
/// comes from `QUIZDOM_DOLT_PATH` / `dolt_path`, defaulting to
/// [`DEFAULT_DOLT_DB_PATH`]. Env wins over config.
pub fn domain_store_from_config() -> SelectedDomainStore {
    let config = crate::settings::config_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    select_domain_store(
        std::env::var("QUIZDOM_STORE").ok().as_deref(),
        std::env::var("QUIZDOM_DOLT_PATH").ok().as_deref(),
        &config,
    )
}

/// The pure selection logic, split from the env/file reads so it is testable
/// without touching the process environment (the settings.rs pattern).
fn select_domain_store(
    env_store: Option<&str>,
    env_path: Option<&str>,
    config: &str,
) -> SelectedDomainStore {
    let choice = env_store
        .map(|value| value.trim().to_string())
        .or_else(|| config_value(config, "store"));
    match choice.as_deref() {
        Some("dolt") => {
            let path = env_path
                .map(|value| value.trim().to_string())
                .or_else(|| config_value(config, "dolt_path"))
                .unwrap_or_else(|| DEFAULT_DOLT_DB_PATH.to_string());
            SelectedDomainStore::Dolt(DoltDomainStore::new(path))
        }
        _ => SelectedDomainStore::Aida(AidaDomainStore::default()),
    }
}

/// Read one `key = value` line from the flat settings schema (unknown keys
/// are ignored on load per STORY-194, so these keys are forward-compatible).
fn config_value(config: &str, key: &str) -> Option<String> {
    config.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        (name.trim() == key).then(|| value.trim().trim_matches('"').to_string())
    })
}

// trace:STORY-207 | ai:claude
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::CommandRunner;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::process::{ExitStatus, Output};

    /// Records every dolt invocation and replays canned
    /// `(raw_status, stdout, stderr)` responses in FIFO order.
    struct ScriptedDoltRunner {
        calls: RefCell<Vec<Vec<String>>>,
        responses: RefCell<Vec<(i32, String, String)>>,
    }

    impl ScriptedDoltRunner {
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

    impl DoltRunner for ScriptedDoltRunner {
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

    fn store_with(responses: Vec<(i32, &str, &str)>) -> DoltDomainStore<ScriptedDoltRunner> {
        DoltDomainStore::with_runner("/tmp/quizdom-dolt", ScriptedDoltRunner::new(responses))
    }

    fn sql_of(call: &[String]) -> &str {
        assert_eq!(&call[0..4], &["sql", "-r", "json", "-q"], "call: {call:?}");
        &call[4]
    }

    #[test]
    fn fetch_node_reads_the_weight_column_and_synthesizes_the_tag() {
        let store = store_with(vec![(
            0,
            r#"{"rows":[{"id":"Q-1","title":"Does free will exist?","body":"seed","tags":"topic:free-will,answer:yes-no","weight":70}]}"#,
            "",
        )]);

        let record = store.fetch_node("Q-1").expect("fetch should succeed");

        assert_eq!(record.id, "Q-1");
        assert_eq!(record.title, "Does free will exist?");
        assert_eq!(record.body, "seed");
        assert_eq!(record.weight, 70);
        assert_eq!(
            record.tags,
            ["topic:free-will", "answer:yes-no", "weight:70"].map(String::from)
        );
        let calls = store.runner.calls.borrow();
        assert!(sql_of(&calls[0]).contains("WHERE id = 'Q-1'"));
    }

    #[test]
    fn fetch_node_missing_row_is_an_error() {
        let store = store_with(vec![(0, r#"{"rows":[]}"#, "")]);
        match store.fetch_node("Q-404") {
            Err(QuizdomError::Dolt(message)) => assert!(message.contains("not found")),
            other => panic!("expected Dolt error, got {other:?}"),
        }
    }

    #[test]
    fn list_node_ids_selects_by_kind() {
        let store = store_with(vec![(0, r#"{"rows":[{"id":"Q-1"},{"id":"Q-2"}]}"#, "")]);

        let ids = store
            .list_node_ids(NodeKind::Question)
            .expect("list should succeed");

        assert_eq!(ids, ["Q-1", "Q-2"].map(String::from));
        let calls = store.runner.calls.borrow();
        assert!(sql_of(&calls[0]).contains("kind = 'question'"));
    }

    #[test]
    fn neighbors_is_a_one_hop_select() {
        let store = store_with(vec![(0, r#"{"rows":[{"to_id":"Q-2"}]}"#, "")]);

        let targets = store
            .neighbors("Q-1", EdgeKind::Begets)
            .expect("neighbors should succeed");

        assert_eq!(targets, ["Q-2"].map(String::from));
        let calls = store.runner.calls.borrow();
        let sql = sql_of(&calls[0]);
        assert!(sql.contains("from_id = 'Q-1'"));
        assert!(sql.contains("kind = 'begets'"));
    }

    #[test]
    fn reachable_runs_a_single_recursive_cte() {
        let store = store_with(vec![(
            0,
            r#"{"rows":[{"id":"Q-1"},{"id":"Q-2"},{"id":"Q-3"},{"id":"Q-4"}]}"#,
            "",
        )]);

        let reached = store
            .reachable("Q-1", EdgeKind::Begets)
            .expect("traversal should succeed");

        assert_eq!(reached, ["Q-1", "Q-2", "Q-3", "Q-4"].map(String::from));
        let calls = store.runner.calls.borrow();
        assert_eq!(calls.len(), 1, "one CTE spawn, not one per hop");
        let sql = sql_of(&calls[0]);
        assert!(sql.contains("WITH RECURSIVE"));
        assert!(sql.contains("e.kind = 'begets'"));
    }

    #[test]
    fn create_node_mints_the_next_id_and_commits() {
        let store = store_with(vec![
            (0, r#"{"rows":[{"id":"Q-7"},{"id":"Q-3"}]}"#, ""), // max scan
            (0, "", ""),                                        // insert
            (0, "", ""),                                        // add -A
            (0, "", ""),                                        // commit
        ]);
        let node = NewNode {
            kind: NodeKind::Question,
            title: "What's a cause?".to_string(),
            description: "follow-on".to_string(),
            tags: ["topic:free-will", "weight:55"].map(String::from).to_vec(),
        };

        let id = store.create_node(&node).expect("create should succeed");

        assert_eq!(id, "Q-8");
        let calls = store.runner.calls.borrow();
        let insert = sql_of(&calls[1]);
        assert!(insert.contains("'Q-8'"));
        assert!(
            insert.contains("'What''s a cause?'"),
            "quotes escaped: {insert}"
        );
        assert!(
            insert.contains("'topic:free-will'"),
            "weight tag split out: {insert}"
        );
        assert!(insert.contains(", 55)"), "weight in the column: {insert}");
        assert_eq!(calls[2], ["add", "-A"]);
        assert_eq!(&calls[3][0..2], &["commit", "-m"]);
    }

    #[test]
    fn term_ids_use_the_term_prefix_starting_at_one() {
        let store = store_with(vec![(0, r#"{"rows":[]}"#, ""), (0, "", "")]);
        let node = NewNode {
            kind: NodeKind::Term,
            title: "free will".to_string(),
            description: "".to_string(),
            tags: Vec::new(),
        };

        let id = store.create_node(&node).expect("create should succeed");

        assert_eq!(id, "TERM-1");
        let calls = store.runner.calls.borrow();
        assert!(sql_of(&calls[1]).contains("'term'"));
    }

    #[test]
    fn create_edge_surfaces_duplicate_key_errors() {
        let store = store_with(vec![(1 << 8, "", "duplicate primary key")]);
        match store.create_edge("Q-1", "Q-2", EdgeKind::Begets) {
            Err(QuizdomError::Dolt(message)) => assert!(message.contains("duplicate")),
            other => panic!("expected Dolt error, got {other:?}"),
        }
    }

    #[test]
    fn ensure_edge_tolerates_the_nothing_to_commit_no_op() {
        let store = store_with(vec![
            (0, "", ""),                       // insert ignore (no-op)
            (0, "", ""),                       // add -A
            (1 << 8, "", "nothing to commit"), // commit refuses
        ]);

        store
            .ensure_edge("Q-1", "Q-2", EdgeKind::Begets)
            .expect("idempotent ensure should succeed");

        let calls = store.runner.calls.borrow();
        assert!(sql_of(&calls[0]).contains("INSERT IGNORE"));
    }

    #[test]
    fn replace_tags_moves_the_weight_into_the_column() {
        let store = store_with(vec![(0, "", ""), (0, "", ""), (0, "", "")]);
        let tags = ["topic:free-will", "quality:insightful", "weight:62"].map(String::from);

        store
            .replace_tags("Q-1", &tags)
            .expect("retag should succeed");

        let calls = store.runner.calls.borrow();
        let update = sql_of(&calls[0]);
        assert!(update.contains("tags = 'topic:free-will,quality:insightful'"));
        assert!(update.contains("weight = 62"));
        assert!(update.contains("WHERE id = 'Q-1'"));
    }

    #[test]
    fn decision_nodes_and_references_edges_stay_in_aida() {
        let store = store_with(vec![]);
        let decision = NewNode {
            kind: NodeKind::Decision,
            title: "resolution".to_string(),
            description: "".to_string(),
            tags: Vec::new(),
        };

        for message in [
            store.create_node(&decision).unwrap_err().to_string(),
            store
                .create_edge("DEC-1", "BELIEF-1", EdgeKind::References)
                .unwrap_err()
                .to_string(),
            store
                .neighbors("DEC-1", EdgeKind::References)
                .unwrap_err()
                .to_string(),
        ] {
            assert!(message.contains("ADR-201"), "got: {message}");
        }
        assert!(store.runner.calls.borrow().is_empty(), "no dolt spawns");
    }

    /// Serves canned `aida rel list` outputs keyed by node id, so the trait's
    /// default per-hop walk can run against an arbitrary edge map.
    struct RelMapRunner {
        edges: BTreeMap<String, Vec<(String, String)>>,
    }

    impl RelMapRunner {
        fn new(edges: &[(&str, &str, &str)]) -> Self {
            let mut map: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
            for (from, kind, to) in edges {
                map.entry(from.to_string())
                    .or_default()
                    .push((kind.to_string(), to.to_string()));
            }
            Self { edges: map }
        }
    }

    impl CommandRunner for RelMapRunner {
        fn run(&self, _program: &str, args: &[String]) -> Result<Output> {
            assert_eq!(&args[0..2], &["rel", "list"], "args: {args:?}");
            let id = &args[2];
            let stdout = self
                .edges
                .get(id)
                .map(|targets| {
                    targets
                        .iter()
                        .map(|(kind, to)| format!("{id} {kind} {to}\n"))
                        .collect::<String>()
                })
                .unwrap_or_default();
            Ok(Output {
                status: ExitStatus::from_raw(0),
                stdout: stdout.into_bytes(),
                stderr: Vec::new(),
            })
        }
    }

    #[test]
    fn default_reachable_walks_per_hop_and_survives_cycles() {
        let runner = RelMapRunner::new(&[
            ("Q-1", "begets", "Q-2"),
            ("Q-2", "begets", "Q-3"),
            ("Q-3", "begets", "Q-1"), // cycle back to the root
            ("Q-1", "probes", "TERM-1"),
        ]);
        let store = AidaDomainStore::new("aida", runner);

        let reached = store
            .reachable("Q-1", EdgeKind::Begets)
            .expect("walk should terminate");

        assert_eq!(reached, ["Q-1", "Q-2", "Q-3"].map(String::from));
    }

    #[test]
    fn backend_selection_defaults_to_aida() {
        assert!(matches!(
            select_domain_store(None, None, ""),
            SelectedDomainStore::Aida(_)
        ));
        // Unrecognised values fall back to aida (the settings-file
        // ignore-unparseable convention).
        assert!(matches!(
            select_domain_store(Some("postgres"), None, ""),
            SelectedDomainStore::Aida(_)
        ));
    }

    #[test]
    fn backend_selection_env_picks_dolt_with_the_default_path() {
        match select_domain_store(Some("dolt"), None, "") {
            SelectedDomainStore::Dolt(store) => {
                assert_eq!(store.path, PathBuf::from(DEFAULT_DOLT_DB_PATH));
            }
            SelectedDomainStore::Aida(_) => panic!("expected the Dolt backend"),
        }
    }

    #[test]
    fn backend_selection_reads_the_settings_file_and_env_wins() {
        let config = "editor = \"vim\"\nstore = \"dolt\"\ndolt_path = \"/tmp/graph\"\n";
        match select_domain_store(None, None, config) {
            SelectedDomainStore::Dolt(store) => {
                assert_eq!(store.path, PathBuf::from("/tmp/graph"));
            }
            SelectedDomainStore::Aida(_) => panic!("expected the Dolt backend"),
        }
        // Env beats the file in both directions.
        assert!(matches!(
            select_domain_store(Some("aida"), None, config),
            SelectedDomainStore::Aida(_)
        ));
        match select_domain_store(Some("dolt"), Some("/env/path"), config) {
            SelectedDomainStore::Dolt(store) => {
                assert_eq!(store.path, PathBuf::from("/env/path"));
            }
            SelectedDomainStore::Aida(_) => panic!("expected the Dolt backend"),
        }
    }

    /// The STORY-207 acceptance check against a real dolt binary: bootstrap a
    /// fixture repo, run the full trait surface against it, and verify that a
    /// 3+ hop traversal returns the same result set the aida backend's
    /// per-hop walk produces on equivalent data. Ignored in CI (no dolt
    /// there); run locally with: cargo test real_dolt -- --ignored
    #[test]
    #[ignore = "requires the dolt binary on PATH"]
    fn real_dolt_full_trait_surface_and_aida_parity() {
        let dir = std::env::temp_dir().join(format!("quizdom-dolt-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut output = Vec::new();
        crate::db_init::run_db_init(
            [
                "db-init".to_string(),
                "--path".to_string(),
                dir.display().to_string(),
            ],
            &mut output,
        )
        .expect("bootstrap should succeed");

        let store = DoltDomainStore::new(&dir);

        // Writes: a 3-hop begets chain plus a probes spur onto a term.
        let mut chain = Vec::new();
        for index in 0..4 {
            let id = store
                .create_node(&NewNode {
                    kind: NodeKind::Question,
                    title: format!("question {index}"),
                    description: format!("body {index}"),
                    tags: ["topic:free-will".to_string(), "weight:70".to_string()].to_vec(),
                })
                .expect("create_node should succeed");
            chain.push(id);
        }
        let term = store
            .create_node(&NewNode {
                kind: NodeKind::Term,
                title: "free will".to_string(),
                description: "the term".to_string(),
                tags: Vec::new(),
            })
            .expect("term create should succeed");
        for pair in chain.windows(2) {
            store
                .create_edge(&pair[0], &pair[1], EdgeKind::Begets)
                .expect("edge create should succeed");
        }
        store
            .create_edge(&chain[0], &term, EdgeKind::Probes)
            .expect("probes edge should succeed");

        // Contract: duplicate create errors, duplicate ensure is a no-op.
        assert!(store
            .create_edge(&chain[0], &chain[1], EdgeKind::Begets)
            .is_err());
        store
            .ensure_edge(&chain[0], &chain[1], EdgeKind::Begets)
            .expect("ensure should be idempotent");

        // Reads: weight column, kind-filtered listing, one-hop neighbors.
        let root = store.fetch_node(&chain[0]).expect("fetch should succeed");
        assert_eq!(root.weight, 70);
        assert_eq!(root.title, "question 0");
        assert!(root.tags.contains(&"weight:70".to_string()));
        assert_eq!(
            store.list_node_ids(NodeKind::Question).unwrap(),
            chain,
            "chain ids in order"
        );
        assert_eq!(
            store.neighbors(&chain[0], EdgeKind::Begets).unwrap(),
            [chain[1].clone()]
        );

        // The ADR-22 write path: retag moves the weight into the column.
        store
            .replace_tags(
                &chain[0],
                &["topic:free-will".to_string(), "weight:41".to_string()],
            )
            .expect("retag should succeed");
        assert_eq!(store.fetch_node(&chain[0]).unwrap().weight, 41);

        // Acceptance: the 3-hop traversal from the root, via one recursive
        // CTE, matches the aida backend's per-hop walk on equivalent data.
        let dolt_reached = store
            .reachable(&chain[0], EdgeKind::Begets)
            .expect("CTE traversal should succeed");
        let mut expected = chain.clone();
        expected.sort();
        assert_eq!(dolt_reached, expected);
        assert!(!dolt_reached.contains(&term), "probes spur not walked");

        let equivalent: Vec<(String, String, String)> = chain
            .windows(2)
            .map(|pair| (pair[0].clone(), "begets".to_string(), pair[1].clone()))
            .chain([(chain[0].clone(), "probes".to_string(), term.clone())])
            .collect();
        let borrowed: Vec<(&str, &str, &str)> = equivalent
            .iter()
            .map(|(from, kind, to)| (from.as_str(), kind.as_str(), to.as_str()))
            .collect();
        let aida = AidaDomainStore::new("aida", RelMapRunner::new(&borrowed));
        let aida_reached = aida
            .reachable(&chain[0], EdgeKind::Begets)
            .expect("per-hop walk should succeed");
        assert_eq!(aida_reached, dolt_reached, "backend parity");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
