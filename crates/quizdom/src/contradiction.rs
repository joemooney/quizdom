//! Contradiction detection across a user's adopted beliefs.
//!
//! This is a self-contained module (no session-loop edits) backing EPIC-9 /
//! STORY-57. Given the propositions a user has adopted — sourced from their
//! per-user session logs — it surfaces inconsistencies two ways:
//!
//! 1. **Graph-based** — two adopted beliefs joined by a `contradicts` edge in
//!    the AIDA bank are flagged directly (`aida rel list <node> --type
//!    contradicts`, walked one hop at a time per ADR-31).
//! 2. **LLM-based** — the full set of adopted beliefs is handed to an
//!    [`LLMClient`] which reports semantic inconsistencies the graph does not
//!    pre-encode (default claude-cli backend).
//!
//! A standalone `quizdom contradictions --user <id>` / `--session <id>` command
//! lists what it finds without touching the live session loop.

// trace:EPIC-9 | ai:claude
use crate::error::{QuizdomError, Result};
use llm::{LLMClient, Message};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Output};

const DEFAULT_USER: &str = "local-user";

const SEMANTIC_SYSTEM_PROMPT: &str = "You are quizdom's contradiction detector. There are no correct answers; your job is only to spot propositions the user has adopted that cannot comfortably be held together under the same definitions. Be conservative: surface a pair only when the tension is genuine, not merely a difference in topic.";

/// A proposition the user has adopted.
///
/// `id` is the graph node (e.g. `BELIEF-7` or the `Q-*` whose answer encoded
/// the position) when the belief is graph-backed; it is `None` for raw session
/// positions that have not been promoted. Graph detection only applies to
/// beliefs that carry an `id`.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AdoptedBelief {
    pub id: Option<String>,
    pub statement: String,
    pub source: String,
}

impl AdoptedBelief {
    /// A short label for display and de-duplication, preferring the statement
    /// text and falling back to the node id.
    fn label(&self) -> String {
        if self.statement.trim().is_empty() {
            self.id.clone().unwrap_or_default()
        } else {
            self.statement.clone()
        }
    }
}

/// How a contradiction was detected.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ContradictionKind {
    Graph,
    Semantic,
}

impl ContradictionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Graph => "graph",
            Self::Semantic => "semantic",
        }
    }
}

/// A detected conflict between two adopted beliefs.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Contradiction {
    pub kind: ContradictionKind,
    pub left_id: Option<String>,
    pub left: String,
    pub right_id: Option<String>,
    pub right: String,
    pub explanation: String,
}

impl Contradiction {
    /// Order-independent identity of the pair, used to de-duplicate findings
    /// that surface from both detectors.
    fn pair_key(&self) -> (String, String) {
        unordered_pair(self.left.clone(), self.right.clone())
    }
}

fn unordered_pair(left: String, right: String) -> (String, String) {
    if left <= right {
        (left, right)
    } else {
        (right, left)
    }
}

/// Reads the `contradicts` neighbours of a belief node. Abstracted so detection
/// can be unit-tested without shelling out to `aida`.
pub trait ContradictsEdges {
    fn contradicts(&self, belief_id: &str) -> Result<Vec<String>>;
}

/// Resolves `contradicts` edges by shelling out to the `aida` CLI, one hop at a
/// time (ADR-31: `aida graph` cannot follow custom edges).
pub struct AidaCliContradictsEdges {
    command: String,
}

impl Default for AidaCliContradictsEdges {
    fn default() -> Self {
        Self {
            command: "aida".to_string(),
        }
    }
}

impl ContradictsEdges for AidaCliContradictsEdges {
    fn contradicts(&self, belief_id: &str) -> Result<Vec<String>> {
        let output = Command::new(&self.command)
            .args(["rel", "list", belief_id, "--type", "contradicts"])
            .output()?;
        if !output.status.success() {
            return Err(QuizdomError::Aida(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }
        Ok(parse_contradicts_rel_list(&String::from_utf8_lossy(
            &output.stdout,
        )))
    }
}

/// Parses the `to` column of `aida rel list <node> --type contradicts` output.
pub fn parse_contradicts_rel_list(output: &str) -> Vec<String> {
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
            (relationship_type == "contradicts").then(|| to.to_string())
        })
        .collect()
}

/// Flags pairs of adopted beliefs joined by a `contradicts` edge in the bank.
pub fn detect_graph_contradictions(
    beliefs: &[AdoptedBelief],
    edges: &dyn ContradictsEdges,
) -> Result<Vec<Contradiction>> {
    // Map each adopted graph node id to its display label. A user may adopt the
    // same node across several sessions, so de-duplicate ids up front.
    let mut adopted: BTreeMap<String, String> = BTreeMap::new();
    for belief in beliefs {
        if let Some(id) = &belief.id {
            adopted.entry(id.clone()).or_insert_with(|| belief.label());
        }
    }

    let mut seen_pairs: BTreeSet<(String, String)> = BTreeSet::new();
    let mut contradictions = Vec::new();
    for (id, label) in &adopted {
        for neighbour in edges.contradicts(id)? {
            let Some(neighbour_label) = adopted.get(&neighbour) else {
                continue;
            };
            let pair = unordered_pair(id.clone(), neighbour.clone());
            if !seen_pairs.insert(pair) {
                continue;
            }
            contradictions.push(Contradiction {
                kind: ContradictionKind::Graph,
                left_id: Some(id.clone()),
                left: label.clone(),
                right_id: Some(neighbour.clone()),
                right: neighbour_label.clone(),
                explanation: format!(
                    "Adopted beliefs {id} and {neighbour} are joined by a `contradicts` edge in the bank."
                ),
            });
        }
    }
    Ok(contradictions)
}

/// Asks an [`LLMClient`] to report semantic inconsistencies among the adopted
/// beliefs. Returns an empty list when there are fewer than two beliefs to
/// compare.
pub fn detect_semantic_contradictions<C>(
    client: &C,
    beliefs: &[AdoptedBelief],
) -> Result<Vec<Contradiction>>
where
    C: LLMClient,
{
    if beliefs.len() < 2 {
        return Ok(Vec::new());
    }
    let prompt = semantic_prompt(beliefs);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(QuizdomError::Io)?;
    let (text, _tool_calls) = runtime
        .block_on(client.call(SEMANTIC_SYSTEM_PROMPT, &[Message::user(prompt)], &[]))
        .map_err(|error| QuizdomError::Aida(error.to_string()))?;
    parse_semantic_contradictions(&text, beliefs)
}

pub(crate) fn semantic_prompt(beliefs: &[AdoptedBelief]) -> String {
    let mut prompt = String::from("Beliefs the user has adopted:\n");
    for (index, belief) in beliefs.iter().enumerate() {
        prompt.push_str(&format!("[{index}] {}\n", belief.label()));
    }
    prompt.push_str(
        "\nReturn only JSON: {\"contradictions\":[{\"a\":<index>,\"b\":<index>,\"explanation\":\"why they conflict\"}]}. Reference beliefs by their [index]. Only include pairs that are genuinely semantically inconsistent under shared definitions. Use an empty list if none.",
    );
    prompt
}

pub(crate) fn parse_semantic_contradictions(
    text: &str,
    beliefs: &[AdoptedBelief],
) -> Result<Vec<Contradiction>> {
    let value: Value = serde_json::from_str(text.trim())
        .map_err(|error| QuizdomError::Parse(format!("invalid contradiction JSON: {error}")))?;
    let Some(items) = value.get("contradictions").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };

    let mut seen_pairs: BTreeSet<(String, String)> = BTreeSet::new();
    let mut contradictions = Vec::new();
    for item in items {
        let Some(a) = belief_index(item, "a", beliefs.len()) else {
            continue;
        };
        let Some(b) = belief_index(item, "b", beliefs.len()) else {
            continue;
        };
        if a == b {
            continue;
        }
        let left = beliefs[a].label();
        let right = beliefs[b].label();
        if !seen_pairs.insert(unordered_pair(left.clone(), right.clone())) {
            continue;
        }
        let explanation = item
            .get("explanation")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        contradictions.push(Contradiction {
            kind: ContradictionKind::Semantic,
            left_id: beliefs[a].id.clone(),
            left,
            right_id: beliefs[b].id.clone(),
            right,
            explanation,
        });
    }
    Ok(contradictions)
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ContradictionResolution {
    pub graph_ref: Option<String>,
    pub kept_side: String,
}

pub trait ContradictionResolutionPersister {
    fn persist_resolution(
        &self,
        contradiction: &Contradiction,
        raw_answer: &str,
    ) -> Result<Option<ContradictionResolution>>;
}

pub struct NoopContradictionResolutionPersister;

impl ContradictionResolutionPersister for NoopContradictionResolutionPersister {
    fn persist_resolution(
        &self,
        _contradiction: &Contradiction,
        _raw_answer: &str,
    ) -> Result<Option<ContradictionResolution>> {
        Ok(None)
    }
}

pub trait ResolutionCommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<Output>;
}

pub struct SystemResolutionCommandRunner;

impl ResolutionCommandRunner for SystemResolutionCommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<Output> {
        Command::new(program)
            .args(args)
            .output()
            .map_err(Into::into)
    }
}

pub struct AidaCliContradictionResolutionPersister<R = SystemResolutionCommandRunner> {
    command: String,
    runner: R,
}

impl Default for AidaCliContradictionResolutionPersister<SystemResolutionCommandRunner> {
    fn default() -> Self {
        Self {
            command: "aida".to_string(),
            runner: SystemResolutionCommandRunner,
        }
    }
}

impl<R> AidaCliContradictionResolutionPersister<R>
where
    R: ResolutionCommandRunner,
{
    #[cfg(test)]
    pub fn new(command: impl Into<String>, runner: R) -> Self {
        Self {
            command: command.into(),
            runner,
        }
    }
}

impl<R> ContradictionResolutionPersister for AidaCliContradictionResolutionPersister<R>
where
    R: ResolutionCommandRunner,
{
    fn persist_resolution(
        &self,
        contradiction: &Contradiction,
        raw_answer: &str,
    ) -> Result<Option<ContradictionResolution>> {
        // trace:STORY-59 | ai:codex
        let (Some(left_id), Some(right_id)) = (&contradiction.left_id, &contradiction.right_id)
        else {
            return Ok(None);
        };
        self.confirm_contradicts_edge(left_id, right_id)?;
        let kept_side = classify_kept_side(contradiction, raw_answer);
        let graph_ref = self.record_resolution_decision(
            left_id,
            right_id,
            &contradiction.left,
            &contradiction.right,
            raw_answer,
            &kept_side,
        )?;
        Ok(Some(ContradictionResolution {
            graph_ref: Some(graph_ref),
            kept_side,
        }))
    }
}

impl<R> AidaCliContradictionResolutionPersister<R>
where
    R: ResolutionCommandRunner,
{
    fn confirm_contradicts_edge(&self, left_id: &str, right_id: &str) -> Result<()> {
        let args = vec![
            "rel".to_string(),
            "add".to_string(),
            "--from".to_string(),
            left_id.to_string(),
            "--to".to_string(),
            right_id.to_string(),
            "--type".to_string(),
            "contradicts".to_string(),
        ];
        let output = self.runner.run(&self.command, &args)?;
        if output.status.success() || relationship_already_exists(&output) {
            return Ok(());
        }
        Err(QuizdomError::Aida(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ))
    }

    fn record_resolution_decision(
        &self,
        left_id: &str,
        right_id: &str,
        left: &str,
        right: &str,
        raw_answer: &str,
        kept_side: &str,
    ) -> Result<String> {
        let title = format!("Contradiction resolution: {left_id} vs {right_id}");
        let description = format!(
            "source: quizdom contradiction follow-up.\n\nleft: {left_id} -- {left}\n\nright: {right_id} -- {right}\n\nuser resolution: {raw_answer}\n\nkept side: {kept_side}"
        );
        let tags =
            format!("contradiction-resolution,kept:{kept_side},left:{left_id},right:{right_id}");
        let add_args = vec![
            "add".to_string(),
            "--type".to_string(),
            "decision".to_string(),
            "--status".to_string(),
            "approved".to_string(),
            "--priority".to_string(),
            "medium".to_string(),
            "--title".to_string(),
            title,
            "--description".to_string(),
            description,
            "--tags".to_string(),
            tags,
        ];
        let output = self.runner.run(&self.command, &add_args)?;
        if !output.status.success() {
            return Err(QuizdomError::Aida(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }
        let resolution_id = parse_added_resolution_id(&String::from_utf8_lossy(&output.stdout))?;
        self.reference_resolution(&resolution_id, left_id)?;
        self.reference_resolution(&resolution_id, right_id)?;
        Ok(resolution_id)
    }

    fn reference_resolution(&self, resolution_id: &str, target_id: &str) -> Result<()> {
        let args = vec![
            "rel".to_string(),
            "add".to_string(),
            "--from".to_string(),
            resolution_id.to_string(),
            "--to".to_string(),
            target_id.to_string(),
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

fn classify_kept_side(contradiction: &Contradiction, raw_answer: &str) -> String {
    let answer = raw_answer.trim().to_ascii_lowercase();
    let left_id = contradiction
        .left_id
        .as_deref()
        .unwrap_or("left")
        .to_ascii_lowercase();
    let right_id = contradiction
        .right_id
        .as_deref()
        .unwrap_or("right")
        .to_ascii_lowercase();
    if answer.contains("left")
        || answer.contains("first")
        || answer.contains(&left_id)
        || answer.contains(&contradiction.left.to_ascii_lowercase())
    {
        return "left".to_string();
    }
    if answer.contains("right")
        || answer.contains("second")
        || answer.contains(&right_id)
        || answer.contains(&contradiction.right.to_ascii_lowercase())
    {
        return "right".to_string();
    }
    "refinement".to_string()
}

fn parse_added_resolution_id(output: &str) -> Result<String> {
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

fn belief_index(item: &Value, key: &str, len: usize) -> Option<usize> {
    let index = item.get(key)?.as_u64()? as usize;
    (index < len).then_some(index)
}

/// Merges graph and semantic findings, with graph findings taking precedence
/// when both detectors surface the same pair of statements.
pub fn merge_contradictions(
    graph: Vec<Contradiction>,
    semantic: Vec<Contradiction>,
) -> Vec<Contradiction> {
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut merged = Vec::new();
    for contradiction in graph.into_iter().chain(semantic) {
        if seen.insert(contradiction.pair_key()) {
            merged.push(contradiction);
        }
    }
    merged
}

/// Builds adopted beliefs from a per-user session log (jsonl). Each
/// `answer_recorded` event becomes one adopted belief: the question text plus
/// the user's answer is the proposition, and the answered node id is carried so
/// graph detection can follow its `contradicts` edges.
pub fn beliefs_from_session_log(
    reader: impl Read,
    branch: Option<&str>,
) -> Result<Vec<AdoptedBelief>> {
    let reader = BufReader::new(reader);
    let mut question_text: BTreeMap<u64, String> = BTreeMap::new();
    let mut beliefs = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value =
            serde_json::from_str(&line).map_err(|error| QuizdomError::Parse(error.to_string()))?;
        let event_branch = value
            .get("branch_id")
            .and_then(Value::as_str)
            .unwrap_or("main");
        if let Some(branch) = branch {
            if event_branch != branch {
                continue;
            }
        }
        match value.get("event_type").and_then(Value::as_str) {
            Some("question_presented") => {
                if let (Some(turn), Some(text)) = (
                    value.get("turn").and_then(Value::as_u64),
                    value.get("question_text").and_then(Value::as_str),
                ) {
                    question_text.insert(turn, text.to_string());
                }
            }
            Some("answer_recorded") => {
                let Some(turn) = value.get("turn").and_then(Value::as_u64) else {
                    continue;
                };
                let Some(question_ref) = value.get("question_ref").and_then(Value::as_str) else {
                    continue;
                };
                let raw_answer = value
                    .get("raw_answer")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                let text = question_text.get(&turn).cloned().unwrap_or_default();
                let statement = match (text.is_empty(), raw_answer.is_empty()) {
                    (false, false) => format!("{text} → {raw_answer}"),
                    (false, true) => text.clone(),
                    (true, false) => raw_answer.to_string(),
                    (true, true) => continue,
                };
                beliefs.push(AdoptedBelief {
                    id: Some(question_ref.to_string()),
                    statement,
                    source: format!("{question_ref} (turn {turn}, branch {event_branch})"),
                });
            }
            _ => {}
        }
    }
    Ok(beliefs)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum LlmBackend {
    ClaudeCli,
    Anthropic,
    Disabled,
}

struct ContradictionsConfig {
    user_id: String,
    session_id: Option<String>,
    log_path: Option<PathBuf>,
    branch: Option<String>,
    backend: LlmBackend,
}

impl ContradictionsConfig {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut user_id = DEFAULT_USER.to_string();
        let mut session_id = None;
        let mut log_path = None;
        let mut branch = None;
        let mut backend = env_backend();
        let mut args = args.into_iter().peekable();

        if matches!(args.peek().map(String::as_str), Some("contradictions")) {
            args.next();
        }

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--user" => user_id = next_arg(&mut args, "--user")?,
                "--session" => session_id = Some(next_arg(&mut args, "--session")?),
                "--log" => log_path = Some(PathBuf::from(next_arg(&mut args, "--log")?)),
                "--branch" => branch = Some(next_arg(&mut args, "--branch")?),
                "--backend" => backend = parse_backend(&next_arg(&mut args, "--backend")?)?,
                "--no-llm" => backend = LlmBackend::Disabled,
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
            user_id,
            session_id,
            log_path,
            branch,
            backend,
        })
    }

    /// The log files to read: an explicit `--log`, a single `--session`, or
    /// every session recorded for `--user`.
    fn log_paths(&self) -> Result<Vec<PathBuf>> {
        if let Some(log_path) = &self.log_path {
            return Ok(vec![log_path.clone()]);
        }
        let sessions_dir = PathBuf::from("data")
            .join("users")
            .join(&self.user_id)
            .join("sessions");
        if let Some(session_id) = &self.session_id {
            return Ok(vec![sessions_dir.join(format!("{session_id}.jsonl"))]);
        }
        if !sessions_dir.exists() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(&sessions_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                paths.push(path);
            }
        }
        paths.sort();
        Ok(paths)
    }
}

fn env_backend() -> LlmBackend {
    std::env::var("QUIZDOM_BACKEND")
        .ok()
        .and_then(|value| parse_backend(&value).ok())
        .unwrap_or(LlmBackend::ClaudeCli)
}

fn parse_backend(value: &str) -> Result<LlmBackend> {
    match value {
        "claude-cli" | "claude_cli" | "claude" => Ok(LlmBackend::ClaudeCli),
        "anthropic" => Ok(LlmBackend::Anthropic),
        "none" | "off" | "disabled" => Ok(LlmBackend::Disabled),
        other => Err(QuizdomError::Usage(format!(
            "unknown LLM backend: {other}; expected claude-cli, anthropic, or none"
        ))),
    }
}

fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| QuizdomError::Usage(format!("{name} requires a value")))
}

fn usage() -> String {
    "usage: quizdom contradictions [--user local-user] [--session sess-id] [--log path] [--branch main] [--backend claude-cli|anthropic|none] [--no-llm]"
        .to_string()
}

/// Entry point for the standalone `quizdom contradictions` command. Reads the
/// user's adopted beliefs from their session log(s), runs graph + (optionally)
/// LLM semantic detection, and prints the findings.
pub fn run_contradictions(
    args: impl IntoIterator<Item = String>,
    output: &mut impl Write,
) -> Result<()> {
    let config = ContradictionsConfig::parse(args)?;
    let beliefs = load_beliefs(&config)?;

    if beliefs.is_empty() {
        writeln!(output, "No adopted beliefs found to analyze.")?;
        return Ok(());
    }

    let edges = AidaCliContradictsEdges::default();
    let graph = detect_graph_contradictions(&beliefs, &edges)?;
    let semantic = detect_semantic(&config, &beliefs)?;
    let contradictions = merge_contradictions(graph, semantic);

    render_contradictions(&contradictions, output)
}

fn load_beliefs(config: &ContradictionsConfig) -> Result<Vec<AdoptedBelief>> {
    let mut beliefs = Vec::new();
    for path in config.log_paths()? {
        if !path.exists() {
            continue;
        }
        let file = std::fs::File::open(&path)?;
        beliefs.extend(beliefs_from_session_log(file, config.branch.as_deref())?);
    }
    Ok(beliefs)
}

fn detect_semantic(
    config: &ContradictionsConfig,
    beliefs: &[AdoptedBelief],
) -> Result<Vec<Contradiction>> {
    match config.backend {
        LlmBackend::Disabled => Ok(Vec::new()),
        LlmBackend::ClaudeCli => {
            let client = llm::ClaudeCliClient::from_env();
            // A missing or misconfigured backend should not sink the graph
            // findings; degrade to graph-only.
            Ok(detect_semantic_contradictions(&client, beliefs).unwrap_or_default())
        }
        LlmBackend::Anthropic => match llm::AnthropicClient::from_env() {
            Ok(client) => Ok(detect_semantic_contradictions(&client, beliefs).unwrap_or_default()),
            Err(_) => Ok(Vec::new()),
        },
    }
}

fn render_contradictions(contradictions: &[Contradiction], output: &mut impl Write) -> Result<()> {
    if contradictions.is_empty() {
        writeln!(output, "No contradictions detected.")?;
        return Ok(());
    }
    writeln!(
        output,
        "Detected {} contradiction(s):",
        contradictions.len()
    )?;
    for (index, contradiction) in contradictions.iter().enumerate() {
        writeln!(
            output,
            "\n{}. [{}] {}",
            index + 1,
            contradiction.kind.as_str(),
            contradiction.left
        )?;
        writeln!(output, "   <-> {}", contradiction.right)?;
        if !contradiction.explanation.is_empty() {
            writeln!(output, "   {}", contradiction.explanation)?;
        }
    }
    Ok(())
}
