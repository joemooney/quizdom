// trace:STORY-207 | ai:claude
// trace:STORY-208 | ai:claude — cutover: Dolt is the only domain backend.
//! The Dolt-backed [`DomainStore`] (EPIC-202 / ADR-201) — since the STORY-208
//! cutover, the only domain backend.
//!
//! Per ADR-203, every operation spawns `dolt sql -r json -q <SQL>` in the
//! domain-graph repo — one spawn per query; no daemon, no port assignment.
//! Reads parse the JSON row format; the selection weight is the real numeric
//! `weight` column (ADR-22's `weight:N` tag encoding is retired — tags no
//! longer carry weight anywhere in the app). Every mutation is followed by
//! `dolt add -A` + `dolt commit`, so the domain graph's history lives in Dolt
//! itself. Multi-hop traversal is a single recursive CTE
//! ([`DomainStore::reachable`]) — ADR-31's app-side per-hop walk is gone.
//!
//! Contradiction-resolution *decision* nodes and their `references` edges are
//! not domain data: ADR-201 keeps decision/intent objects in the AIDA store,
//! written through [`crate::store::AidaIntentStore`].

use crate::db_init::{DoltRunner, SystemDoltRunner};
use crate::db_migrate::sql_quote;
use crate::error::{QuizdomError, Result};
use crate::store::{DomainStore, EdgeKind, NewNode, NodeKind, NodeRecord};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

// trace:STORY-244 | ai:claude
/// How many ids ride in one `IN (...)` list or one batched `UPDATE`. Bounds
/// the SQL text handed to a single `dolt` spawn, so a bank far larger than
/// this costs one extra spawn per chunk rather than one per row.
const MAX_BATCH_IDS: usize = 500;

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

    // trace:TASK-247 | ai:claude
    /// The rows behind `ids`, keyed by id — one `IN (...)` SELECT per
    /// [`MAX_BATCH_IDS`] distinct ids. Absent ids simply have no entry; it is
    /// the caller that decides whether that is an error
    /// ([`DomainStore::fetch_nodes`]) or a skip
    /// ([`DomainStore::fetch_nodes_present`]).
    fn found_nodes(&self, ids: &[String]) -> Result<BTreeMap<String, NodeRecord>> {
        let distinct: Vec<&str> = ids
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut found: BTreeMap<String, NodeRecord> = BTreeMap::new();
        for chunk in distinct.chunks(MAX_BATCH_IDS) {
            for row in self.sql_json(&format!(
                "SELECT {NODE_COLUMNS} FROM nodes WHERE id IN ({});",
                sql_id_list(chunk)
            ))? {
                let record = node_from_row(&row);
                found.insert(record.id.clone(), record);
            }
        }
        Ok(found)
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
fn dolt_kind(kind: NodeKind) -> (&'static str, &'static str) {
    match kind {
        NodeKind::Question => ("question", "Q-"),
        NodeKind::Term => ("term", "TERM-"),
    }
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

// trace:STORY-244 | ai:claude
/// Materialise one `nodes` row. Shared by the per-item and set-based reads so
/// the two cannot drift in how they decode a node.
fn node_from_row(row: &serde_json::Map<String, serde_json::Value>) -> NodeRecord {
    NodeRecord {
        id: string_column(row, "id"),
        title: string_column(row, "title"),
        tags: string_column(row, "tags")
            .split(',')
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
            .map(str::to_string)
            .collect(),
        weight: u32_column(row, "weight"),
        body: string_column(row, "body"),
    }
}

// trace:STORY-244 | ai:claude
/// The `'a', 'b', 'c'` body of an `IN (...)` list.
fn sql_id_list(ids: &[&str]) -> String {
    ids.iter()
        .map(|id| sql_quote(id))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The column list every node read selects, kept in one place so the per-item
/// and set-based reads stay decodable by [`node_from_row`].
const NODE_COLUMNS: &str = "id, title, body, tags, weight";

/// The one "no such node" error text, shared so [`DomainStore::fetch_nodes`]
/// fails exactly the way looping [`DomainStore::fetch_node`] does.
fn missing_node(id: &str) -> QuizdomError {
    QuizdomError::Dolt(format!("node {id} not found in the Dolt store"))
}

impl<R> DomainStore for DoltDomainStore<R>
where
    R: DoltRunner,
{
    fn fetch_node(&self, id: &str) -> Result<NodeRecord> {
        let rows = self.sql_json(&format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE id = {};",
            sql_quote(id)
        ))?;
        rows.first()
            .map(node_from_row)
            .ok_or_else(|| missing_node(id))
    }

    fn list_node_ids(&self, kind: NodeKind) -> Result<Vec<String>> {
        let (kind_value, _) = dolt_kind(kind);
        Ok(self
            .sql_json(&format!(
                "SELECT id FROM nodes WHERE kind = '{kind_value}' ORDER BY id;"
            ))?
            .iter()
            .map(|row| string_column(row, "id"))
            .collect())
    }

    fn neighbors(&self, id: &str, edge: EdgeKind) -> Result<Vec<String>> {
        let kind = edge.as_str();
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
        let (kind_value, prefix) = dolt_kind(node.kind);
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
        self.sql_json(&format!(
            "INSERT INTO nodes (id, kind, title, body, tags, weight) \
             VALUES ({}, '{kind_value}', {}, {}, {}, {});",
            sql_quote(&id),
            sql_quote(&node.title),
            sql_quote(&node.description),
            sql_quote(&node.tags.join(",")),
            node.weight
        ))?;
        self.commit(&format!("quizdom: add node {id}"))?;
        Ok(id)
    }

    fn create_edge(&self, from: &str, to: &str, edge: EdgeKind) -> Result<()> {
        let kind = edge.as_str();
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
        let kind = edge.as_str();
        self.sql_json(&format!(
            "INSERT IGNORE INTO edges (from_id, to_id, kind) VALUES ({}, {}, '{kind}');",
            sql_quote(from),
            sql_quote(to)
        ))?;
        self.commit(&format!("quizdom: ensure edge {from} -{kind}-> {to}"))
    }

    // trace:STORY-208 | ai:claude
    fn update_weight_and_tags(&self, id: &str, weight: u32, tags: &[String]) -> Result<()> {
        self.sql_json(&format!(
            "UPDATE nodes SET tags = {}, weight = {weight} WHERE id = {};",
            sql_quote(&tags.join(",")),
            sql_quote(id)
        ))?;
        self.commit(&format!("quizdom: reweight {id}"))
    }

    /// The STORY-207 multi-hop read: one recursive CTE instead of a per-hop
    /// walk. `UNION` (not `UNION ALL`) deduplicates rows, so the walk
    /// terminates on cyclic graphs — visited-set semantics, sorted results.
    fn reachable(&self, root: &str, edge: EdgeKind) -> Result<Vec<String>> {
        let kind = edge.as_str();
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

    // trace:STORY-244 | ai:claude
    /// One `IN (...)` SELECT per [`MAX_BATCH_IDS`] distinct ids, replacing the
    /// default's spawn-per-id. Rows come back unordered, so the requested
    /// order is restored from the id index.
    fn fetch_nodes(&self, ids: &[String]) -> Result<Vec<NodeRecord>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let found = self.found_nodes(ids)?;
        ids.iter()
            .map(|id| found.get(id).cloned().ok_or_else(|| missing_node(id)))
            .collect()
    }

    // trace:TASK-247 | ai:claude
    /// The same one-`IN (...)`-per-chunk read as [`Self::fetch_nodes`], minus
    /// the missing-row check: a query error still propagates, an absent id is
    /// just absent from the result.
    fn fetch_nodes_present(&self, ids: &[String]) -> Result<Vec<NodeRecord>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let found = self.found_nodes(ids)?;
        Ok(ids.iter().filter_map(|id| found.get(id).cloned()).collect())
    }

    // trace:STORY-244 | ai:claude
    /// One kind-filtered SELECT of whole rows — the default's `list_node_ids`
    /// plus a fetch per id collapses to this.
    fn list_nodes(&self, kind: NodeKind) -> Result<Vec<NodeRecord>> {
        let (kind_value, _) = dolt_kind(kind);
        Ok(self
            .sql_json(&format!(
                "SELECT {NODE_COLUMNS} FROM nodes WHERE kind = '{kind_value}' ORDER BY id;"
            ))?
            .iter()
            .map(node_from_row)
            .collect())
    }

    // trace:STORY-244 | ai:claude
    /// One SELECT over the edge table for every source. `ORDER BY from_id`
    /// first groups the rows; within a group the trailing `created_at, to_id`
    /// is exactly [`DomainStore::neighbors`]' ordering.
    fn neighbors_many(
        &self,
        ids: &[String],
        edge: EdgeKind,
    ) -> Result<BTreeMap<String, Vec<String>>> {
        let kind = edge.as_str();
        // Seeding from `ids` makes the map total over them (and de-duplicates
        // the id list the queries chunk over).
        let mut targets: BTreeMap<String, Vec<String>> =
            ids.iter().map(|id| (id.clone(), Vec::new())).collect();
        let distinct: Vec<String> = targets.keys().cloned().collect();
        for chunk in distinct.chunks(MAX_BATCH_IDS) {
            for row in self.sql_json(&format!(
                "SELECT from_id, to_id FROM edges WHERE from_id IN ({}) AND kind = '{kind}' \
                 ORDER BY from_id, created_at, to_id;",
                sql_id_list(&chunk.iter().map(String::as_str).collect::<Vec<_>>())
            ))? {
                if let Some(entry) = targets.get_mut(&string_column(&row, "from_id")) {
                    entry.push(string_column(&row, "to_id"));
                }
            }
        }
        Ok(targets)
    }

    // trace:STORY-244 | ai:claude
    /// One multi-row `UPDATE ... CASE id WHEN ...` per [`MAX_BATCH_IDS`] ids,
    /// then a single add + commit — so re-weighting a whole bank costs three
    /// spawns instead of three per question. A `CASE` arm per id keeps this a
    /// single statement, which `dolt sql -r json -q` renders as one document.
    fn update_weights(&self, updates: &[(String, u32, Vec<String>)]) -> Result<()> {
        // Last entry per id wins, matching a loop over update_weight_and_tags.
        let mut latest: BTreeMap<&str, (u32, String)> = BTreeMap::new();
        for (id, weight, tags) in updates {
            latest.insert(id.as_str(), (*weight, tags.join(",")));
        }
        if latest.is_empty() {
            return Ok(());
        }
        let count = latest.len();
        let rows: Vec<(&str, (u32, String))> = latest.into_iter().collect();
        for chunk in rows.chunks(MAX_BATCH_IDS) {
            let mut tag_arms = String::new();
            let mut weight_arms = String::new();
            let mut ids = Vec::with_capacity(chunk.len());
            for (id, (weight, tags)) in chunk {
                let quoted = sql_quote(id);
                tag_arms.push_str(&format!(" WHEN {quoted} THEN {}", sql_quote(tags)));
                weight_arms.push_str(&format!(" WHEN {quoted} THEN {weight}"));
                ids.push(*id);
            }
            self.sql_json(&format!(
                "UPDATE nodes SET tags = CASE id{tag_arms} END, \
                 weight = CASE id{weight_arms} END WHERE id IN ({});",
                sql_id_list(&ids)
            ))?;
        }
        self.commit(&format!("quizdom: reweight {count} node(s)"))
    }
}

// trace:STORY-208 | ai:claude
// trace:TASK-228 | ai:claude — one shared chain with db-init / db-migrate.
/// Resolve the domain store from the environment and the STORY-194 settings
/// file. Since the cutover there is one backend — Dolt — and only its repo
/// path is configurable, through [`crate::settings::resolve_dolt_path`]:
/// `QUIZDOM_DOLT_PATH` (env, wins) or `dolt_path` (settings.toml), defaulting
/// to [`crate::DEFAULT_DOLT_DB_PATH`]. `db-init` / `db-migrate` call the same helper
/// (with `--path` layered on top), so the bootstrap and the runtime can no
/// longer disagree about which repo is "the" domain graph.
pub fn domain_store_from_config() -> DoltDomainStore {
    DoltDomainStore::new(crate::settings::resolve_dolt_path())
}

// trace:STORY-208 | ai:claude
/// Test double shared across modules: records every dolt invocation and
/// replays canned `(raw_status, stdout, stderr)` responses in FIFO order.
/// Cloning shares the call log, so a test can keep a handle while the store
/// owns the runner.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct ScriptedDoltRunner {
    pub(crate) calls: std::rc::Rc<std::cell::RefCell<Vec<Vec<String>>>>,
    responses: std::rc::Rc<std::cell::RefCell<Vec<(i32, String, String)>>>,
}

#[cfg(test)]
impl ScriptedDoltRunner {
    pub(crate) fn new(responses: Vec<(i32, &str, &str)>) -> Self {
        Self {
            calls: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            responses: std::rc::Rc::new(std::cell::RefCell::new(
                responses
                    .into_iter()
                    .map(|(status, out, err)| (status, out.to_string(), err.to_string()))
                    .collect(),
            )),
        }
    }

    /// The SQL text of recorded call `index`, asserting it was a query spawn.
    pub(crate) fn sql_of_call(call: &[String]) -> String {
        assert_eq!(&call[0..4], &["sql", "-r", "json", "-q"], "call: {call:?}");
        call[4].clone()
    }
}

#[cfg(test)]
impl DoltRunner for ScriptedDoltRunner {
    fn run(&self, _cwd: &std::path::Path, args: &[String]) -> Result<std::process::Output> {
        use std::os::unix::process::ExitStatusExt;
        self.calls.borrow_mut().push(args.to_vec());
        let (raw_status, stdout, stderr) = {
            let mut responses = self.responses.borrow_mut();
            if responses.is_empty() {
                (0, String::new(), String::new())
            } else {
                responses.remove(0)
            }
        };
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(raw_status),
            stdout: stdout.into_bytes(),
            stderr: stderr.into_bytes(),
        })
    }
}

// trace:STORY-207 | ai:claude
#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(responses: Vec<(i32, &str, &str)>) -> DoltDomainStore<ScriptedDoltRunner> {
        DoltDomainStore::with_runner("/tmp/quizdom-dolt", ScriptedDoltRunner::new(responses))
    }

    fn sql_of(call: &[String]) -> String {
        ScriptedDoltRunner::sql_of_call(call)
    }

    #[test]
    fn fetch_node_reads_the_weight_column_without_synthesizing_a_tag() {
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
        // STORY-208: the weight lives only in the numeric field — the tag
        // list is exactly what the tags column holds.
        assert_eq!(
            record.tags,
            ["topic:free-will", "answer:yes-no"].map(String::from)
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
            tags: ["topic:free-will"].map(String::from).to_vec(),
            weight: 55,
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
            "tags column carries only real tags: {insert}"
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
            weight: 0,
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
    fn update_weight_and_tags_writes_both_columns_in_one_update() {
        let store = store_with(vec![(0, "", ""), (0, "", ""), (0, "", "")]);
        let tags = ["topic:free-will", "quality:insightful"].map(String::from);

        store
            .update_weight_and_tags("Q-1", 62, &tags)
            .expect("reweight should succeed");

        let calls = store.runner.calls.borrow();
        let update = sql_of(&calls[0]);
        assert!(update.contains("tags = 'topic:free-will,quality:insightful'"));
        assert!(update.contains("weight = 62"));
        assert!(update.contains("WHERE id = 'Q-1'"));
    }

    // trace:STORY-244 | ai:claude — the set-based reads and writes: each one
    // is checked against the per-item method it batches, so the two paths
    // cannot drift, and against the spawn count that motivated the story.

    /// Two `nodes` rows as a set-based read yields them — deliberately *not*
    /// in requested-id order, so a test can prove who restores the order.
    const TWO_NODE_ROWS: &str = r#"{"rows":[
        {"id":"Q-2","title":"two","body":"b2","tags":"answer:yes-no","weight":40},
        {"id":"Q-1","title":"one","body":"b1","tags":"topic:x, answer:yes-no","weight":70}]}"#;

    #[test]
    fn fetch_nodes_matches_looping_fetch_node_in_one_spawn() {
        let ids = ["Q-1", "Q-2"].map(String::from).to_vec();
        let looping = store_with(vec![
            (
                0,
                r#"{"rows":[{"id":"Q-1","title":"one","body":"b1","tags":"topic:x, answer:yes-no","weight":70}]}"#,
                "",
            ),
            (
                0,
                r#"{"rows":[{"id":"Q-2","title":"two","body":"b2","tags":"answer:yes-no","weight":40}]}"#,
                "",
            ),
        ]);
        let expected: Vec<NodeRecord> = ids
            .iter()
            .map(|id| looping.fetch_node(id).expect("per-item fetch"))
            .collect();

        let batched = store_with(vec![(0, TWO_NODE_ROWS, "")]);
        let records = batched
            .fetch_nodes(&ids)
            .expect("batch fetch should succeed");

        assert_eq!(records, expected, "same records, in requested-id order");
        assert_eq!(
            looping.runner.calls.borrow().len(),
            2,
            "the loop it replaces"
        );
        let calls = batched.runner.calls.borrow();
        assert_eq!(calls.len(), 1, "one IN(...) spawn, not one per id");
        assert!(
            sql_of(&calls[0]).contains("id IN ('Q-1', 'Q-2')"),
            "{}",
            sql_of(&calls[0])
        );
    }

    #[test]
    fn fetch_nodes_errors_on_a_missing_id_like_fetch_node_does() {
        let store = store_with(vec![(
            0,
            r#"{"rows":[{"id":"Q-1","title":"one","body":"","tags":"","weight":1}]}"#,
            "",
        )]);
        match store.fetch_nodes(&["Q-1".to_string(), "Q-404".to_string()]) {
            Err(QuizdomError::Dolt(message)) => {
                assert!(message.contains("Q-404"), "{message}");
                assert!(message.contains("not found"), "{message}");
            }
            other => panic!("expected Dolt error, got {other:?}"),
        }
    }

    // trace:TASK-247 | ai:claude
    #[test]
    fn fetch_nodes_present_skips_a_missing_id_instead_of_failing_the_batch() {
        let ids = ["Q-1", "Q-404", "Q-2"].map(String::from).to_vec();
        let store = store_with(vec![(0, TWO_NODE_ROWS, "")]);

        let records = store
            .fetch_nodes_present(&ids)
            .expect("an absent id is a skip, not an error");

        assert_eq!(
            records.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["Q-1", "Q-2"],
            "the rows that exist, still in requested-id order"
        );
        let calls = store.runner.calls.borrow();
        assert_eq!(calls.len(), 1, "one IN(...) spawn, same as the strict read");
    }

    // trace:TASK-247 | ai:claude
    #[test]
    fn fetch_nodes_present_still_propagates_a_query_failure() {
        let store = store_with(vec![(1, "", "connection refused")]);

        match store.fetch_nodes_present(&["Q-1".to_string()]) {
            Err(QuizdomError::Dolt(message)) => {
                assert!(message.contains("connection refused"), "{message}");
            }
            other => panic!("a store failure is not an absent row, got {other:?}"),
        }
    }

    #[test]
    fn empty_batches_do_no_work_at_all() {
        let store = store_with(Vec::new());

        assert!(store.fetch_nodes(&[]).expect("empty fetch").is_empty());
        // trace:TASK-247 | ai:claude
        assert!(store
            .fetch_nodes_present(&[])
            .expect("empty lenient fetch")
            .is_empty());
        assert!(store
            .neighbors_many(&[], EdgeKind::Begets)
            .expect("empty neighbors")
            .is_empty());
        store.update_weights(&[]).expect("empty write");

        assert!(
            store.runner.calls.borrow().is_empty(),
            "no ids, no spawns — matching a loop over an empty slice"
        );
    }

    #[test]
    fn list_nodes_reads_whole_rows_in_one_select() {
        let store = store_with(vec![(0, TWO_NODE_ROWS, "")]);

        let nodes = store
            .list_nodes(NodeKind::Question)
            .expect("listing should succeed");

        // Decoding is shared with fetch_node, so the records are complete.
        assert_eq!(nodes[1].id, "Q-1");
        assert_eq!(nodes[1].title, "one");
        assert_eq!(nodes[1].body, "b1");
        assert_eq!(nodes[1].weight, 70);
        assert_eq!(
            nodes[1].tags,
            ["topic:x", "answer:yes-no"].map(String::from)
        );
        let calls = store.runner.calls.borrow();
        assert_eq!(calls.len(), 1, "one select, no fetch per id");
        let sql = sql_of(&calls[0]);
        assert!(sql.contains("kind = 'question'"), "{sql}");
        assert!(sql.contains("ORDER BY id"), "{sql}");
    }

    #[test]
    fn neighbors_many_matches_looping_neighbors_in_one_spawn() {
        let ids = ["Q-1", "Q-2", "Q-3"].map(String::from).to_vec();
        // TASK-221: same-second edges come back in to_id order, so Q-10
        // follows Q-9 — the batched read must preserve that per source.
        let looping = store_with(vec![
            (0, r#"{"rows":[{"to_id":"Q-9"},{"to_id":"Q-10"}]}"#, ""),
            (0, r#"{"rows":[]}"#, ""),
            (0, r#"{"rows":[{"to_id":"Q-4"}]}"#, ""),
        ]);
        let expected: BTreeMap<String, Vec<String>> = ids
            .iter()
            .map(|id| {
                (
                    id.clone(),
                    looping.neighbors(id, EdgeKind::Begets).expect("per-item"),
                )
            })
            .collect();

        let batched = store_with(vec![(
            0,
            r#"{"rows":[
                {"from_id":"Q-1","to_id":"Q-9"},
                {"from_id":"Q-1","to_id":"Q-10"},
                {"from_id":"Q-3","to_id":"Q-4"}]}"#,
            "",
        )]);
        let targets = batched
            .neighbors_many(&ids, EdgeKind::Begets)
            .expect("batch neighbors should succeed");

        assert_eq!(targets, expected);
        assert!(
            targets["Q-2"].is_empty(),
            "the map is total over its inputs"
        );
        let calls = batched.runner.calls.borrow();
        assert_eq!(calls.len(), 1, "one spawn, not one per source");
        let sql = sql_of(&calls[0]);
        assert!(sql.contains("from_id IN ('Q-1', 'Q-2', 'Q-3')"), "{sql}");
        assert!(sql.contains("kind = 'begets'"), "{sql}");
        assert!(sql.contains("ORDER BY from_id, created_at, to_id"), "{sql}");
    }

    #[test]
    fn update_weights_writes_every_row_in_one_update_and_one_commit() {
        let store = store_with(vec![(0, "", ""), (0, "", ""), (0, "", "")]);
        let updates = vec![
            (
                "Q-1".to_string(),
                62,
                ["topic:x", "quality:insightful"].map(String::from).to_vec(),
            ),
            (
                "Q-2".to_string(),
                30,
                ["quality:unhelpful"].map(String::from).to_vec(),
            ),
        ];

        store
            .update_weights(&updates)
            .expect("batch reweight should succeed");

        let calls = store.runner.calls.borrow();
        assert_eq!(
            calls.len(),
            3,
            "one UPDATE + one add + one commit, not three per row"
        );
        let update = sql_of(&calls[0]);
        assert!(
            update.contains(
                "tags = CASE id WHEN 'Q-1' THEN 'topic:x,quality:insightful' \
                 WHEN 'Q-2' THEN 'quality:unhelpful' END"
            ),
            "{update}"
        );
        assert!(
            update.contains("weight = CASE id WHEN 'Q-1' THEN 62 WHEN 'Q-2' THEN 30 END"),
            "{update}"
        );
        assert!(update.contains("WHERE id IN ('Q-1', 'Q-2')"), "{update}");
        assert_eq!(calls[1], ["add", "-A"]);
        assert_eq!(&calls[2][0..2], &["commit", "-m"]);
    }

    #[test]
    fn update_weights_takes_the_last_entry_for_a_repeated_id() {
        let store = store_with(vec![(0, "", ""), (0, "", ""), (0, "", "")]);
        let updates = vec![
            (
                "Q-1".to_string(),
                10,
                ["quality:unhelpful"].map(String::from).to_vec(),
            ),
            (
                "Q-1".to_string(),
                90,
                ["quality:insightful"].map(String::from).to_vec(),
            ),
        ];

        store.update_weights(&updates).expect("reweight");

        let calls = store.runner.calls.borrow();
        let update = sql_of(&calls[0]);
        assert!(update.contains("THEN 90"), "last write wins: {update}");
        assert!(!update.contains("THEN 10"), "{update}");
    }

    // The path-resolution chain moved to settings.rs with TASK-228 (one helper
    // shared with db-init / db-migrate); its tests live there now.

    /// The STORY-207/208 acceptance check against a real dolt binary:
    /// bootstrap a fixture repo and run the full trait surface against it,
    /// including a 3+ hop recursive-CTE traversal. Ignored in CI (no dolt
    /// there); run locally with: cargo test real_dolt -- --ignored
    #[test]
    #[ignore = "requires the dolt binary on PATH"]
    fn real_dolt_full_trait_surface() {
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
                    tags: ["topic:free-will".to_string()].to_vec(),
                    weight: 70,
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
                weight: 0,
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
        assert_eq!(root.tags, ["topic:free-will".to_string()]);
        assert_eq!(
            store.list_node_ids(NodeKind::Question).unwrap(),
            chain,
            "chain ids in order"
        );
        assert_eq!(
            store.neighbors(&chain[0], EdgeKind::Begets).unwrap(),
            [chain[1].clone()]
        );

        // The reweight write path: weight column + rewritten tags together.
        store
            .update_weight_and_tags(
                &chain[0],
                41,
                &[
                    "topic:free-will".to_string(),
                    "quality:unhelpful".to_string(),
                ],
            )
            .expect("reweight should succeed");
        assert_eq!(store.fetch_node(&chain[0]).unwrap().weight, 41);

        // Acceptance: the 3-hop traversal from the root via one recursive
        // CTE reaches the whole begets chain and nothing else.
        let reached = store
            .reachable(&chain[0], EdgeKind::Begets)
            .expect("CTE traversal should succeed");
        let mut expected = chain.clone();
        expected.sort();
        assert_eq!(reached, expected);
        assert!(!reached.contains(&term), "probes spur not walked");

        // trace:STORY-244 — against a real dolt, every set-based method agrees
        // with the loop over the per-item method it replaces.
        let ids = store.list_node_ids(NodeKind::Question).unwrap();
        let looped: Vec<NodeRecord> = ids
            .iter()
            .map(|id| store.fetch_node(id).expect("per-item fetch"))
            .collect();
        assert_eq!(store.fetch_nodes(&ids).unwrap(), looped, "batch fetch");
        assert_eq!(
            store.list_nodes(NodeKind::Question).unwrap(),
            looped,
            "one-query listing"
        );
        let looped_edges: BTreeMap<String, Vec<String>> = ids
            .iter()
            .map(|id| (id.clone(), store.neighbors(id, EdgeKind::Begets).unwrap()))
            .collect();
        assert_eq!(
            store.neighbors_many(&ids, EdgeKind::Begets).unwrap(),
            looped_edges,
            "batch neighbors, per-source ordering intact"
        );

        // The batched write lands on every row it names.
        store
            .update_weights(&[
                (chain[1].clone(), 12, ["batched".to_string()].to_vec()),
                (chain[2].clone(), 88, ["batched".to_string()].to_vec()),
            ])
            .expect("batched reweight should succeed");
        let after = store
            .fetch_nodes(&[chain[1].clone(), chain[2].clone()])
            .unwrap();
        assert_eq!(
            after.iter().map(|node| node.weight).collect::<Vec<_>>(),
            [12, 88]
        );
        assert_eq!(after[0].tags, ["batched".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
