// trace:STORY-204 | ai:claude
//! The storage abstraction for quizdom's domain graph (EPIC-202 / ADR-201).
//!
//! [`DomainStore`] is the single seam through which the app reads and writes
//! domain-graph data — `Q-*` questions, `TERM-*` definitions, and the custom
//! edges joining them. Since the STORY-208 cutover the only backend is the
//! Dolt store ([`crate::dolt_store::DoltDomainStore`]): multi-hop traversal is
//! the backend's recursive CTE (retiring ADR-31's app-side per-hop walk) and
//! the selection weight is a numeric column (retiring ADR-22's `weight:N`
//! tag encoding).
//!
//! What survives of the aida CLI here is [`AidaIntentStore`]: contradiction-
//! resolution decision nodes and their `references` edges are project intent,
//! which stays AIDA-canonical per ADR-201. [`parse_node_show`] also stays —
//! the STORY-206 exporter reads the legacy store through it during migration.

use crate::aida_cmd::aida_command;
use crate::error::{QuizdomError, Result};
use std::collections::BTreeMap;
use std::process::Output;

/// Runs an external command and captures its output. Abstracted so backends
/// can be unit-tested without spawning real processes.
pub trait CommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<Output>;
}

/// The real runner: spawns via the BUG-200 pinned-format choke point.
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    // trace:BUG-200 | ai:claude — spawn via the pinned-format choke point.
    fn run(&self, program: &str, args: &[String]) -> Result<Output> {
        aida_command(program)
            .args(args)
            .output()
            .map_err(Into::into)
    }
}

/// The kind of a domain-graph node, deciding how the backend materialises it
/// (id prefix, storage type) — not what the app does with it.
///
/// Contradiction-resolution decision nodes are not domain nodes: they are
/// project intent, written through [`IntentStore`] instead (ADR-201).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum NodeKind {
    /// A `Q-*` question node.
    Question,
    /// A `TERM-*` definition node.
    Term,
}

/// The custom edge types of the domain graph (see
/// `docs/architecture/graph-schema.md`). The built-in `references` edge joins
/// AIDA-side decision nodes and goes through [`IntentStore`], not here.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum EdgeKind {
    Begets,
    Probes,
    Refines,
    Contradicts,
    Agrees,
    Disagrees,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Begets => "begets",
            Self::Probes => "probes",
            Self::Refines => "refines",
            Self::Contradicts => "contradicts",
            Self::Agrees => "agrees",
            Self::Disagrees => "disagrees",
        }
    }
}

/// A domain-graph node as stored: identity, title, tags, the numeric
/// selection weight (the `weight` column since ADR-201), and the node's
/// descriptive body text.
///
/// `body` may carry a backend-specific envelope around the description;
/// consumers extract what they need with pure helpers rather than assuming a
/// shape.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct NodeRecord {
    pub id: String,
    pub title: String,
    pub tags: Vec<String>,
    pub weight: u32,
    pub body: String,
}

/// A node to be created in the domain graph. The selection weight is a
/// first-class field — it lands in the backend's numeric `weight` column,
/// never in the tag list (STORY-208 retired the ADR-22 `weight:N` tag).
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct NewNode {
    pub kind: NodeKind,
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    pub weight: u32,
}

/// Every domain-graph operation quizdom performs, behind one storage
/// abstraction. The Dolt backend is the only implementation since the
/// STORY-208 cutover; the trait remains the app's seam (and the test seam).
pub trait DomainStore {
    /// Fetch a single node by id.
    fn fetch_node(&self, id: &str) -> Result<NodeRecord>;

    /// List the ids of every node of `kind` in the bank.
    fn list_node_ids(&self, kind: NodeKind) -> Result<Vec<String>>;

    /// The targets of `id`'s outgoing `edge` edges — one hop.
    fn neighbors(&self, id: &str, edge: EdgeKind) -> Result<Vec<String>>;

    /// Create a node, returning its freshly minted id.
    fn create_node(&self, node: &NewNode) -> Result<String>;

    /// Create an edge; an already-existing edge is an error.
    fn create_edge(&self, from: &str, to: &str, edge: EdgeKind) -> Result<()>;

    /// Create an edge if it does not already exist; idempotent.
    fn ensure_edge(&self, from: &str, to: &str, edge: EdgeKind) -> Result<()>;

    // trace:STORY-208 | ai:claude
    /// Set a node's selection weight and replace its full tag list in one
    /// write — the re-weighting path: the recomputed weight goes to the
    /// numeric column, the rewritten `quality:*` tag rides in the tag list.
    fn update_weight_and_tags(&self, id: &str, weight: u32, tags: &[String]) -> Result<()>;

    // trace:STORY-207 | ai:claude
    /// Every node reachable from `root` over `edge` edges — `root` included,
    /// deduplicated, sorted. Cycle-safe.
    ///
    /// The Dolt backend runs this as a single recursive CTE (STORY-208
    /// deleted the ADR-31 per-hop default that walked [`Self::neighbors`]).
    fn reachable(&self, root: &str, edge: EdgeKind) -> Result<Vec<String>>;

    // trace:STORY-244 | ai:claude
    /// Fetch many nodes at once.
    ///
    /// Contract parity with looping [`Self::fetch_node`]: one record per
    /// requested id, in `ids` order, repeated ids repeated in the output, and
    /// an id with no row is an error. An empty `ids` does no work at all.
    ///
    /// The default loops the per-item read; a backend overrides it with a
    /// set-based query so the loop costs one round trip rather than `n`.
    fn fetch_nodes(&self, ids: &[String]) -> Result<Vec<NodeRecord>> {
        ids.iter().map(|id| self.fetch_node(id)).collect()
    }

    // trace:STORY-244 | ai:claude
    /// Every node of `kind`, id-ordered — the set-based form of
    /// [`Self::list_node_ids`] followed by a fetch per id, which is what the
    /// default does.
    fn list_nodes(&self, kind: NodeKind) -> Result<Vec<NodeRecord>> {
        let ids = self.list_node_ids(kind)?;
        self.fetch_nodes(&ids)
    }

    // trace:STORY-244 | ai:claude
    /// The one-hop `edge` targets of many sources at once.
    ///
    /// The map is total over `ids` — a source with no such edges maps to an
    /// empty vec — and each vec carries the ordering [`Self::neighbors`]
    /// guarantees (TASK-221: `created_at`, ties broken by `to_id`).
    fn neighbors_many(
        &self,
        ids: &[String],
        edge: EdgeKind,
    ) -> Result<BTreeMap<String, Vec<String>>> {
        ids.iter()
            .map(|id| Ok((id.clone(), self.neighbors(id, edge)?)))
            .collect()
    }

    // trace:STORY-244 | ai:claude
    /// Apply many `(id, weight, tags)` re-weights as one write.
    ///
    /// A repeated id takes its last entry, matching the last-write-wins of
    /// looping [`Self::update_weight_and_tags`] — which is the default.
    fn update_weights(&self, updates: &[(String, u32, Vec<String>)]) -> Result<()> {
        for (id, weight, tags) in updates {
            self.update_weight_and_tags(id, *weight, tags)?;
        }
        Ok(())
    }
}

// trace:STORY-208 | ai:claude
/// The AIDA-canonical intent writes that survive the Dolt cutover (ADR-201):
/// contradiction-resolution decision nodes and the `references` edges linking
/// a decision to the nodes it arbitrates. Everything else the aida CLI used
/// to store is domain data and lives in Dolt.
pub trait IntentStore {
    /// Create a contradiction-resolution decision node, returning its id.
    fn create_decision_node(
        &self,
        title: &str,
        description: &str,
        tags: &[String],
    ) -> Result<String>;

    /// Link a decision node to a node it arbitrates; idempotent.
    fn ensure_references_edge(&self, from: &str, to: &str) -> Result<()>;
}

/// The aida CLI intent store: decision nodes and `references` edges shell out
/// to `aida`. This is the only place in the crate that still writes through
/// the aida CLI at runtime.
pub struct AidaIntentStore<R = SystemCommandRunner> {
    command: String,
    pub(crate) runner: R,
}

impl Default for AidaIntentStore<SystemCommandRunner> {
    fn default() -> Self {
        Self {
            command: "aida".to_string(),
            runner: SystemCommandRunner,
        }
    }
}

impl<R> AidaIntentStore<R>
where
    R: CommandRunner,
{
    #[cfg(test)]
    pub(crate) fn new(command: impl Into<String>, runner: R) -> Self {
        Self {
            command: command.into(),
            runner,
        }
    }

    fn run_ok(&self, args: Vec<String>) -> Result<Output> {
        let output = self.runner.run(&self.command, &args)?;
        if !output.status.success() {
            return Err(QuizdomError::Aida(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }
        Ok(output)
    }
}

impl<R> IntentStore for AidaIntentStore<R>
where
    R: CommandRunner,
{
    fn create_decision_node(
        &self,
        title: &str,
        description: &str,
        tags: &[String],
    ) -> Result<String> {
        let mut args = vec!["add".to_string()];
        args.extend(["--type", "decision"].map(String::from));
        args.extend(["--status", "approved", "--priority", "medium"].map(String::from));
        args.extend([
            "--title".to_string(),
            title.to_string(),
            "--description".to_string(),
            description.to_string(),
            "--tags".to_string(),
            tags.join(","),
        ]);
        let output = self.run_ok(args)?;
        parse_decision_id(&String::from_utf8_lossy(&output.stdout))
    }

    fn ensure_references_edge(&self, from: &str, to: &str) -> Result<()> {
        let args = vec![
            "rel".to_string(),
            "add".to_string(),
            "--from".to_string(),
            from.to_string(),
            "--to".to_string(),
            to.to_string(),
            "--type".to_string(),
            "references".to_string(),
        ];
        let output = self.runner.run(&self.command, &args)?;
        if output.status.success() || relationship_already_exists(&output) {
            return Ok(());
        }
        Err(QuizdomError::Aida(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ))
    }
}

fn relationship_already_exists(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("already") || stderr.contains("duplicate") || stderr.contains("exists")
}

/// Parse the human layout of `aida show <id>` into a [`NodeRecord`]. Used
/// only by the STORY-206 exporter, which reads the legacy AIDA store during
/// migration — including the legacy ADR-22 `weight:N` tag it converts into
/// the numeric `weight` column. The full show text rides along as the
/// record's `body` so description extraction keeps working on whatever the
/// description section contains.
pub(crate) fn parse_node_show(output: &str) -> Result<NodeRecord> {
    let id = prefixed_line(output, "ID:")
        .ok_or_else(|| QuizdomError::Parse("aida show output missing ID".to_string()))?;
    let title = prefixed_line(output, "Title:")
        .ok_or_else(|| QuizdomError::Parse("aida show output missing Title".to_string()))?;
    let tags = split_tags(&prefixed_line(output, "Tags:").unwrap_or_default());
    let weight = tags
        .iter()
        .find_map(|tag| tag.strip_prefix("weight:")?.parse::<u32>().ok())
        .unwrap_or(0);
    Ok(NodeRecord {
        id,
        title,
        tags,
        weight,
        body: output.to_string(),
    })
}

/// Extract the freshly minted decision id from `aida add` output, keeping the
/// exact token-matching (and error text) the pre-STORY-204 call site used.
fn parse_decision_id(output: &str) -> Result<String> {
    output
        .split_whitespace()
        .find_map(|word| {
            let candidate = word.trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && character != '-'
            });
            (candidate.contains('-')
                && candidate
                    .chars()
                    .any(|character| character.is_ascii_digit()))
            .then(|| candidate.to_string())
        })
        .ok_or_else(|| {
            QuizdomError::Parse("aida add output did not include a resolution id".to_string())
        })
}

fn prefixed_line(output: &str, prefix: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix(prefix).map(str::trim))
        .map(str::to_string)
}

/// Split a `Tags:` line on top-level commas, keeping bracketed groups (e.g.
/// `answer:choice[a, b]`) intact.
fn split_tags(line: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let mut current = String::new();
    let mut bracket_depth = 0_u32;

    for character in line.chars() {
        match character {
            '[' => {
                bracket_depth += 1;
                current.push(character);
            }
            ']' => {
                bracket_depth = bracket_depth.saturating_sub(1);
                current.push(character);
            }
            ',' if bracket_depth == 0 => {
                let tag = current.trim();
                if !tag.is_empty() {
                    tags.push(tag.to_string());
                }
                current.clear();
            }
            _ => current.push(character),
        }
    }

    let tag = current.trim();
    if !tag.is_empty() {
        tags.push(tag.to_string());
    }

    tags
}
