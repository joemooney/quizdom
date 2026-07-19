// trace:STORY-206 | ai:claude
//! `quizdom db-migrate` — one-shot exporter that moves the domain graph
//! (`Q-*` / `TERM-*` / `BELIEF-*` nodes and their custom edges) out of the
//! AIDA store and into the Dolt `nodes` / `edges` tables (EPIC-202 /
//! ADR-201).
//!
//! Reads through the `aida` CLI exactly like the [`crate::store`] backend
//! does: two `aida list` calls for the id inventory, one `aida show <id>
//! --full` per node, and one global `aida rel list --limit 0 --all` scan for
//! the edges — show output collapses custom edge kinds to `Related` (the
//! FR-282 blind spot behind BUG-231), so the edge leg cannot come from the
//! show parse. It converts the ADR-22 `weight:N` tag into the numeric
//! `weight` column and writes via `dolt sql`. Re-running is safe: nodes upsert (`INSERT … ON
//! DUPLICATE KEY UPDATE`) and edges insert-ignore against their
//! duplicate-proof primary key.
//!
//! Every run ends with a parity report — node count per kind and edge count
//! per kind, aida-side vs Dolt-side — plus a spot-check that walks a `begets`
//! lineage (default root `Q-23`, the free-will seed) with a recursive CTE in
//! Dolt and compares the reached set against an app-side BFS. Any mismatch is
//! an error.
//!
//! The aida-side edge truth in that report is NOT the exporter's own read
//! (BUG-231: a blind spot shared by both sides self-passes). It is a second,
//! independent read — per-node `aida rel list <id> --type <kind>`, a
//! filtered query path distinct from the exporter's unfiltered global scan —
//! that is cross-checked edge-by-edge against what the exporter loaded, and
//! that also feeds the spot-check BFS.

use crate::db_init::{DoltRunner, SystemDoltRunner, DEFAULT_DOLT_DB_PATH};
use crate::error::{QuizdomError, Result};
use crate::store::{parse_node_show, CommandRunner, SystemCommandRunner};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Default root of the parity spot-check: the free-will seed question whose
/// `begets` lineage is the store's canonical chain.
pub const DEFAULT_SPOT_CHECK_ROOT: &str = "Q-23";

/// The six custom edge kinds of `docs/architecture/graph-schema.md` — the
/// only edges the exporter migrates (built-in edges like `references` stay
/// with the AIDA object).
const CUSTOM_EDGE_KINDS: &[&str] = &[
    "begets",
    "probes",
    "refines",
    "contradicts",
    "agrees",
    "disagrees",
];

/// A domain node as read from the AIDA store, ready for the `nodes` table.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DomainNode {
    id: String,
    /// `question` | `term` | `belief` — the `nodes.kind` enum value.
    kind: &'static str,
    title: String,
    /// Tags minus `weight:*` (the weight becomes the numeric column).
    tags: Vec<String>,
    weight: u32,
    /// The description text (the `nodes.body` column).
    body: String,
}

/// A custom edge as read from the AIDA store — by the exporter (a node's
/// `Relations:` section) or by the independent `rel list` verification read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DomainEdge {
    from: String,
    to: String,
    kind: String,
}

struct DbMigrateConfig {
    path: PathBuf,
    dolt_command: String,
    aida_command: String,
    /// Root of the `begets` spot-check; `None` disables it (`--spot-check none`).
    spot_check_root: Option<String>,
}

impl DbMigrateConfig {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut path = PathBuf::from(DEFAULT_DOLT_DB_PATH);
        let mut dolt_command = "dolt".to_string();
        let mut aida_command = "aida".to_string();
        let mut spot_check_root = Some(DEFAULT_SPOT_CHECK_ROOT.to_string());
        let mut args = args.into_iter().peekable();

        if matches!(args.peek().map(String::as_str), Some("db-migrate")) {
            args.next();
        }

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--path" => path = PathBuf::from(next_arg(&mut args, "--path")?),
                "--dolt" => dolt_command = next_arg(&mut args, "--dolt")?,
                "--aida" => aida_command = next_arg(&mut args, "--aida")?,
                "--spot-check" => {
                    let root = next_arg(&mut args, "--spot-check")?;
                    spot_check_root = (root != "none").then_some(root);
                }
                "--help" | "-h" => return Err(QuizdomError::Usage(usage())),
                other => {
                    return Err(QuizdomError::Usage(format!(
                        "unknown argument: {other}\n{}",
                        usage()
                    )))
                }
            }
        }

        Ok(Self {
            path,
            dolt_command,
            aida_command,
            spot_check_root,
        })
    }
}

fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| QuizdomError::Usage(format!("{name} requires a value")))
}

fn usage() -> String {
    format!(
        "usage: quizdom db-migrate [--path {DEFAULT_DOLT_DB_PATH}] [--dolt dolt] \
         [--aida aida] [--spot-check {DEFAULT_SPOT_CHECK_ROOT}|none]"
    )
}

/// Entry point for `quizdom db-migrate`.
pub fn run_db_migrate(
    args: impl IntoIterator<Item = String>,
    output: &mut impl Write,
) -> Result<()> {
    let config = DbMigrateConfig::parse(args)?;
    let dolt_runner = SystemDoltRunner::new(config.dolt_command.clone());
    db_migrate(&config, &SystemCommandRunner, &dolt_runner, output)
}

/// The full migration flow: read every domain object out of aida, upsert into
/// Dolt, then verify parity. Generic over both runners so the whole pipeline
/// is testable without either binary.
fn db_migrate(
    config: &DbMigrateConfig,
    aida: &dyn CommandRunner,
    dolt: &dyn DoltRunner,
    output: &mut impl Write,
) -> Result<()> {
    if !config.path.join(".dolt").exists() {
        return Err(QuizdomError::Dolt(format!(
            "no Dolt repo at {} — run `quizdom db-init --path {}` first",
            config.path.display(),
            config.path.display()
        )));
    }

    // Read side: inventory, one show per node, then one global edge scan
    // (show output cannot see custom edge kinds — BUG-231).
    let ids = collect_domain_ids(aida, &config.aida_command)?;
    let mut nodes = Vec::new();
    for id in &ids {
        nodes.push(fetch_node(aida, &config.aida_command, id)?);
    }
    let edges = collect_domain_edges(aida, &config.aida_command)?;
    let node_counts = count_by(nodes.iter().map(|node| node.kind.to_string()));
    writeln!(
        output,
        "Read {} domain objects from the AIDA store ({}).",
        nodes.len(),
        render_counts(&node_counts)
    )?;

    // Keep only edges whose both endpoints are domain nodes — an edge into a
    // non-domain object (or a dangling target) cannot satisfy the foreign
    // keys and is not part of the domain graph.
    let known: BTreeSet<&str> = nodes.iter().map(|node| node.id.as_str()).collect();
    let (kept, skipped): (Vec<DomainEdge>, Vec<DomainEdge>) = edges
        .into_iter()
        .partition(|edge| known.contains(edge.from.as_str()) && known.contains(edge.to.as_str()));
    for edge in &skipped {
        writeln!(
            output,
            "  skipping {} -{}-> {} (endpoint outside the domain set)",
            edge.from, edge.kind, edge.to
        )?;
    }

    // Write side: nodes first (edges have foreign keys into them). Batched
    // into size-bounded `dolt sql` calls — a single call carrying the whole
    // store overflows the OS per-argument limit (E2BIG).
    for batch in chunk_statements(nodes_upsert_sql(&nodes), SQL_BATCH_BUDGET) {
        run_dolt_sql(dolt, &config.path, &batch)?;
    }
    for batch in chunk_statements(edges_insert_sql(&kept), SQL_BATCH_BUDGET) {
        run_dolt_sql(dolt, &config.path, &batch)?;
    }
    writeln!(
        output,
        "Loaded {} nodes and {} edges into {} (re-run safe: nodes upsert, edges insert-ignore).",
        nodes.len(),
        kept.len(),
        config.path.display()
    )?;

    // Independent aida-side edge read (BUG-231): per-node `rel list --type`
    // sees custom edge kinds through a different aida surface than the
    // exporter's show parse, so a blind spot in either read cannot self-pass.
    // trace:BUG-231 | ai:claude
    let verified: Vec<DomainEdge> = read_edges_via_rel_list(aida, &config.aida_command, &ids)?
        .into_iter()
        .filter(|edge| known.contains(edge.from.as_str()) && known.contains(edge.to.as_str()))
        .collect();
    let verified_counts = count_by(verified.iter().map(|edge| edge.kind.clone()));

    // Parity: what Dolt now holds must match the independent aida-side read.
    let dolt_nodes = parse_count_table(&run_dolt_sql(
        dolt,
        &config.path,
        "SELECT kind, COUNT(*) FROM nodes GROUP BY kind ORDER BY kind;",
    )?);
    let dolt_edges = parse_count_table(&run_dolt_sql(
        dolt,
        &config.path,
        "SELECT kind, COUNT(*) FROM edges GROUP BY kind ORDER BY kind;",
    )?);

    let mut mismatches = Vec::new();
    writeln!(output, "Parity report (aida-side / dolt-side):")?;
    writeln!(
        output,
        "  nodes: {}",
        render_parity(&node_counts, &dolt_nodes, &mut mismatches)
    )?;
    writeln!(
        output,
        "  edges: {}",
        render_parity(&verified_counts, &dolt_edges, &mut mismatches)
    )?;
    let before_cross_check = mismatches.len();
    cross_check_edge_reads(&kept, &verified, &mut mismatches);
    if mismatches.len() == before_cross_check {
        writeln!(
            output,
            "  edge cross-check: exporter read matches per-node `aida rel list --type` ({} edges) ✓",
            verified.len()
        )?;
    }

    if let Some(root) = &config.spot_check_root {
        let expected = begets_reachable(root, &verified);
        if expected.len() <= 1 && !known.contains(root.as_str()) {
            mismatches.push(format!(
                "spot-check root {root} is not a migrated node (use --spot-check <id>|none)"
            ));
        } else {
            let reached =
                parse_id_column(&run_dolt_sql(dolt, &config.path, &begets_cte_sql(root))?);
            if reached == expected {
                writeln!(
                    output,
                    "  spot-check: begets lineage of {root} reaches {} nodes — dolt CTE matches aida-side walk ✓",
                    expected.len()
                )?;
            } else {
                mismatches.push(format!(
                    "spot-check lineage of {root}: aida-side walk reaches {:?} but dolt CTE reaches {:?}",
                    expected, reached
                ));
            }
        }
    }

    if !mismatches.is_empty() {
        return Err(QuizdomError::Dolt(format!(
            "parity mismatch:\n  {}",
            mismatches.join("\n  ")
        )));
    }
    writeln!(output, "Parity OK.")?;
    Ok(())
}

/// The id inventory: `Q-*` and `BELIEF-*` are both aida `functional` objects
/// (the `Q` prefix is an override), `TERM-*` objects are aida `term` objects.
fn collect_domain_ids(aida: &dyn CommandRunner, command: &str) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for (aida_type, prefixes) in [
        ("functional", &["Q-", "BELIEF-"][..]),
        ("term", &["TERM-"][..]),
    ] {
        let listing = run_aida(aida, command, &["list", "--type", aida_type, "--no-scope"])?;
        ids.extend(parse_list_ids(&listing, prefixes));
    }
    Ok(ids)
}

/// First-column ids out of `aida list` output, keeping rows whose id carries
/// one of `prefixes` followed by digits (skips the header and rule lines).
fn parse_list_ids(output: &str, prefixes: &[&str]) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let id = line.split_whitespace().next()?;
            prefixes
                .iter()
                .any(|prefix| {
                    id.strip_prefix(prefix).is_some_and(|rest| {
                        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
                    })
                })
                .then(|| id.to_string())
        })
        .collect()
}

/// One `aida show <id> --full` gives the node's fields and its description.
/// Edges do NOT come from here — show output collapses custom edge kinds to
/// `Related`, the FR-282 blind spot behind BUG-231.
fn fetch_node(aida: &dyn CommandRunner, command: &str, id: &str) -> Result<DomainNode> {
    let show = run_aida(aida, command, &["show", id, "--full"])?;
    let record = parse_node_show(&show)?;
    let kind = node_kind_for_id(&record.id)?;
    let tags: Vec<String> = record
        .tags
        .iter()
        .filter(|tag| !tag.starts_with("weight:"))
        .cloned()
        .collect();
    Ok(DomainNode {
        id: record.id,
        kind,
        title: record.title,
        tags,
        weight: record.weight,
        body: extract_description(&show),
    })
}

/// Map an id prefix onto the `nodes.kind` enum value.
fn node_kind_for_id(id: &str) -> Result<&'static str> {
    if id.starts_with("Q-") {
        Ok("question")
    } else if id.starts_with("TERM-") {
        Ok("term")
    } else if id.starts_with("BELIEF-") {
        Ok("belief")
    } else {
        Err(QuizdomError::Parse(format!(
            "{id} is not a domain object (expected a Q-/TERM-/BELIEF- prefix)"
        )))
    }
}

/// The exporter's edge read (BUG-231): one global `aida rel list --limit 0
/// --all` scan of the whole graph. `--limit 0` lifts the firehose auto-cap
/// and `--all` keeps edges whose endpoints reached a terminal status —
/// truncation on either axis would silently drop domain edges. Custom kinds
/// only; endpoint filtering against the migrated node set happens at the
/// call site.
// trace:BUG-231 | ai:claude
fn collect_domain_edges(aida: &dyn CommandRunner, command: &str) -> Result<Vec<DomainEdge>> {
    let listing = run_aida(aida, command, &["rel", "list", "--limit", "0", "--all"])?;
    Ok(parse_global_rel_list(&listing))
}

/// Custom-kind rows of the global `aida rel list` table. Built-in edge kinds
/// (`references`, `parent`, …) are not domain edges and are dropped here;
/// the header, the `N edges` footer, and pagination notes fail the row shape.
fn parse_global_rel_list(output: &str) -> Vec<DomainEdge> {
    output
        .lines()
        .filter_map(|line| {
            let (from, kind, to) = rel_list_row(line)?;
            CUSTOM_EDGE_KINDS.contains(&kind).then(|| DomainEdge {
                from: from.to_string(),
                to: to.to_string(),
                kind: kind.to_string(),
            })
        })
        .collect()
}

/// The `FROM TYPE TO` head of an `aida rel list` data row (both the global
/// and the per-node table share the layout); a leading `·` (rel list's
/// built-in-edge marker) is tolerated.
fn rel_list_row(line: &str) -> Option<(&str, &str, &str)> {
    let mut tokens = line.split_whitespace().peekable();
    if tokens.peek() == Some(&"·") {
        tokens.next();
    }
    Some((tokens.next()?, tokens.next()?, tokens.next()?))
}

/// The description block of a human-format `aida show`: everything between
/// the header (whose last line is `Centrality:`, falling back to the first
/// blank line) and the trailing `Git linkage:` / rule section.
fn extract_description(show: &str) -> String {
    let lines: Vec<&str> = show.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.starts_with("Centrality:"))
        .map(|index| index + 1)
        .or_else(|| lines.iter().position(|line| line.trim().is_empty()))
        .unwrap_or(lines.len());
    let body: Vec<&str> = lines[start.min(lines.len())..]
        .iter()
        .take_while(|line| !line.starts_with("Git linkage:") && !line.starts_with("────"))
        .copied()
        .collect();
    let trimmed_start = body.iter().position(|line| !line.trim().is_empty());
    let trimmed_end = body.iter().rposition(|line| !line.trim().is_empty());
    match (trimmed_start, trimmed_end) {
        (Some(first), Some(last)) => body[first..=last].join("\n"),
        _ => String::new(),
    }
}

/// The independent aida-side edge read (BUG-231): one `aida rel list <id>
/// --type <kind>` per node per custom kind. These are filtered per-node
/// queries — a different query path than the unfiltered global scan the
/// exporter loads from — so the parity check it feeds cannot share a blind
/// spot (a broken type filter, a missed node) with the exporter.
// trace:BUG-231 | ai:claude
fn read_edges_via_rel_list(
    aida: &dyn CommandRunner,
    command: &str,
    ids: &[String],
) -> Result<Vec<DomainEdge>> {
    let mut edges = Vec::new();
    for id in ids {
        for kind in CUSTOM_EDGE_KINDS {
            let listing = run_aida(aida, command, &["rel", "list", id, "--type", kind])?;
            edges.extend(parse_rel_list(&listing, id, kind));
        }
    }
    Ok(edges)
}

/// Data rows of a per-node `aida rel list` table (`FROM TYPE TO TITLE`): keep
/// rows whose FROM is the queried node and TYPE the queried kind. The header,
/// the `N edges` footer, and the empty-filter message all fail that shape, so
/// no explicit skipping is needed.
fn parse_rel_list(output: &str, from: &str, kind: &str) -> Vec<DomainEdge> {
    output
        .lines()
        .filter_map(|line| {
            let (row_from, row_kind, to) = rel_list_row(line)?;
            (row_from == from && row_kind == kind).then(|| DomainEdge {
                from: from.to_string(),
                to: to.to_string(),
                kind: kind.to_string(),
            })
        })
        .collect()
}

/// Edge-by-edge comparison of the two reads. Counts alone could coincide
/// while the sets differ, so any edge visible to one read and not the other
/// is a parity mismatch in its own right.
fn cross_check_edge_reads(
    exporter: &[DomainEdge],
    verified: &[DomainEdge],
    mismatches: &mut Vec<String>,
) {
    let key = |edge: &DomainEdge| (edge.from.clone(), edge.kind.clone(), edge.to.clone());
    let exporter_set: BTreeSet<_> = exporter.iter().map(key).collect();
    let verified_set: BTreeSet<_> = verified.iter().map(key).collect();
    for (from, kind, to) in verified_set.difference(&exporter_set) {
        mismatches.push(format!(
            "edge {from} -{kind}-> {to} visible to `aida rel list --type` but missed by the exporter read"
        ));
    }
    for (from, kind, to) in exporter_set.difference(&verified_set) {
        mismatches.push(format!(
            "edge {from} -{kind}-> {to} read by the exporter but absent from `aida rel list --type`"
        ));
    }
}

fn run_aida(aida: &dyn CommandRunner, command: &str, args: &[&str]) -> Result<String> {
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    let output = aida.run(command, &args)?;
    if !output.status.success() {
        return Err(QuizdomError::Aida(format!(
            "aida {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn run_dolt_sql(dolt: &dyn DoltRunner, path: &Path, sql: &str) -> Result<String> {
    let args = vec!["sql".to_string(), "-q".to_string(), sql.to_string()];
    let output = dolt.run(path, &args)?;
    if !output.status.success() {
        return Err(QuizdomError::Dolt(format!(
            "dolt sql failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Byte budget per `dolt sql -q` invocation. Linux caps a single argv string
/// at 128 KiB (`MAX_ARG_STRLEN`); staying well under it leaves headroom for
/// the rest of the command line.
const SQL_BATCH_BUDGET: usize = 64 * 1024;

/// Pack statements into batches whose joined length stays within `budget`
/// (a statement larger than the budget still gets its own batch).
fn chunk_statements(statements: Vec<String>, budget: usize) -> Vec<String> {
    let mut batches = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut current_len = 0;
    for statement in statements {
        if !current.is_empty() && current_len + statement.len() > budget {
            batches.push(current.join("\n"));
            current = Vec::new();
            current_len = 0;
        }
        current_len += statement.len() + 1;
        current.push(statement);
    }
    if !current.is_empty() {
        batches.push(current.join("\n"));
    }
    batches
}

/// One upsert statement per node. The update clause repeats the literals so
/// re-running refreshes a changed node in place (idempotent by construction —
/// no dependency on the deprecated `VALUES()` function).
fn nodes_upsert_sql(nodes: &[DomainNode]) -> Vec<String> {
    nodes
        .iter()
        .map(|node| {
            let id = sql_quote(&node.id);
            let kind = sql_quote(node.kind);
            let title = sql_quote(&node.title);
            let body = sql_quote(&node.body);
            let tags = sql_quote(&node.tags.join(","));
            let weight = node.weight;
            format!(
                "INSERT INTO nodes (id, kind, title, body, tags, weight) \
                 VALUES ({id}, {kind}, {title}, {body}, {tags}, {weight}) \
                 ON DUPLICATE KEY UPDATE kind = {kind}, title = {title}, \
                 body = {body}, tags = {tags}, weight = {weight};"
            )
        })
        .collect()
}

/// One `INSERT IGNORE` per edge: the `(from_id, to_id, kind)` primary key
/// makes duplicates no-ops, so re-runs are safe.
fn edges_insert_sql(edges: &[DomainEdge]) -> Vec<String> {
    edges
        .iter()
        .map(|edge| {
            format!(
                "INSERT IGNORE INTO edges (from_id, to_id, kind) VALUES ({}, {}, {});",
                sql_quote(&edge.from),
                sql_quote(&edge.to),
                sql_quote(&edge.kind)
            )
        })
        .collect()
}

/// A MySQL single-quoted string literal (backslashes and quotes escaped).
pub(crate) fn sql_quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('\'');
    for character in text.chars() {
        match character {
            '\'' => quoted.push_str("''"),
            '\\' => quoted.push_str("\\\\"),
            _ => quoted.push(character),
        }
    }
    quoted.push('\'');
    quoted
}

/// The Dolt-side spot-check: the distinct set of nodes reachable from `root`
/// over `begets` edges. `UNION` (not `UNION ALL`) deduplicates rows, so the
/// walk terminates even on a cyclic graph — the same visited-set semantics as
/// the app-side BFS it is compared against.
fn begets_cte_sql(root: &str) -> String {
    format!(
        "WITH RECURSIVE reachable (id) AS (\
         SELECT CAST({root} AS CHAR(64)) \
         UNION \
         SELECT e.to_id FROM edges e JOIN reachable r ON e.from_id = r.id \
         WHERE e.kind = 'begets') \
         SELECT id FROM reachable ORDER BY id;",
        root = sql_quote(root)
    )
}

/// The aida-side spot-check: BFS over the just-read edges, `begets` only.
fn begets_reachable(root: &str, edges: &[DomainEdge]) -> BTreeSet<String> {
    let mut reached = BTreeSet::from([root.to_string()]);
    let mut frontier = vec![root.to_string()];
    while let Some(current) = frontier.pop() {
        for edge in edges {
            if edge.kind == "begets" && edge.from == current && reached.insert(edge.to.clone()) {
                frontier.push(edge.to.clone());
            }
        }
    }
    reached
}

fn count_by(keys: impl Iterator<Item = String>) -> BTreeMap<String, u64> {
    let mut counts = BTreeMap::new();
    for key in keys {
        *counts.entry(key).or_insert(0) += 1;
    }
    counts
}

fn render_counts(counts: &BTreeMap<String, u64>) -> String {
    if counts.is_empty() {
        return "none".to_string();
    }
    counts
        .iter()
        .map(|(kind, count)| format!("{count} {kind}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// One `kind expected/actual` cell per kind across both sides; any difference
/// is appended to `mismatches`.
fn render_parity(
    expected: &BTreeMap<String, u64>,
    actual: &BTreeMap<String, u64>,
    mismatches: &mut Vec<String>,
) -> String {
    let kinds: BTreeSet<&String> = expected.keys().chain(actual.keys()).collect();
    if kinds.is_empty() {
        return "none".to_string();
    }
    kinds
        .into_iter()
        .map(|kind| {
            let want = expected.get(kind).copied().unwrap_or(0);
            let got = actual.get(kind).copied().unwrap_or(0);
            if want == got {
                format!("{kind} {want}/{got} ✓")
            } else {
                mismatches.push(format!("{kind}: aida-side {want}, dolt-side {got}"));
                format!("{kind} {want}/{got} ✗")
            }
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Parse a two-column `kind | COUNT(*)` table out of `dolt sql` ASCII output.
fn parse_count_table(output: &str) -> BTreeMap<String, u64> {
    table_rows(output)
        .filter_map(|cells| {
            let count = cells.get(1)?.parse::<u64>().ok()?;
            Some((cells.first()?.clone(), count))
        })
        .collect()
}

/// Parse a single-column id table out of `dolt sql` ASCII output.
fn parse_id_column(output: &str) -> BTreeSet<String> {
    table_rows(output)
        .filter_map(|cells| cells.first().cloned())
        .collect()
}

/// Data rows of a `dolt sql` ASCII table: `| a | b |` lines minus the header
/// row (which names columns, never data) and the `+---+` rules.
fn table_rows(output: &str) -> impl Iterator<Item = Vec<String>> + '_ {
    output
        .lines()
        .filter(|line| line.trim_start().starts_with('|'))
        .skip(1)
        .map(|line| {
            line.trim()
                .trim_matches('|')
                .split('|')
                .map(|cell| cell.trim().to_string())
                .collect()
        })
}

// trace:STORY-206 | ai:claude
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};

    /// Canned `aida` responses keyed by the full argument vector.
    struct ScriptedAida {
        responses: Vec<(Vec<String>, String)>,
    }

    impl ScriptedAida {
        fn new(responses: &[(&[&str], &str)]) -> Self {
            Self {
                responses: responses
                    .iter()
                    .map(|(args, stdout)| {
                        (
                            args.iter().map(|arg| arg.to_string()).collect(),
                            stdout.to_string(),
                        )
                    })
                    .collect(),
            }
        }

        /// Script the exporter's global `rel list --limit 0 --all` response,
        /// rendered from `edges` the way the real CLI prints the full-graph
        /// table — `·`-marked built-in rows, a header, and a footer included.
        fn with_global_rel_list(mut self, edges: &[(&str, &str, &str)]) -> Self {
            let mut table = "FROM  TYPE  TO  TITLE\n".to_string();
            table.push_str("· VIS-1  parent  EPIC-5  Domain graph model on AIDA\n");
            for (from, kind, to) in edges {
                table.push_str(&format!("  {from}  {kind}  {to}  Title of {to}\n"));
            }
            table.push_str("  Q-1  references  STORY-16  seed cluster\n");
            table.push_str(&format!("\n{} edges\n", edges.len() + 2));
            self.responses.push((
                ["rel", "list", "--limit", "0", "--all"]
                    .iter()
                    .map(|arg| arg.to_string())
                    .collect(),
                table,
            ));
            self
        }

        /// Script one `rel list <id> --type <kind>` response per node × custom
        /// kind, rendered from `edges` the way the real CLI prints them.
        fn with_rel_lists(mut self, nodes: &[&str], edges: &[(&str, &str, &str)]) -> Self {
            for id in nodes {
                for kind in CUSTOM_EDGE_KINDS {
                    let rows: Vec<&(&str, &str, &str)> = edges
                        .iter()
                        .filter(|(from, edge_kind, _)| from == id && edge_kind == kind)
                        .collect();
                    let stdout = if rows.is_empty() {
                        "(no outgoing relationships match the filter)\n".to_string()
                    } else {
                        let mut table = "FROM  TYPE  TO  TITLE\n".to_string();
                        for (from, edge_kind, to) in &rows {
                            table
                                .push_str(&format!("  {from}  {edge_kind}  {to}  Title of {to}\n"));
                        }
                        table.push_str(&format!("\n{} edges\n", rows.len()));
                        table
                    };
                    self.responses.push((
                        ["rel", "list", id, "--type", kind]
                            .iter()
                            .map(|arg| arg.to_string())
                            .collect(),
                        stdout,
                    ));
                }
            }
            self
        }
    }

    impl CommandRunner for ScriptedAida {
        fn run(&self, _program: &str, args: &[String]) -> Result<Output> {
            let stdout = self
                .responses
                .iter()
                .find(|(expected, _)| expected == args)
                .map(|(_, stdout)| stdout.clone())
                .unwrap_or_else(|| panic!("unscripted aida call: {args:?}"));
            Ok(Output {
                status: ExitStatus::from_raw(0),
                stdout: stdout.into_bytes(),
                stderr: Vec::new(),
            })
        }
    }

    /// Records every dolt invocation and replays canned stdout in FIFO order.
    struct RecordingDolt {
        calls: RefCell<Vec<Vec<String>>>,
        responses: RefCell<Vec<String>>,
    }

    impl RecordingDolt {
        fn new(responses: &[&str]) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                responses: RefCell::new(responses.iter().map(|s| s.to_string()).collect()),
            }
        }

        fn sql(&self, index: usize) -> String {
            self.calls.borrow()[index][2].clone()
        }
    }

    impl DoltRunner for RecordingDolt {
        fn run(&self, _cwd: &Path, args: &[String]) -> Result<Output> {
            self.calls.borrow_mut().push(args.to_vec());
            let stdout = {
                let mut responses = self.responses.borrow_mut();
                if responses.is_empty() {
                    String::new()
                } else {
                    responses.remove(0)
                }
            };
            Ok(Output {
                status: ExitStatus::from_raw(0),
                stdout: stdout.into_bytes(),
                stderr: Vec::new(),
            })
        }
    }

    const FUNCTIONAL_LIST: &str = "  ID    Type   Status   Priority   Title\n\
        ───────────────────────────────────────\n\
        \x20 Q-1          Functional   Approved   High   Root question\n\
        \x20 Q-2          Functional   Approved   High   Follow-up\n\
        \x20 BELIEF-9     Functional   Approved   Medium Belief one\n\
        \n3 requirements\n";

    const TERM_LIST: &str = "  ID    Type   Status   Priority   Title\n\
        ───────────────────────────────────────\n\
        \x20 TERM-5       Term         Approved   High   free will / libertarian\n\
        \n1 requirements\n";

    fn show_output(id: &str, relations: &str, description: &str) -> String {
        format!(
            "ID: {id}\nUUID: 0000\nTitle: Title of {id}\nType: Functional\n\
             Status: ▸ Approved\nPriority: High\n\
             Tags: topic:free-will, weight:70, seed\n\
             Relations:\n{relations}Centrality: 0 in / 1 out  (heft 1)\n\n\
             {description}\n\n\
             Git linkage: no commits or trace comments reference this spec yet\n"
        )
    }

    /// The scripted store's node inventory and its true custom-edge set —
    /// what both the global `rel list` scan and the per-node `rel list`
    /// responses render (`Q-2 -begets-> Q-404` dangles on purpose).
    const SCRIPTED_NODES: &[&str] = &["Q-1", "Q-2", "BELIEF-9", "TERM-5"];
    const SCRIPTED_EDGES: &[(&str, &str, &str)] = &[
        ("Q-1", "begets", "Q-2"),
        ("Q-1", "probes", "TERM-5"),
        ("Q-2", "probes", "BELIEF-9"),
        ("Q-2", "begets", "Q-404"),
    ];

    /// The four show responses — `Relations:` sections deliberately carry
    /// only a built-in edge: show output cannot see custom edge kinds (the
    /// FR-282 blind spot), so nothing here may feed the edge read.
    fn scripted_shows() -> Vec<(Vec<String>, String)> {
        let q1 = show_output(
            "Q-1",
            "  ↳ references STORY-16 (seed cluster)\n",
            "answer: yes-no\n\nRoot of the chain. It's \"quoted\".",
        );
        let q2 = show_output("Q-2", "", "answer: free-text");
        let belief = show_output("BELIEF-9", "", "A proposition.");
        let term = show_output("TERM-5", "", "definition: the libertarian sense");
        [
            ("Q-1", q1),
            ("Q-2", q2),
            ("BELIEF-9", belief),
            ("TERM-5", term),
        ]
        .into_iter()
        .map(|(id, stdout)| (["show", id, "--full"].map(String::from).to_vec(), stdout))
        .collect()
    }

    fn inventory_store() -> ScriptedAida {
        let mut store = ScriptedAida::new(&[
            (
                &["list", "--type", "functional", "--no-scope"],
                FUNCTIONAL_LIST,
            ),
            (&["list", "--type", "term", "--no-scope"], TERM_LIST),
        ]);
        store.responses.extend(scripted_shows());
        store
    }

    fn scripted_store() -> ScriptedAida {
        inventory_store()
            .with_global_rel_list(SCRIPTED_EDGES)
            .with_rel_lists(SCRIPTED_NODES, SCRIPTED_EDGES)
    }

    /// The BUG-231 shape: the exporter's global scan comes back without the
    /// custom edges, while the per-node `rel list --type` verification still
    /// sees them. The parity check must fail, not self-pass.
    fn blind_exporter_store() -> ScriptedAida {
        inventory_store()
            .with_global_rel_list(&[])
            .with_rel_lists(SCRIPTED_NODES, SCRIPTED_EDGES)
    }

    fn dolt_repo_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("quizdom-db-migrate-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".dolt")).unwrap();
        dir
    }

    fn config(path: &Path, spot_check: Option<&str>) -> DbMigrateConfig {
        DbMigrateConfig {
            path: path.to_path_buf(),
            dolt_command: "dolt".to_string(),
            aida_command: "aida".to_string(),
            spot_check_root: spot_check.map(str::to_string),
        }
    }

    const NODES_PARITY: &str = "+----------+----------+\n\
        | kind     | COUNT(*) |\n+----------+----------+\n\
        | belief   | 1        |\n| question | 2        |\n| term     | 1        |\n\
        +----------+----------+\n";
    const EDGES_PARITY: &str = "+--------+----------+\n\
        | kind   | COUNT(*) |\n+--------+----------+\n\
        | begets | 1        |\n| probes | 2        |\n\
        +--------+----------+\n";
    const SPOT_CHECK: &str = "+------+\n| id   |\n+------+\n| Q-1  |\n| Q-2  |\n+------+\n";

    #[test]
    fn migrates_nodes_then_edges_and_reports_parity() {
        let dir = dolt_repo_dir("happy");
        let dolt = RecordingDolt::new(&["", "", NODES_PARITY, EDGES_PARITY, SPOT_CHECK]);
        let mut output = Vec::new();

        db_migrate(
            &config(&dir, Some("Q-1")),
            &scripted_store(),
            &dolt,
            &mut output,
        )
        .expect("migration should succeed");

        let calls = dolt.calls.borrow();
        assert_eq!(
            calls.len(),
            5,
            "nodes, edges, two parity queries, spot-check"
        );
        drop(calls);

        // Nodes land first (edges carry foreign keys into them), as upserts.
        let nodes_sql = dolt.sql(0);
        assert!(nodes_sql.contains("INSERT INTO nodes"));
        assert!(nodes_sql.contains("ON DUPLICATE KEY UPDATE"));
        assert!(nodes_sql.contains("'Q-1', 'question', 'Title of Q-1'"));
        assert!(nodes_sql.contains("'BELIEF-9', 'belief'"));
        assert!(nodes_sql.contains("'TERM-5', 'term'"));
        // weight:70 became the numeric column and left the tag list.
        assert!(nodes_sql.contains(", 70)"));
        assert!(nodes_sql.contains("'topic:free-will,seed'"));
        // The description body rides along, with quotes escaped.
        assert!(nodes_sql.contains("It''s \"quoted\"."));

        // Edges: insert-ignore, only custom kinds with both endpoints known.
        let edges_sql = dolt.sql(1);
        assert!(edges_sql.contains("INSERT IGNORE INTO edges"));
        assert!(edges_sql.contains("('Q-1', 'Q-2', 'begets')"));
        assert!(edges_sql.contains("('Q-1', 'TERM-5', 'probes')"));
        assert!(edges_sql.contains("('Q-2', 'BELIEF-9', 'probes')"));
        assert!(
            !edges_sql.contains("references"),
            "built-in edges stay behind"
        );
        assert!(!edges_sql.contains("Q-404"), "dangling targets are skipped");

        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("Read 4 domain objects"));
        assert!(rendered.contains("1 belief, 2 question, 1 term"));
        assert!(rendered.contains("skipping Q-2 -begets-> Q-404"));
        assert!(rendered.contains("question 2/2 ✓"));
        assert!(rendered.contains("begets 1/1 ✓"));
        assert!(rendered.contains(
            "edge cross-check: exporter read matches per-node `aida rel list --type` (3 edges) ✓"
        ));
        assert!(rendered.contains("begets lineage of Q-1 reaches 2 nodes"));
        assert!(rendered.contains("Parity OK."));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BUG-231 regression: an exporter edge read that comes back empty must
    /// fail parity against the independent `rel list --type` read — the
    /// original bug was both sides sharing the blind read and self-passing.
    #[test]
    fn blind_exporter_edge_read_fails_parity_instead_of_self_passing() {
        let dir = dolt_repo_dir("blind");
        // Nodes write, then the two parity queries — no edges were read, so
        // no edge batch fires and Dolt reports an empty edges table.
        let dolt = RecordingDolt::new(&["", NODES_PARITY, ""]);
        let mut output = Vec::new();

        let result = db_migrate(
            &config(&dir, None),
            &blind_exporter_store(),
            &dolt,
            &mut output,
        );
        match result {
            Err(QuizdomError::Dolt(message)) => {
                assert!(message.contains("parity mismatch"));
                assert!(message.contains("begets: aida-side 1, dolt-side 0"));
                assert!(message.contains("probes: aida-side 2, dolt-side 0"));
                assert!(message
                    .contains("Q-1 -begets-> Q-2 visible to `aida rel list --type` but missed by the exporter read"));
            }
            other => panic!("expected parity error, got {other:?}"),
        }
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("begets 1/0 ✗"));
        assert!(
            !rendered.contains("edge cross-check: exporter read matches"),
            "the ✓ line must not render when the reads disagree"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_rel_list_keeps_matching_data_rows_only() {
        let listing = "FROM  TYPE    TO    TITLE\n\
            \x20 Q-23  begets  Q-26  Do you mean the ability…\n\
            · Q-23  begets  Q-27  a built-in-marked row is still a row\n\
            \n2 edges\n";
        let edges = parse_rel_list(listing, "Q-23", "begets");
        assert_eq!(
            edges,
            vec![
                DomainEdge {
                    from: "Q-23".to_string(),
                    to: "Q-26".to_string(),
                    kind: "begets".to_string(),
                },
                DomainEdge {
                    from: "Q-23".to_string(),
                    to: "Q-27".to_string(),
                    kind: "begets".to_string(),
                },
            ]
        );
        // The empty-filter message parses to no edges.
        assert!(parse_rel_list(
            "(no outgoing relationships match the filter)\n",
            "Q-23",
            "begets"
        )
        .is_empty());
        // A row for a different kind or node never leaks through.
        assert!(parse_rel_list(listing, "Q-23", "probes").is_empty());
        assert!(parse_rel_list(listing, "Q-26", "begets").is_empty());
    }

    #[test]
    fn count_mismatch_is_a_parity_error() {
        let dir = dolt_repo_dir("mismatch");
        // Dolt reports one question too few.
        let short = NODES_PARITY.replace("| question | 2        |", "| question | 1        |");
        let dolt = RecordingDolt::new(&["", "", &short, EDGES_PARITY, SPOT_CHECK]);
        let mut output = Vec::new();

        let result = db_migrate(&config(&dir, None), &scripted_store(), &dolt, &mut output);
        match result {
            Err(QuizdomError::Dolt(message)) => {
                assert!(message.contains("parity mismatch"));
                assert!(message.contains("question: aida-side 2, dolt-side 1"));
            }
            other => panic!("expected parity error, got {other:?}"),
        }
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("question 2/1 ✗"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spot_check_set_difference_is_a_parity_error() {
        let dir = dolt_repo_dir("spot");
        // The CTE reaches Q-1 only — the aida-side walk expects Q-1 and Q-2.
        let short_chain = "+------+\n| id   |\n+------+\n| Q-1  |\n+------+\n";
        let dolt = RecordingDolt::new(&["", "", NODES_PARITY, EDGES_PARITY, short_chain]);
        let mut output = Vec::new();

        let result = db_migrate(
            &config(&dir, Some("Q-1")),
            &scripted_store(),
            &dolt,
            &mut output,
        );
        match result {
            Err(QuizdomError::Dolt(message)) => {
                assert!(message.contains("spot-check lineage of Q-1"));
            }
            other => panic!("expected spot-check error, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_spot_check_root_is_a_parity_error() {
        let dir = dolt_repo_dir("badroot");
        let dolt = RecordingDolt::new(&["", "", NODES_PARITY, EDGES_PARITY]);
        let mut output = Vec::new();

        let result = db_migrate(
            &config(&dir, Some("Q-999")),
            &scripted_store(),
            &dolt,
            &mut output,
        );
        match result {
            Err(QuizdomError::Dolt(message)) => {
                assert!(message.contains("Q-999 is not a migrated node"));
            }
            other => panic!("expected root error, got {other:?}"),
        }
        assert_eq!(dolt.calls.borrow().len(), 4, "no spot-check query fired");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dolt_repo_points_at_db_init() {
        let dir =
            std::env::temp_dir().join(format!("quizdom-db-migrate-norepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dolt = RecordingDolt::new(&[]);
        let mut output = Vec::new();

        let result = db_migrate(&config(&dir, None), &scripted_store(), &dolt, &mut output);
        match result {
            Err(QuizdomError::Dolt(message)) => {
                assert!(message.contains("quizdom db-init"));
            }
            other => panic!("expected missing-repo error, got {other:?}"),
        }
        assert!(dolt.calls.borrow().is_empty());
    }

    #[test]
    fn extract_description_takes_the_block_between_header_and_linkage() {
        let show = show_output(
            "Q-1",
            "  ↳ begets Q-2 (x)\n",
            "answer: yes-no\n\nBody text.",
        );
        assert_eq!(extract_description(&show), "answer: yes-no\n\nBody text.");
        // A show with no description yields an empty body.
        let empty = show_output("Q-1", "", "");
        assert_eq!(extract_description(&empty), "");
    }

    #[test]
    fn parse_global_rel_list_keeps_custom_kinds_only() {
        let listing = "FROM  TYPE  TO  TITLE\n\
            \x20 Q-23   begets      Q-26      Do you mean the ability…\n\
            · VIS-1  parent      EPIC-5    Domain graph model on AIDA\n\
            · Q-23   disagrees   BELIEF-9  a built-in-marked row is still a row\n\
            \x20 Q-1    references  STORY-16  built-in kinds stay behind\n\
            \n50 of 143 edges shown — pass --limit 0 for all, or a filter\n\
            \x20 (185 hidden between Completed/Rejected reqs — pass --all to see them)\n";
        assert_eq!(
            parse_global_rel_list(listing),
            vec![
                DomainEdge {
                    from: "Q-23".to_string(),
                    to: "Q-26".to_string(),
                    kind: "begets".to_string(),
                },
                DomainEdge {
                    from: "Q-23".to_string(),
                    to: "BELIEF-9".to_string(),
                    kind: "disagrees".to_string(),
                },
            ]
        );
        assert!(parse_global_rel_list("FROM  TYPE  TO  TITLE\n\n0 edges\n").is_empty());
    }

    #[test]
    fn parse_list_ids_skips_headers_and_foreign_prefixes() {
        assert_eq!(
            parse_list_ids(FUNCTIONAL_LIST, &["Q-", "BELIEF-"]),
            vec!["Q-1", "Q-2", "BELIEF-9"]
        );
        assert_eq!(
            parse_list_ids(FUNCTIONAL_LIST, &["TERM-"]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn chunk_statements_respects_the_byte_budget() {
        let statements: Vec<String> = (0..5).map(|index| format!("stmt-{index};")).collect();
        // Budget fits two ~7-byte statements per batch.
        let batches = chunk_statements(statements.clone(), 16);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0], "stmt-0;\nstmt-1;");
        assert_eq!(batches[2], "stmt-4;");
        // A statement larger than the budget still ships, alone.
        let oversized = chunk_statements(vec!["x".repeat(100)], 10);
        assert_eq!(oversized.len(), 1);
        // No statements → no batches → no dolt calls.
        assert!(chunk_statements(Vec::new(), 10).is_empty());
    }

    #[test]
    fn sql_quote_escapes_quotes_and_backslashes() {
        assert_eq!(sql_quote("it's"), "'it''s'");
        assert_eq!(sql_quote(r"a\b"), r"'a\\b'");
        assert_eq!(sql_quote("plain"), "'plain'");
    }

    #[test]
    fn begets_reachable_walks_only_begets_and_survives_cycles() {
        let edge = |from: &str, to: &str, kind: &str| DomainEdge {
            from: from.to_string(),
            to: to.to_string(),
            kind: kind.to_string(),
        };
        let edges = vec![
            edge("Q-1", "Q-2", "begets"),
            edge("Q-2", "Q-1", "begets"), // cycle back to the root
            edge("Q-2", "Q-3", "begets"),
            edge("Q-1", "TERM-5", "probes"), // not walked
        ];
        let reached = begets_reachable("Q-1", &edges);
        assert_eq!(
            reached,
            BTreeSet::from(["Q-1".to_string(), "Q-2".to_string(), "Q-3".to_string()])
        );
    }

    #[test]
    fn parse_count_table_reads_dolt_ascii_output() {
        let counts = parse_count_table(NODES_PARITY);
        assert_eq!(counts.get("question"), Some(&2));
        assert_eq!(counts.get("term"), Some(&1));
        assert_eq!(counts.get("belief"), Some(&1));
        assert_eq!(counts.len(), 3);
    }

    #[test]
    fn config_parse_reads_overrides_and_spot_check_none() {
        let parsed = DbMigrateConfig::parse(
            [
                "db-migrate",
                "--path",
                "/tmp/x",
                "--dolt",
                "dolt2",
                "--aida",
                "aida2",
                "--spot-check",
                "none",
            ]
            .map(String::from),
        )
        .unwrap();
        assert_eq!(parsed.path, PathBuf::from("/tmp/x"));
        assert_eq!(parsed.dolt_command, "dolt2");
        assert_eq!(parsed.aida_command, "aida2");
        assert_eq!(parsed.spot_check_root, None);

        let default = DbMigrateConfig::parse(["db-migrate".to_string()]).unwrap();
        assert_eq!(default.path, PathBuf::from(DEFAULT_DOLT_DB_PATH));
        assert_eq!(
            default.spot_check_root,
            Some(DEFAULT_SPOT_CHECK_ROOT.to_string())
        );
    }

    #[test]
    fn unknown_argument_is_a_usage_error() {
        let result = DbMigrateConfig::parse(["db-migrate".to_string(), "--bogus".to_string()]);
        assert!(matches!(result, Err(QuizdomError::Usage(_))));
    }

    /// End-to-end against a real dolt binary: bootstrap a repo, migrate the
    /// scripted store into it twice (the second run proves idempotency), and
    /// let the real recursive CTE serve the spot-check. Ignored in CI (no
    /// dolt there); run locally with: cargo test real_dolt -- --ignored
    #[test]
    #[ignore = "requires the dolt binary on PATH"]
    fn real_dolt_migrate_is_idempotent_and_passes_parity() {
        let dir =
            std::env::temp_dir().join(format!("quizdom-db-migrate-real-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dolt = SystemDoltRunner::new("dolt".to_string());
        crate::db_init::run_db_init(
            [
                "db-init".to_string(),
                "--path".to_string(),
                dir.display().to_string(),
            ],
            &mut Vec::new(),
        )
        .expect("bootstrap should succeed");

        for run in ["first", "second (idempotency)"] {
            let mut output = Vec::new();
            db_migrate(
                &config(&dir, Some("Q-1")),
                &scripted_store(),
                &dolt,
                &mut output,
            )
            .unwrap_or_else(|error| panic!("{run} run failed: {error}"));
            let rendered = String::from_utf8(output).unwrap();
            assert!(rendered.contains("Parity OK."), "{run} run: {rendered}");
            assert!(rendered.contains("begets lineage of Q-1 reaches 2 nodes"));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
