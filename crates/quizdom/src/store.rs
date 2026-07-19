// trace:STORY-204 | ai:claude
//! The storage abstraction for quizdom's domain graph (EPIC-202 / ADR-201).
//!
//! [`DomainStore`] is the single seam through which the app reads and writes
//! domain-graph data — `Q-*` questions, `TERM-*` definitions, decision nodes,
//! and the custom edges joining them. Everything above this trait speaks in
//! graph vocabulary (nodes, one-hop neighbours per ADR-31, tags carrying the
//! ADR-22 `weight:N`); everything below it is backend plumbing.
//!
//! [`AidaDomainStore`] is the first backend: it shells out to the `aida` CLI
//! through the BUG-200 pinned-format choke point and screen-scrapes the human
//! output, exactly as the pre-STORY-204 call sites did. A Dolt-backed
//! implementation lands in STORY-207; no Dolt dependency exists in this slice.

use crate::aida_cmd::aida_command;
use crate::error::{QuizdomError, Result};
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

/// The kind of a domain-graph node, deciding how a backend materialises it
/// (id prefix, storage type) — not what the app does with it.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum NodeKind {
    /// A `Q-*` question node.
    Question,
    /// A `TERM-*` definition node.
    Term,
    /// A contradiction-resolution decision node.
    Decision,
}

/// The custom edge types of the domain graph (see
/// `docs/architecture/graph-schema.md`), plus the built-in `references` edge
/// used to link resolution decisions back to the nodes they arbitrate.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum EdgeKind {
    Begets,
    Probes,
    Refines,
    Contradicts,
    Agrees,
    Disagrees,
    References,
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
            Self::References => "references",
        }
    }
}

/// A domain-graph node as stored: identity, title, tags, the ADR-22 weight
/// (taken from the `weight:N` tag by the aida backend; a numeric column once
/// Dolt lands), and the node's descriptive body text.
///
/// `body` may carry a backend-specific envelope around the description (the
/// aida backend hands back the full `aida show` text); consumers extract what
/// they need with pure helpers rather than assuming a shape.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct NodeRecord {
    pub id: String,
    pub title: String,
    pub tags: Vec<String>,
    pub weight: u32,
    pub body: String,
}

/// A node to be created in the domain graph.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct NewNode {
    pub kind: NodeKind,
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
}

/// Every domain-graph operation quizdom performs, behind one storage
/// abstraction. Backends implement this; the app depends only on the trait.
pub trait DomainStore {
    /// Fetch a single node by id.
    fn fetch_node(&self, id: &str) -> Result<NodeRecord>;

    /// List the ids of every node of `kind` in the bank.
    fn list_node_ids(&self, kind: NodeKind) -> Result<Vec<String>>;

    /// The targets of `id`'s outgoing `edge` edges — one hop, per ADR-31.
    fn neighbors(&self, id: &str, edge: EdgeKind) -> Result<Vec<String>>;

    /// Create a node, returning its freshly minted id.
    fn create_node(&self, node: &NewNode) -> Result<String>;

    /// Create an edge; an already-existing edge is an error.
    fn create_edge(&self, from: &str, to: &str, edge: EdgeKind) -> Result<()>;

    /// Create an edge if it does not already exist; idempotent.
    fn ensure_edge(&self, from: &str, to: &str, edge: EdgeKind) -> Result<()>;

    /// Replace a node's full tag list (the ADR-22 weight-write path — the new
    /// list carries the recomputed `weight:N`).
    fn replace_tags(&self, id: &str, tags: &[String]) -> Result<()>;
}

/// The aida CLI backend: every operation shells out to `aida` and parses its
/// human-layout output. This is the only place in the crate that knows how
/// domain data maps onto `aida` subcommands.
pub struct AidaDomainStore<R = SystemCommandRunner> {
    command: String,
    pub(crate) runner: R,
}

impl Default for AidaDomainStore<SystemCommandRunner> {
    fn default() -> Self {
        Self {
            command: "aida".to_string(),
            runner: SystemCommandRunner,
        }
    }
}

impl<R> AidaDomainStore<R>
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

impl<R> DomainStore for AidaDomainStore<R>
where
    R: CommandRunner,
{
    fn fetch_node(&self, id: &str) -> Result<NodeRecord> {
        let output = self.run_ok(vec!["show".to_string(), id.to_string()])?;
        parse_node_show(&String::from_utf8_lossy(&output.stdout))
    }

    fn list_node_ids(&self, kind: NodeKind) -> Result<Vec<String>> {
        match kind {
            NodeKind::Question => {
                let output = self.run_ok(vec![
                    "list".to_string(),
                    "--type".to_string(),
                    "functional".to_string(),
                    "--no-scope".to_string(),
                ])?;
                Ok(parse_question_list_ids(&String::from_utf8_lossy(
                    &output.stdout,
                )))
            }
            other => Err(QuizdomError::Aida(format!(
                "listing {other:?} nodes is not supported by the aida backend"
            ))),
        }
    }

    fn neighbors(&self, id: &str, edge: EdgeKind) -> Result<Vec<String>> {
        let output = self.run_ok(vec![
            "rel".to_string(),
            "list".to_string(),
            id.to_string(),
            "--type".to_string(),
            edge.as_str().to_string(),
        ])?;
        Ok(parse_rel_list(
            &String::from_utf8_lossy(&output.stdout),
            edge.as_str(),
        ))
    }

    fn create_node(&self, node: &NewNode) -> Result<String> {
        let mut args = vec!["add".to_string()];
        match node.kind {
            NodeKind::Question => {
                args.extend(["--prefix", "Q", "--type", "functional"].map(String::from));
            }
            NodeKind::Term => args.extend(["--type", "term"].map(String::from)),
            NodeKind::Decision => args.extend(["--type", "decision"].map(String::from)),
        }
        args.extend(["--status", "approved", "--priority", "medium"].map(String::from));
        args.extend([
            "--title".to_string(),
            node.title.clone(),
            "--description".to_string(),
            node.description.clone(),
            "--tags".to_string(),
            node.tags.join(","),
        ]);
        let output = self.run_ok(args)?;
        parse_added_node_id(&String::from_utf8_lossy(&output.stdout), node.kind)
    }

    fn create_edge(&self, from: &str, to: &str, edge: EdgeKind) -> Result<()> {
        self.run_ok(rel_add_args(from, to, edge)).map(|_| ())
    }

    fn ensure_edge(&self, from: &str, to: &str, edge: EdgeKind) -> Result<()> {
        let output = self
            .runner
            .run(&self.command, &rel_add_args(from, to, edge))?;
        if output.status.success() || relationship_already_exists(&output) {
            return Ok(());
        }
        Err(QuizdomError::Aida(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ))
    }

    fn replace_tags(&self, id: &str, tags: &[String]) -> Result<()> {
        self.run_ok(vec![
            "edit".to_string(),
            id.to_string(),
            "--tags".to_string(),
            tags.join(","),
        ])
        .map(|_| ())
    }
}

fn rel_add_args(from: &str, to: &str, edge: EdgeKind) -> Vec<String> {
    vec![
        "rel".to_string(),
        "add".to_string(),
        "--from".to_string(),
        from.to_string(),
        "--to".to_string(),
        to.to_string(),
        "--type".to_string(),
        edge.as_str().to_string(),
    ]
}

fn relationship_already_exists(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("already") || stderr.contains("duplicate") || stderr.contains("exists")
}

/// Parse the human layout of `aida show <id>` into a [`NodeRecord`]. The full
/// show text rides along as the record's `body` so term-definition extraction
/// keeps working on whatever the description section contains.
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

/// Parse the `to` column of `aida rel list <id> --type <edge>` output,
/// keeping only rows of `expected_type`.
pub(crate) fn parse_rel_list(output: &str, expected_type: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty()
                || trimmed.starts_with("FROM")
                || trimmed.starts_with("(no outgoing")
                || trimmed.ends_with("edges")
            {
                return None;
            }
            let mut columns = trimmed.split_whitespace();
            let _from = columns.next()?;
            let relationship_type = columns.next()?;
            let to = columns.next()?;
            (relationship_type == expected_type).then(|| to.to_string())
        })
        .collect()
}

// trace:STORY-53 | ai:codex
/// Parse the `Q-*` ids out of `aida list` output, one per line.
pub(crate) fn parse_question_list_ids(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let id = line.split_whitespace().next()?;
            id.starts_with("Q-").then(|| id.to_string())
        })
        .collect()
}

/// Extract the freshly minted id from `aida add` output. Each kind keeps the
/// exact token-matching (and error text) its pre-STORY-204 call site used.
fn parse_added_node_id(output: &str, kind: NodeKind) -> Result<String> {
    match kind {
        NodeKind::Question => output
            .split(|character: char| character.is_whitespace() || character == ':')
            .find(|token| token.starts_with("Q-"))
            .map(str::to_string)
            .ok_or_else(|| QuizdomError::Parse("aida add output did not include Q id".to_string())),
        NodeKind::Term => output
            .split(|character: char| character.is_whitespace() || character == ':')
            .find(|token| token.starts_with("TERM-"))
            .map(str::to_string)
            .ok_or_else(|| {
                QuizdomError::Parse("aida add output did not include TERM id".to_string())
            }),
        NodeKind::Decision => output
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
            }),
    }
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
