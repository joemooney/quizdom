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
//! `dolt add nodes edges` + `dolt commit`, so the domain graph's history lives
//! in Dolt
//! itself. Multi-hop traversal is a single recursive CTE
//! ([`DomainStore::reachable`]) — ADR-31's app-side per-hop walk is gone.
//!
//! Contradiction-resolution *decision* nodes and their `references` edges are
//! not domain data: ADR-201 keeps decision/intent objects in the AIDA store,
//! written through [`crate::store::AidaIntentStore`].

use crate::db_init::{DoltRunner, SystemDoltRunner};
use crate::db_migrate::{sql_quote, SQL_BATCH_BUDGET};
use crate::error::{QuizdomError, Result};
use crate::store::{DomainStore, EdgeKind, NewNode, NodeKind, NodeRecord};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Output;
use std::sync::atomic::{AtomicBool, Ordering};

// trace:STORY-244 | ai:claude
/// How many ids ride in one `IN (...)` list or one batched `UPDATE`, so a bank
/// far larger than this costs one extra spawn per chunk rather than one per
/// row. This is a row-count cap only — what bounds the *SQL text* is
/// [`SQL_BATCH_BUDGET`], applied alongside it by [`chunk_by_sql_bytes`].
const MAX_BATCH_IDS: usize = 500;

// trace:TASK-248 | ai:claude
/// Split `items` into chunks bounded by both caps: at most [`MAX_BATCH_IDS`]
/// items, and at most [`SQL_BATCH_BUDGET`] bytes of the SQL they will build
/// (`sql_bytes` per item).
///
/// The count cap alone is not enough: for [`DomainStore::update_weights`] the
/// dominant term is the tags payload, not the ids, and `nodes.tags` is
/// `VARCHAR(2048)` — so 500 wide rows would be ~1 MB of SQL in a single argv
/// element, which `execve` refuses with `E2BIG` (an opaque spawn failure, not
/// a SQL error). Bounding on bytes turns that into one extra spawn.
///
/// An item whose own SQL exceeds the budget still ships, alone — matching
/// [`crate::db_migrate`]'s statement chunker rather than looping forever.
fn chunk_by_sql_bytes<T>(items: &[T], sql_bytes: impl Fn(&T) -> usize) -> Vec<&[T]> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut bytes = 0;
    for (index, item) in items.iter().enumerate() {
        let cost = sql_bytes(item);
        if index > start && (index - start >= MAX_BATCH_IDS || bytes + cost > SQL_BATCH_BUDGET) {
            chunks.push(&items[start..index]);
            start = index;
            bytes = 0;
        }
        bytes += cost;
    }
    if start < items.len() {
        chunks.push(&items[start..]);
    }
    chunks
}

// trace:TASK-248 | ai:claude
/// What one id costs inside an `IN (...)` list: its quoted literal plus the
/// `, ` joining it to the next.
fn id_list_bytes(id: &str) -> usize {
    sql_quote(id).len() + 2
}

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

    // trace:TASK-280 | ai:claude
    /// Every dolt spawn this store makes, through the shared tripwire.
    ///
    /// The guard sits at the SPAWN, not at the constructor. The store is built
    /// eagerly by several `Default` impls (`persist.rs`, `contradiction.rs`),
    /// so dozens of offline session tests hold a real-path store they never run
    /// a query through — and a store that is never asked anything cannot poison
    /// anything. What must not happen is a test reaching the developer's actual
    /// `data/dolt` with a real `dolt sql`, and that is exactly here.
    fn run_dolt(&self, args: &[String]) -> Result<Output> {
        crate::db_init::guard_test_path("the domain-graph path", &self.path);
        self.runner.run(&self.path, args)
    }

    /// The single query choke point: `dolt sql -r json -q <sql>`, parsed into
    /// the `rows` array of the JSON result format.
    fn sql_json(&self, sql: &str) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
        let args: Vec<String> = ["sql", "-r", "json", "-q", sql]
            .into_iter()
            .map(String::from)
            .collect();
        let output = self.run_dolt(&args)?;
        if !output.status.success() {
            return Err(QuizdomError::Dolt(format!(
                "dolt sql failed: {}",
                // trace:TASK-279 | ai:claude
                crate::db_init::clean_dolt_message(&String::from_utf8_lossy(&output.stderr))
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
        // trace:TASK-248 | ai:claude — bounded by id count *and* SQL bytes.
        for chunk in chunk_by_sql_bytes(&distinct, |id| id_list_bytes(id)) {
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

    // trace:STORY-291 | ai:claude — one commit tail, shared with db-init /
    // db-migrate now that they commit their own writes too.
    /// Stage and commit a completed write. A no-op write (e.g. an idempotent
    /// [`DomainStore::ensure_edge`] hitting an existing row) leaves nothing to
    /// commit — dolt's refusal for that case is success here.
    ///
    // trace:TASK-297 | ai:claude — stages `nodes` / `edges` by name, never
    // `-A`, so a hand-made table in the working set is not swept into a commit
    // message about a session answer.
    fn commit(&self, message: &str) -> Result<()> {
        // trace:TASK-280 | ai:claude — the commit tail spawns dolt directly
        // rather than through `run_dolt`, so it takes the tripwire itself.
        crate::db_init::guard_test_path("the domain-graph path", &self.path);
        crate::db_init::commit_tables(
            &self.runner,
            &self.path,
            crate::db_init::QUIZDOM_TABLES,
            message,
        )
        .map(|committed| {
            // trace:STORY-299 | ai:claude
            if committed {
                GRAPH_WRITTEN.store(true, Ordering::Relaxed);
            }
        })
    }
}

// trace:STORY-299 | ai:claude
/// Set once this process has COMMITTED at least one domain-graph write.
///
/// A process-wide flag rather than a value threaded through the session, for
/// two reasons. It is a property of the PROCESS (the durability question is
/// "did this run of quizdom move the graph?"), and the writers are half a dozen
/// independently-constructed `Default` persisters — `persist.rs`,
/// `contradiction.rs`, `bank.rs` — each holding its own store, so there is no
/// single object to hang it on and threading one would mean widening every
/// persister trait for a boolean.
///
/// It is set HERE because [`DoltDomainStore::commit`] is the choke point every
/// write already passes through, and set only when a commit was actually
/// created: an idempotent `ensure_edge` that changed nothing leaves the graph
/// where it was and must not trigger a backup reminder.
static GRAPH_WRITTEN: AtomicBool = AtomicBool::new(false);

/// Whether this process has committed a domain-graph write — the "a session
/// that wrote to the graph" half of [`crate::db_backup::session_end_durability`].
pub(crate) fn graph_written_this_process() -> bool {
    GRAPH_WRITTEN.load(Ordering::Relaxed)
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

// trace:TASK-248 | ai:claude
/// One row's contribution to the batched re-weight `UPDATE`: its quoted id
/// and the two `CASE` arms naming it. Built ahead of chunking so the chunker
/// measures real SQL rather than guessing from the id count.
struct CaseArms {
    id: String,
    tag_arm: String,
    weight_arm: String,
}

impl CaseArms {
    /// Everything this row adds to the statement: both arms plus its entry in
    /// the trailing `IN (...)` list.
    fn sql_bytes(&self) -> usize {
        self.tag_arm.len() + self.weight_arm.len() + self.id.len() + 2
    }
}

// trace:TASK-224 | ai:claude
// trace:STORY-291 | ai:claude — named "default" because that is all quizdom
// knows: nothing here sets the variable, so this is the engine's own default
// and the error says so rather than claiming a limit it configured.
/// Dolt inherits MySQL's `cte_max_recursion_depth`, whose default is 1000
/// iterations — the ceiling on how deep [`DomainStore::reachable`]'s recursive
/// CTE can walk before the engine aborts it.
const CTE_MAX_RECURSION_DEPTH: u32 = 1000;

// trace:TASK-224 | ai:claude
/// Whether a dolt failure is the recursion-depth abort rather than a real
/// query error. Matched loosely on purpose: MySQL says "Recursive query
/// aborted after N iterations", go-mysql-server says the iteration limit was
/// exceeded, and neither wording is a stable API.
fn is_cte_depth_failure(message: &str) -> bool {
    let text = message.to_ascii_lowercase();
    text.contains("recursi")
        && (text.contains("depth") || text.contains("iteration") || text.contains("limit"))
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
///
// trace:STORY-293 | ai:claude — `NotFound`, not `Dolt`: an absent row is the
// store answering, not the store failing, and the
// [`DomainStore::fetch_nodes_present`] default keys its skip off exactly that
// distinction.
// trace:TASK-318 | ai:claude — which is why the variant is no longer chosen
// here: [`crate::store::missing_node`] is the shared constructor that fixes it,
// so this backend owns only the operator-facing wording.
fn missing_node(id: &str) -> QuizdomError {
    crate::store::missing_node(id, "Dolt store")
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

    // trace:TASK-224 | ai:claude — name the depth limit instead of leaking it.
    // trace:STORY-291 | ai:claude — and advise something the spawn model can
    // actually do.
    /// The STORY-207 multi-hop read: one recursive CTE instead of a per-hop
    /// walk. `UNION` (not `UNION ALL`) deduplicates rows, so the walk
    /// terminates on cyclic graphs — visited-set semantics, sorted results.
    ///
    /// Pushing the walk into the engine means inheriting the engine's ceiling:
    /// a chain longer than [`CTE_MAX_RECURSION_DEPTH`] aborts mid-traversal.
    /// That is not reachable by the current domain graph, but when it happens
    /// the caller gets a quizdom error naming the limit, not a raw engine
    /// string about CTE iterations.
    ///
    /// What that error may *advise* is constrained by ADR-203: every query is
    /// its own `dolt sql` spawn, so a `SET SESSION cte_max_recursion_depth` the
    /// user runs in their own shell dies with that shell and never reaches
    /// quizdom's next spawn. The remedies that do work are walking a shorter
    /// chain, or quizdom sending the `SET` in the same statement batch as the
    /// CTE — which is a change here, not something the user can do from
    /// outside. The error says exactly that.
    fn reachable(&self, root: &str, edge: EdgeKind) -> Result<Vec<String>> {
        let kind = edge.as_str();
        let rows = self
            .sql_json(&format!(
                "WITH RECURSIVE reachable (id) AS (\
                 SELECT CAST({quoted_root} AS CHAR(64)) \
                 UNION \
                 SELECT e.to_id FROM edges e JOIN reachable r ON e.from_id = r.id \
                 WHERE e.kind = '{kind}') \
                 SELECT id FROM reachable ORDER BY id;",
                quoted_root = sql_quote(root)
            ))
            .map_err(|error| match &error {
                QuizdomError::Dolt(message) if is_cte_depth_failure(message) => {
                    QuizdomError::Dolt(format!(
                        "traversal from {root} over {kind} edges exceeded Dolt's default \
                         recursive-CTE depth limit of {CTE_MAX_RECURSION_DEPTH} hops \
                         (cte_max_recursion_depth). quizdom runs each query in its own \
                         `dolt sql` spawn (ADR-203), so setting that variable in your own \
                         shell cannot reach it: traverse from a node further down the \
                         chain, or raise the ceiling in quizdom itself by sending \
                         `SET SESSION cte_max_recursion_depth = N` in the same statement \
                         batch as the CTE. Engine detail: {message}"
                    ))
                }
                _ => error,
            })?;
        Ok(rows.iter().map(|row| string_column(row, "id")).collect())
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
        // trace:TASK-248 | ai:claude — bounded by id count *and* SQL bytes.
        for chunk in chunk_by_sql_bytes(&distinct, |id| id_list_bytes(id)) {
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
    // trace:TASK-248 | ai:claude — chunked on SQL bytes, not just id count.
    /// One multi-row `UPDATE ... CASE id WHEN ...` per chunk, then a single
    /// add + commit — so re-weighting a whole bank costs three spawns instead
    /// of three per question. A `CASE` arm per id keeps this a single
    /// statement, which `dolt sql -r json -q` renders as one document.
    ///
    /// The arms are built before the chunking so [`chunk_by_sql_bytes`] can
    /// bound a chunk by the bytes it will actually hand to `dolt`: here the
    /// tags payload, not the id count, is what can blow past `MAX_ARG_STRLEN`.
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
        let rows: Vec<CaseArms> = latest
            .into_iter()
            .map(|(id, (weight, tags))| {
                let quoted = sql_quote(id);
                CaseArms {
                    tag_arm: format!(" WHEN {quoted} THEN {}", sql_quote(&tags)),
                    weight_arm: format!(" WHEN {quoted} THEN {weight}"),
                    id: quoted,
                }
            })
            .collect();
        for chunk in chunk_by_sql_bytes(&rows, CaseArms::sql_bytes) {
            let tag_arms: String = chunk.iter().map(|row| row.tag_arm.as_str()).collect();
            let weight_arms: String = chunk.iter().map(|row| row.weight_arm.as_str()).collect();
            let ids = chunk
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            self.sql_json(&format!(
                "UPDATE nodes SET tags = CASE id{tag_arms} END, \
                 weight = CASE id{weight_arms} END WHERE id IN ({ids});"
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
    #[cfg(not(test))]
    let path = crate::settings::resolve_dolt_path();
    // trace:TASK-280 | ai:claude
    #[cfg(test)]
    let path = test_only_domain_graph_path();
    DoltDomainStore::new(path)
}

// trace:TASK-280 | ai:claude
/// Where a test's store points instead of the resolved real graph.
///
/// This constructor is the one with no `--path` escape hatch: `db-init`,
/// `db-migrate` and `db-backup` all take a flag, so their tests pin a temp
/// directory and always did. The store does not — it is built eagerly by the
/// `Default` impls in `persist.rs` / `contradiction.rs`, so every session test
/// that runs the loop was aiming `dolt sql` at whatever
/// [`crate::settings::resolve_dolt_path`] returned. Under `cargo test` that
/// resolves relative to the crate directory rather than the project root, so it
/// has been landing on a `crates/quizdom/data/dolt` that does not exist and
/// failing gracefully — a near miss, not a hit, and only by accident of the
/// working directory.
///
/// Making the redirect structural is the point of TASK-280: no test can reach
/// the real graph through the settings chain, so none of them has to remember
/// not to. The path deliberately does not exist — these tests want the offline
/// degrade they have always had, just aimed somewhere that could never matter.
#[cfg(test)]
fn test_only_domain_graph_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "quizdom-tests-never-a-real-graph-{}",
        std::process::id()
    ))
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

    // trace:TASK-280 | ai:claude
    /// The tripwire, live on the store. It guards the SPAWN rather than the
    /// constructor — holding a real-path store costs nothing, asking it a
    /// question is what would read (and, on a write, rewrite) the developer's
    /// actual graph.
    #[test]
    #[should_panic(expected = "BUG-277 tripwire")]
    fn querying_a_store_outside_the_temp_directory_trips_the_guard() {
        let store = DoltDomainStore::with_runner(
            "/var/lib/quizdom-must-never-be-touched",
            ScriptedDoltRunner::new(vec![]),
        );
        let _ = store.fetch_node("Q-1");
    }

    // trace:TASK-280 | ai:claude
    /// And the settings chain cannot hand a test the real graph in the first
    /// place: the one constructor with no `--path` escape hatch is redirected
    /// into the temp directory for the whole suite.
    #[test]
    fn the_config_resolved_store_never_points_at_a_real_graph() {
        let path = domain_store_from_config().path;
        assert!(
            path.starts_with(std::env::temp_dir()),
            "{} escaped the temp directory",
            path.display()
        );
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
            // trace:STORY-293 | ai:claude — an absent row is `NotFound`, the
            // variant `fetch_nodes_present`'s default skips on.
            Err(QuizdomError::NotFound(message)) => assert!(message.contains("not found")),
            other => panic!("expected a not-found error, got {other:?}"),
        }
    }

    // trace:TASK-223 | ai:claude
    /// weight 0 and weight N are the same shape on the way out. The
    /// asymmetry this closes was ADR-22-era: `weight:0` was consumed off the
    /// tag list on write and then not synthesized back, so an unweighted node
    /// round-tripped differently from a weighted one. Since STORY-208 the
    /// weight is a column and tags are only tags — assert that, so a future
    /// change that reintroduces tag-encoded weight fails here.
    #[test]
    fn weight_zero_and_weight_n_round_trip_the_same_shape() {
        const ROWS: &str = r#"{"rows":[
            {"id":"Q-0","title":"unweighted","body":"b","tags":"topic:x","weight":0},
            {"id":"Q-5","title":"weighted","body":"b","tags":"topic:x","weight":5}]}"#;
        let per_item = store_with(vec![
            (
                0,
                r#"{"rows":[{"id":"Q-0","title":"unweighted","body":"b","tags":"topic:x","weight":0}]}"#,
                "",
            ),
            (
                0,
                r#"{"rows":[{"id":"Q-5","title":"weighted","body":"b","tags":"topic:x","weight":5}]}"#,
                "",
            ),
        ]);

        let unweighted = per_item.fetch_node("Q-0").expect("fetch weight-0 node");
        let weighted = per_item.fetch_node("Q-5").expect("fetch weight-5 node");

        assert_eq!(unweighted.weight, 0);
        assert_eq!(weighted.weight, 5);
        assert_eq!(
            unweighted.tags, weighted.tags,
            "identical tag columns decode identically at either weight"
        );
        assert_eq!(
            unweighted.tags,
            ["topic:x".to_string()],
            "no weight:N synthesized in, none dropped out"
        );

        // The set-based read decodes both the same way (shared node_from_row).
        let batched = store_with(vec![(0, ROWS, "")]);
        assert_eq!(
            batched
                .fetch_nodes(&["Q-0".to_string(), "Q-5".to_string()])
                .expect("batch fetch"),
            [unweighted, weighted]
        );
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

    // trace:TASK-221 | ai:claude
    /// The documented ordering contract: `created_at` is a 1-second TIMESTAMP,
    /// so same-second edges are ordered entirely by the `to_id` tie-break —
    /// lexically, which puts `Q-10` ahead of `Q-2`. The clause is what makes
    /// that true, and the store passes the engine's order through untouched.
    /// `real_dolt_full_trait_surface` proves it against a real dolt.
    #[test]
    fn neighbors_same_second_edges_tie_break_lexically_on_to_id() {
        let store = store_with(vec![(
            0,
            r#"{"rows":[{"to_id":"Q-10"},{"to_id":"Q-2"},{"to_id":"Q-9"}]}"#,
            "",
        )]);

        let targets = store
            .neighbors("Q-1", EdgeKind::Begets)
            .expect("neighbors should succeed");

        assert_eq!(
            targets,
            ["Q-10", "Q-2", "Q-9"].map(String::from),
            "lexical, not numeric — and not re-sorted by the store"
        );
        let calls = store.runner.calls.borrow();
        assert!(
            sql_of(&calls[0]).contains("ORDER BY created_at, to_id"),
            "the tie-break is in the query: {}",
            sql_of(&calls[0])
        );
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

    // trace:TASK-224 | ai:claude
    #[test]
    fn reachable_names_the_recursion_depth_limit_instead_of_leaking_it() {
        let store = store_with(vec![(
            1 << 8,
            "",
            "error analyzing query: recursive query aborted after 1001 iterations; \
             try increasing @@cte_max_recursion_depth",
        )]);

        match store.reachable("Q-1", EdgeKind::Begets) {
            Err(QuizdomError::Dolt(message)) => {
                assert!(message.contains("Q-1"), "names the root: {message}");
                assert!(
                    message.contains("cte_max_recursion_depth"),
                    "names the limit: {message}"
                );
                assert!(
                    message.contains(&CTE_MAX_RECURSION_DEPTH.to_string()),
                    "names the depth: {message}"
                );
                // trace:STORY-291 | ai:claude — the advice has to be reachable
                // from where the user stands. Under ADR-203 each query is its
                // own spawn, so "raise the session variable" was advice nobody
                // could follow; the message must say so and name a remedy that
                // works.
                assert!(
                    message.contains("default"),
                    "the limit is the engine's default, not one quizdom set: {message}"
                );
                assert!(
                    message.contains("spawn"),
                    "explains why a shell-set variable cannot reach it: {message}"
                );
                assert!(
                    message.contains("traverse from a node further down the chain"),
                    "offers a remedy the per-spawn model allows: {message}"
                );
            }
            other => panic!("expected a depth-limit error, got {other:?}"),
        }
    }

    // trace:TASK-224 | ai:claude
    #[test]
    fn reachable_leaves_an_unrelated_dolt_failure_alone() {
        let store = store_with(vec![(1 << 8, "", "table 'edges' does not exist")]);

        match store.reachable("Q-1", EdgeKind::Begets) {
            Err(QuizdomError::Dolt(message)) => {
                assert!(message.contains("does not exist"), "{message}");
                assert!(
                    !message.contains("cte_max_recursion_depth"),
                    "not every failure is a depth failure: {message}"
                );
            }
            other => panic!("expected the raw Dolt error, got {other:?}"),
        }
    }

    #[test]
    fn create_node_mints_the_next_id_and_commits() {
        let store = store_with(vec![
            (0, r#"{"rows":[{"id":"Q-7"},{"id":"Q-3"}]}"#, ""), // max scan
            (0, "", ""),                                        // insert
            (0, "", ""),                                        // add nodes edges
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
        // trace:TASK-297 | ai:claude — the tables quizdom owns, by name.
        assert_eq!(calls[2], ["add", "nodes", "edges"]);
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
            (0, "", ""),                       // add nodes edges
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
            Err(QuizdomError::NotFound(message)) => {
                assert!(message.contains("Q-404"), "{message}");
                assert!(message.contains("not found"), "{message}");
            }
            other => panic!("expected a not-found error, got {other:?}"),
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

    // trace:TASK-318 | ai:claude
    // trace:STORY-327 | ai:claude — the Dolt backend's side of the absent-node
    // invariant, asserted through the shared conformance check rather than by
    // re-describing it here. The three scripted empty results answer the check's
    // three reads (`fetch_node`, `fetch_nodes_present`, `fetch_nodes`).
    #[test]
    fn the_dolt_backend_satisfies_the_absence_contract() {
        let store = store_with(vec![
            (0, r#"{"rows":[]}"#, ""),
            (0, r#"{"rows":[]}"#, ""),
            (0, r#"{"rows":[]}"#, ""),
        ]);

        crate::store::assert_absence_contract(&store, "Q-404");
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
        assert_eq!(calls[1], ["add", "nodes", "edges"]);
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

    // trace:TASK-248 | ai:claude — the chunker splits on both caps, and the
    // write path that can actually reach the byte cap is checked against the
    // execve limit that motivated it.

    /// Linux's cap on a *single* argv element (32 pages) — the limit a chunk
    /// of SQL must stay under, separate from and far below the ~2 MB `ARG_MAX`
    /// for the whole command line. Exceeding it fails `execve` with `E2BIG`.
    const MAX_ARG_STRLEN: usize = 128 * 1024;

    #[test]
    fn chunk_by_sql_bytes_splits_on_whichever_cap_binds_first() {
        // Count cap: cheap items, so only MAX_BATCH_IDS can bind.
        let cheap: Vec<usize> = (0..MAX_BATCH_IDS * 2 + 1).collect();
        let by_count = chunk_by_sql_bytes(&cheap, |_| 1);
        assert_eq!(by_count.len(), 3);
        assert_eq!(by_count[0].len(), MAX_BATCH_IDS);
        assert_eq!(by_count[2].len(), 1);

        // Byte cap: few items, each a quarter of the budget.
        let wide: Vec<usize> = (0..8).collect();
        let by_bytes = chunk_by_sql_bytes(&wide, |_| SQL_BATCH_BUDGET / 4);
        assert_eq!(by_bytes.len(), 2, "four per chunk fills the budget exactly");
        assert_eq!(by_bytes[0].len(), 4);

        // An item bigger than the whole budget still ships, alone.
        let oversized = chunk_by_sql_bytes(&[0, 1], |_| SQL_BATCH_BUDGET * 2);
        assert_eq!(oversized.len(), 2);

        assert!(chunk_by_sql_bytes(&[0u8; 0], |_| 1).is_empty(), "no items");
    }

    #[test]
    fn update_weights_splits_a_wide_tags_batch_below_the_argv_limit() {
        let store = store_with(Vec::new());
        // Worst case for the id-count cap: far fewer than MAX_BATCH_IDS rows,
        // but each carrying a full VARCHAR(2048) tags column. Chunking on ids
        // alone would ship all of this as one ~200 KB argv element.
        let wide_tags = "t".repeat(2048);
        let updates: Vec<(String, u32, Vec<String>)> = (0..100)
            .map(|index| (format!("Q-{index}"), 50, vec![wide_tags.clone()]))
            .collect();

        store
            .update_weights(&updates)
            .expect("a wide batch is chunked, not refused");

        let calls = store.runner.calls.borrow();
        let statements: Vec<String> = calls
            .iter()
            .filter(|call| call[0] == "sql")
            .map(|call| sql_of(call))
            .collect();
        assert!(
            statements.len() > 1,
            "the byte cap bound before the id cap: {} statement(s)",
            statements.len()
        );
        for sql in &statements {
            assert!(
                sql.len() < MAX_ARG_STRLEN,
                "one argv element stayed under E2BIG: {} bytes",
                sql.len()
            );
        }
        // Every row still lands exactly once, across the chunks.
        for (id, _, _) in &updates {
            let arm = format!(" WHEN '{id}' THEN ");
            let hits: usize = statements.iter().map(|sql| sql.matches(&arm).count()).sum();
            assert_eq!(hits, 2, "one tags arm + one weight arm for {id}");
        }
        // Still one add + one commit for the whole batch, not one per chunk.
        assert_eq!(calls[calls.len() - 2], ["add", "nodes", "edges"]);
        assert_eq!(&calls[calls.len() - 1][0..2], &["commit", "-m"]);
        assert_eq!(
            calls.iter().filter(|call| call[0] == "commit").count(),
            1,
            "chunking is a spawn detail, not extra history"
        );
    }

    // The path-resolution chain moved to settings.rs with TASK-228 (one helper
    // shared with db-init / db-migrate); its tests live there now.

    /// The STORY-207/208 acceptance check against a real dolt binary:
    /// bootstrap a fixture repo and run the full trait surface against it,
    /// including a 3+ hop recursive-CTE traversal. `#[ignore]`d so a plain
    /// `cargo test` never needs a dolt binary; CI installs dolt and runs the
    /// `real_dolt` family explicitly (TASK-219), as does:
    /// cargo test real_dolt -- --ignored
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

        // trace:TASK-318 | ai:claude — the absent-node invariant against a real
        // dolt, not only against scripted rows: an id no row holds must come
        // back as `NotFound` from the per-item read, be skipped by the lenient
        // batch read, and still fail the strict one.
        crate::store::assert_absence_contract(&store, "Q-999999");

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

        // trace:TASK-221 | ai:claude — the same-second tie-break, for real.
        // The edges go in as ONE statement so they share a created_at: that is
        // the case the 1-second TIMESTAMP cannot separate, and the case a
        // per-edge loop of create_edge (a dolt commit each) could not stage
        // reliably.
        //
        // trace:STORY-291 | ai:claude — the targets are named here rather than
        // minted by create_node. Minted ids made the lexical/insertion straddle
        // an accident of how many questions the test happened to create first
        // (Q-5..Q-11 straddles; a decade-aligned Q-20..Q-26 would not), so
        // unrelated fixture edits could turn the assert_ne guard below into a
        // failure that looked like an ordering regression. Written out, the
        // straddle is structural. The `Q-fan-*` suffix does not parse as a
        // number, so create_node's max-suffix mint ignores these rows entirely.
        let fan_root = chain[0].clone();
        let targets: Vec<String> = ["Q-fan-3", "Q-fan-1", "Q-fan-7", "Q-fan-5", "Q-fan-2"]
            .map(String::from)
            .to_vec();
        let fan_rows: Vec<String> = targets
            .iter()
            .map(|id| format!("({}, 'question', 'fan', '', '', 0)", sql_quote(id)))
            .collect();
        store
            .sql_json(&format!(
                "INSERT INTO nodes (id, kind, title, body, tags, weight) VALUES {};",
                fan_rows.join(", ")
            ))
            .expect("fan node insert should succeed");
        let values: Vec<String> = targets
            .iter()
            .map(|target| {
                format!(
                    "({}, {}, 'refines')",
                    sql_quote(&fan_root),
                    sql_quote(target)
                )
            })
            .collect();
        store
            .sql_json(&format!(
                "INSERT INTO edges (from_id, to_id, kind) VALUES {};",
                values.join(", ")
            ))
            .expect("same-second edge insert should succeed");
        let mut lexical = targets.clone();
        lexical.sort();
        assert_ne!(
            lexical, targets,
            "the fixture is only meaningful if lexical != insertion order — with \
             the ids written out above, a failure here means someone reordered \
             them into sorted order, not that the ordering contract regressed"
        );
        // A kind with no earlier edge from this root, so created_at cannot
        // separate anything and the assertion is purely about the tie-break.
        assert_eq!(
            store.neighbors(&fan_root, EdgeKind::Refines).unwrap(),
            lexical,
            "same-second edges come back in to_id lexical order"
        );

        // trace:TASK-223 | ai:claude — weight 0 and weight N are the same
        // shape out of a real dolt: same tags in, same tags back, at either
        // weight, with the weight only ever in its own column.
        let shared_tags = ["topic:free-will".to_string(), "seed".to_string()];
        let unweighted = store
            .create_node(&NewNode {
                kind: NodeKind::Question,
                title: "unweighted".to_string(),
                description: String::new(),
                tags: shared_tags.to_vec(),
                weight: 0,
            })
            .expect("weight-0 create should succeed");
        let weighted = store
            .create_node(&NewNode {
                kind: NodeKind::Question,
                title: "weighted".to_string(),
                description: String::new(),
                tags: shared_tags.to_vec(),
                weight: 5,
            })
            .expect("weight-5 create should succeed");
        let pair = store
            .fetch_nodes(&[unweighted.clone(), weighted.clone()])
            .expect("weight pair fetch should succeed");
        assert_eq!((pair[0].weight, pair[1].weight), (0, 5));
        assert_eq!(pair[0].tags, shared_tags, "no weight tag consumed or added");
        assert_eq!(pair[0].tags, pair[1].tags, "same shape at either weight");
        assert_eq!(
            store.fetch_node(&unweighted).unwrap(),
            pair[0],
            "per-item and set-based reads agree at weight 0"
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
