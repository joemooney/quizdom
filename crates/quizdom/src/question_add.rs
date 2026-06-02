//! Standalone `quizdom question add` command (STORY-87).
//!
//! Authors a single question end to end: prompt for the question text and
//! answer shape, run the STORY-86 DEDUP/REFINE approve flow, and persist the
//! result as a real Q-object via the STORY-85 user-authored persister.
//!
//! The command mirrors the `quizdom curate` / `quizdom contradictions` dispatch
//! in `main.rs`: a thin public [`run_question_add`] entry point wires the real
//! AIDA-backed bank + persister and an LLM strategy, then defers to a
//! [`question_add`] seam that takes its collaborators by trait object so tests
//! can drive the whole flow with fakes.
//!
//! Degrades gracefully offline / non-TTY: the dedup search is pure (always
//! runs), a missing or failing LLM yields no refinement (the question is added
//! verbatim), and all prompting reads from the supplied reader so a piped /
//! redirected stdin works without a terminal.

// trace:STORY-87 | ai:claude

use crate::bank::{AidaCliQuestionBank, QuestionBank};
use crate::error::{QuizdomError, Result};
use crate::model::{AnswerKind, Question};
use crate::persist::{
    AidaCliUserAuthoredQuestionPersister, QuestionLink, UserAuthoredQuestionPersister,
};
use crate::strategy::{
    assist_user_question, DeterministicNextQuestionStrategy, LlmNextQuestionStrategy,
    NextQuestionStrategy, UserQuestionAssist,
};
use llm::{AnthropicClient, ClaudeCliClient};
use std::io::{BufRead, BufReader, Read, Write};

const DEFAULT_USER: &str = "local-user";

/// LLM backend selection for the REFINE step, mirroring the contradictions
/// command's `--backend` / `--no-llm` knobs.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum LlmBackend {
    ClaudeCli,
    Anthropic,
    Disabled,
}

/// Parsed `quizdom question add` invocation.
#[derive(Debug)]
struct QuestionAddConfig {
    /// User id the question is authored under (recorded in the `topic` and used
    /// as the default topic when none is derived). Reserved for future
    /// per-user authoring scopes; today it only colours the topic fallback.
    user_id: String,
    /// `--seed Q-N`: the new question `begets` from this origin question.
    seed: Option<String>,
    /// `--probes TERM-N`: the new question `probes` this term definition.
    probes: Option<String>,
    /// Explicit topic tag; falls back to the user id when omitted.
    topic: Option<String>,
    backend: LlmBackend,
}

impl QuestionAddConfig {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut user_id = DEFAULT_USER.to_string();
        let mut seed = None;
        let mut probes = None;
        let mut topic = None;
        let mut backend = env_backend();
        let mut args = args.into_iter().peekable();

        // Tolerate being handed the leading `question` / `add` tokens (main.rs
        // forwards the whole argv) or just the flags (tests).
        if matches!(args.peek().map(String::as_str), Some("question")) {
            args.next();
        }
        if matches!(args.peek().map(String::as_str), Some("add")) {
            args.next();
        }

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--user" => user_id = next_arg(&mut args, "--user")?,
                "--seed" => seed = Some(next_arg(&mut args, "--seed")?),
                "--probes" => probes = Some(next_arg(&mut args, "--probes")?),
                "--topic" => topic = Some(next_arg(&mut args, "--topic")?),
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

        if seed.is_some() && probes.is_some() {
            return Err(QuizdomError::Usage(
                "--seed and --probes are mutually exclusive: a new question either \
                 begets from a seed question or probes a term, not both"
                    .to_string(),
            ));
        }

        Ok(Self {
            user_id,
            seed,
            probes,
            topic,
            backend,
        })
    }

    /// The graph edge the freshly persisted question is wired with: a `--seed`
    /// makes it `begets` from that origin, a `--probes` makes it `probes` the
    /// term, and otherwise it is a standalone hand-authored seed.
    fn link(&self) -> QuestionLink {
        if let Some(origin_id) = &self.seed {
            QuestionLink::Begets {
                origin_id: origin_id.clone(),
            }
        } else if let Some(term_id) = &self.probes {
            QuestionLink::Probes {
                term_id: term_id.clone(),
            }
        } else {
            QuestionLink::Standalone
        }
    }

    /// Topic tag for the persisted question: an explicit `--topic`, else the
    /// authoring user id.
    fn topic(&self) -> String {
        self.topic.clone().unwrap_or_else(|| self.user_id.clone())
    }
}

fn next_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| QuizdomError::Usage(format!("{name} requires a value")))
}

fn usage() -> String {
    "usage: quizdom question add [--user local-user] [--seed Q-23] [--probes TERM-7] \
     [--topic name] [--backend claude-cli|anthropic|none] [--no-llm]"
        .to_string()
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

/// Build the REFINE strategy for the approve flow. `Disabled` (and a
/// misconfigured Anthropic backend) fall back to the deterministic strategy,
/// whose `refine_user_question` is a no-op — so the flow adds questions
/// verbatim offline.
fn build_strategy(backend: LlmBackend) -> Box<dyn NextQuestionStrategy> {
    match backend {
        LlmBackend::Disabled => Box::new(DeterministicNextQuestionStrategy),
        LlmBackend::ClaudeCli => {
            Box::new(LlmNextQuestionStrategy::new(ClaudeCliClient::from_env()))
        }
        LlmBackend::Anthropic => match AnthropicClient::from_env() {
            Ok(client) => Box::new(LlmNextQuestionStrategy::new(client)),
            Err(_) => Box::new(DeterministicNextQuestionStrategy),
        },
    }
}

/// Public entry point for the standalone `quizdom question add` command.
///
/// Wires the real AIDA-backed [`AidaCliQuestionBank`] and
/// [`AidaCliUserAuthoredQuestionPersister`] plus an LLM strategy, then defers to
/// the [`question_add`] seam. Reads prompts from `input` (so piped / non-TTY
/// stdin works) and writes to `output`.
// trace:STORY-87 | ai:claude
pub fn run_question_add(
    args: impl IntoIterator<Item = String>,
    input: impl Read,
    mut output: impl Write,
) -> Result<()> {
    let config = QuestionAddConfig::parse(args)?;
    let bank = AidaCliQuestionBank::default();
    // The dedup search is pure over the in-memory bank snapshot; an empty bank
    // (or an AIDA hiccup) simply yields no duplicate.
    let existing = bank.all_questions().unwrap_or_default();
    let strategy = build_strategy(config.backend);
    let persister = AidaCliUserAuthoredQuestionPersister::default();
    let mut reader = BufReader::new(input);
    question_add(
        &config,
        &existing,
        strategy.as_ref(),
        &persister,
        &mut reader,
        &mut output,
    )
}

/// Testable core of the command: prompt for the question text + answer shape,
/// run the DEDUP/REFINE approve flow over `existing`, and persist via
/// `persister`. Takes its collaborators by trait object so tests drive the flow
/// with fakes (no AIDA, no terminal, no network).
// trace:STORY-87 | ai:claude
fn question_add(
    config: &QuestionAddConfig,
    existing: &[Question],
    strategy: &dyn NextQuestionStrategy,
    persister: &dyn UserAuthoredQuestionPersister,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<()> {
    let title = prompt_question_text(input, output)?;
    let answer_kind = prompt_answer_shape(input, output)?;

    // Approve flow (STORY-86): DEDUP then REFINE, collapsed into one decision.
    let assist = assist_user_question(strategy, &title, &answer_kind, existing);

    let candidate = match resolve_assist(assist, &title, &answer_kind, input, output)? {
        Some(candidate) => candidate,
        None => {
            // The user reused an existing duplicate instead of authoring a new
            // question, or aborted; nothing to persist.
            return Ok(());
        }
    };

    let link = config.link();
    // The persister derives the canonical tag set + neutral weight + real id
    // itself (STORY-85), so the in-memory question only needs its title and
    // answer shape; id / tags / weight are placeholders it overwrites.
    let draft = Question {
        id: String::new(),
        title: candidate.title,
        tags: Vec::new(),
        answer_kind: candidate.answer_kind,
        weight: 0,
    };
    let persisted = persister.persist_user_authored_question(&draft, &config.topic(), &link)?;
    render_persisted(&persisted, &link, output)
}

/// A drafted question the user has approved for persistence.
struct Candidate {
    title: String,
    answer_kind: AnswerKind,
}

/// Apply the DEDUP/REFINE decision, prompting the user where a choice is
/// required. Returns the question to persist, or `None` when the user reused an
/// existing duplicate (nothing new to persist).
fn resolve_assist(
    assist: UserQuestionAssist,
    title: &str,
    answer_kind: &AnswerKind,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<Option<Candidate>> {
    match assist {
        UserQuestionAssist::Duplicate(duplicate) => {
            writeln!(
                output,
                "A near-duplicate already exists ({}, similarity {:.0}%):",
                duplicate.question.id,
                duplicate.similarity * 100.0
            )?;
            writeln!(output, "  {}", duplicate.question.title)?;
            if prompt_yes_no(
                input,
                output,
                "Add your wording anyway instead of reusing it? [y/N] ",
                false,
            )? {
                Ok(Some(Candidate {
                    title: title.to_string(),
                    answer_kind: answer_kind.clone(),
                }))
            } else {
                writeln!(
                    output,
                    "Reusing {} — nothing new to add.",
                    duplicate.question.id
                )?;
                Ok(None)
            }
        }
        UserQuestionAssist::Refinement(proposal) => {
            writeln!(output, "Suggested refinement:")?;
            writeln!(output, "  {}", proposal.refined_title)?;
            writeln!(
                output,
                "  answer shape: {}",
                proposal.suggested_answer_kind.mode()
            )?;
            if proposal.weak_socratic {
                writeln!(
                    output,
                    "  (flagged as a weak Socratic prompt: {})",
                    proposal.rationale
                )?;
            } else if !proposal.rationale.is_empty() {
                writeln!(output, "  reason: {}", proposal.rationale)?;
            }
            if prompt_yes_no(input, output, "Adopt this refinement? [Y/n] ", true)? {
                Ok(Some(Candidate {
                    title: proposal.refined_title,
                    answer_kind: proposal.suggested_answer_kind,
                }))
            } else {
                Ok(Some(Candidate {
                    title: title.to_string(),
                    answer_kind: answer_kind.clone(),
                }))
            }
        }
        UserQuestionAssist::Verbatim => Ok(Some(Candidate {
            title: title.to_string(),
            answer_kind: answer_kind.clone(),
        })),
    }
}

/// Prompt for the question text, re-prompting until a non-empty line is given.
/// EOF (a closed / empty piped stdin) is a usage error rather than an infinite
/// loop.
fn prompt_question_text(input: &mut impl BufRead, output: &mut impl Write) -> Result<String> {
    loop {
        write!(output, "Question text: ")?;
        output.flush()?;
        match read_line(input)? {
            None => {
                return Err(QuizdomError::Usage(
                    "no question text provided (stdin closed)".to_string(),
                ))
            }
            Some(line) if line.trim().is_empty() => {
                writeln!(output, "Please enter the question text.")?;
            }
            Some(line) => return Ok(line.trim().to_string()),
        }
    }
}

/// Prompt for the answer shape: yes-no, free-text, or a multiple-choice list.
fn prompt_answer_shape(input: &mut impl BufRead, output: &mut impl Write) -> Result<AnswerKind> {
    loop {
        writeln!(output, "Answer shape:")?;
        writeln!(output, "  1. Yes / No")?;
        writeln!(output, "  2. Free text")?;
        writeln!(output, "  3. Multiple choice")?;
        write!(output, "Choose [1-3] (default 1): ")?;
        output.flush()?;
        let raw = match read_line(input)? {
            None => {
                return Err(QuizdomError::Usage(
                    "no answer shape provided (stdin closed)".to_string(),
                ))
            }
            Some(line) => line.trim().to_ascii_lowercase(),
        };
        match raw.as_str() {
            "" | "1" | "y" | "yes-no" | "yesno" => return Ok(AnswerKind::YesNo),
            "2" | "free" | "free-text" | "freetext" | "text" => return Ok(AnswerKind::FreeText),
            "3" | "choice" | "multiple-choice" | "mc" => {
                if let Some(kind) = prompt_choice_options(input, output)? {
                    return Ok(kind);
                }
                // Too few options: fall through and re-prompt the shape.
            }
            _ => writeln!(output, "Please choose 1, 2, or 3.")?,
        }
    }
}

/// Collect the options for a multiple-choice question. Requires at least two
/// distinct options; returns `None` (so the caller re-prompts the shape) when
/// fewer are supplied.
fn prompt_choice_options(
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<Option<AnswerKind>> {
    writeln!(
        output,
        "Enter one option per line; a blank line ends the list (need 2+)."
    )?;
    let mut options: Vec<String> = Vec::new();
    loop {
        write!(output, "  option {}: ", options.len() + 1)?;
        output.flush()?;
        match read_line(input)? {
            None => break,
            Some(line) if line.trim().is_empty() => break,
            Some(line) => {
                let option = line.trim().to_string();
                if options
                    .iter()
                    .any(|existing| existing.eq_ignore_ascii_case(&option))
                {
                    writeln!(output, "  (duplicate option ignored)")?;
                } else {
                    options.push(option);
                }
            }
        }
    }
    if options.len() < 2 {
        writeln!(
            output,
            "A multiple-choice question needs at least two options."
        )?;
        return Ok(None);
    }
    Ok(Some(AnswerKind::Choice(options)))
}

/// Prompt a yes/no question with a default applied on a blank line or EOF.
fn prompt_yes_no(
    input: &mut impl BufRead,
    output: &mut impl Write,
    prompt: &str,
    default: bool,
) -> Result<bool> {
    write!(output, "{prompt}")?;
    output.flush()?;
    match read_line(input)? {
        None => Ok(default),
        Some(line) => match line.trim().to_ascii_lowercase().as_str() {
            "" => Ok(default),
            "y" | "yes" => Ok(true),
            "n" | "no" => Ok(false),
            _ => Ok(default),
        },
    }
}

/// Read one line, returning `None` at EOF. Strips the trailing newline.
fn read_line(input: &mut impl BufRead) -> Result<Option<String>> {
    let mut raw = String::new();
    if input.read_line(&mut raw)? == 0 {
        Ok(None)
    } else {
        Ok(Some(raw.trim_end_matches(['\n', '\r']).to_string()))
    }
}

/// Print a confirmation of the persisted question and how it was wired in.
fn render_persisted(
    persisted: &Question,
    link: &QuestionLink,
    output: &mut impl Write,
) -> Result<()> {
    writeln!(
        output,
        "Added {} [{}]: {}",
        persisted.id,
        persisted.answer_kind.mode(),
        persisted.title
    )?;
    match link {
        QuestionLink::Begets { origin_id } => {
            writeln!(output, "  linked: begets from {origin_id}")?
        }
        QuestionLink::Probes { term_id } => writeln!(output, "  linked: probes {term_id}")?,
        QuestionLink::Standalone => writeln!(output, "  linked: standalone seed")?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RefinementProposal;
    use crate::persist::NoopUserAuthoredQuestionPersister;
    use std::cell::RefCell;
    use std::io::Cursor;

    fn strings<const N: usize>(args: [&str; N]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    fn titled(id: &str, title: &str) -> Question {
        Question {
            id: id.to_string(),
            title: title.to_string(),
            tags: vec!["answer:yes-no".to_string(), "weight:50".to_string()],
            answer_kind: AnswerKind::YesNo,
            weight: 50,
        }
    }

    fn config() -> QuestionAddConfig {
        QuestionAddConfig {
            user_id: DEFAULT_USER.to_string(),
            seed: None,
            probes: None,
            topic: None,
            backend: LlmBackend::Disabled,
        }
    }

    /// A strategy whose REFINE step always returns the configured proposal,
    /// standing in for the LLM-assist path without a network call.
    struct RefiningStrategy {
        proposal: Option<RefinementProposal>,
    }

    impl NextQuestionStrategy for RefiningStrategy {
        fn next_question(
            &self,
            _current: &Question,
            _context: &crate::strategy::StrategyContext,
            _bank: &dyn QuestionBank,
        ) -> Result<Option<Question>> {
            Ok(None)
        }
        fn refine_user_question(
            &self,
            _title: &str,
            _answer_kind: &AnswerKind,
        ) -> Result<Option<RefinementProposal>> {
            Ok(self.proposal.clone())
        }
    }

    /// Records every persisted question + topic + link so a test can assert the
    /// command drove the persister with the right arguments.
    #[derive(Default)]
    struct RecordingPersister {
        calls: RefCell<Vec<(Question, String, QuestionLink)>>,
    }

    impl UserAuthoredQuestionPersister for RecordingPersister {
        fn persist_user_authored_question(
            &self,
            question: &Question,
            topic: &str,
            link: &QuestionLink,
        ) -> Result<Question> {
            self.calls
                .borrow_mut()
                .push((question.clone(), topic.to_string(), link.clone()));
            let mut persisted = question.clone();
            persisted.id = "Q-99".to_string();
            Ok(persisted)
        }
    }

    fn run(
        config: &QuestionAddConfig,
        existing: &[Question],
        strategy: &dyn NextQuestionStrategy,
        persister: &dyn UserAuthoredQuestionPersister,
        stdin: &str,
    ) -> (String, Result<()>) {
        let mut input = Cursor::new(stdin.as_bytes().to_vec());
        let mut output = Vec::new();
        let result = question_add(
            config,
            existing,
            strategy,
            persister,
            &mut input,
            &mut output,
        );
        (String::from_utf8(output).expect("utf8"), result)
    }

    // --- argument parsing -------------------------------------------------

    #[test]
    fn parses_all_flags() {
        let config = QuestionAddConfig::parse(strings([
            "question", "add", "--user", "ada", "--seed", "Q-23", "--topic", "ethics", "--no-llm",
        ]))
        .expect("parse should succeed");
        assert_eq!(config.user_id, "ada");
        assert_eq!(config.seed.as_deref(), Some("Q-23"));
        assert_eq!(config.topic.as_deref(), Some("ethics"));
        assert_eq!(config.backend, LlmBackend::Disabled);
        assert_eq!(
            config.link(),
            QuestionLink::Begets {
                origin_id: "Q-23".to_string()
            }
        );
        assert_eq!(config.topic(), "ethics");
    }

    #[test]
    fn defaults_to_local_user_and_standalone_link() {
        let config =
            QuestionAddConfig::parse(strings(["question", "add"])).expect("parse should succeed");
        assert_eq!(config.user_id, DEFAULT_USER);
        assert!(config.seed.is_none());
        assert!(config.probes.is_none());
        assert_eq!(config.link(), QuestionLink::Standalone);
        // Topic falls back to the authoring user id.
        assert_eq!(config.topic(), DEFAULT_USER);
    }

    #[test]
    fn probes_flag_yields_probes_link() {
        let config = QuestionAddConfig::parse(strings(["question", "add", "--probes", "TERM-7"]))
            .expect("parse should succeed");
        assert_eq!(
            config.link(),
            QuestionLink::Probes {
                term_id: "TERM-7".to_string()
            }
        );
    }

    #[test]
    fn seed_and_probes_are_mutually_exclusive() {
        let error = QuestionAddConfig::parse(strings([
            "question", "add", "--seed", "Q-1", "--probes", "T-1",
        ]))
        .unwrap_err();
        assert!(matches!(error, QuizdomError::Usage(_)));
    }

    #[test]
    fn rejects_unknown_flag() {
        let error = QuestionAddConfig::parse(strings(["question", "add", "--nope"])).unwrap_err();
        assert!(matches!(error, QuizdomError::Usage(_)));
    }

    #[test]
    fn missing_flag_value_is_usage_error() {
        let error = QuestionAddConfig::parse(strings(["question", "add", "--seed"])).unwrap_err();
        assert!(matches!(error, QuizdomError::Usage(_)));
    }

    // --- offline / verbatim path -----------------------------------------

    #[test]
    fn authors_a_yes_no_question_verbatim_offline() {
        // Deterministic strategy never refines + empty bank -> no duplicate ->
        // the question is persisted exactly as authored.
        let persister = RecordingPersister::default();
        let (out, result) = run(
            &config(),
            &[],
            &DeterministicNextQuestionStrategy,
            &persister,
            "Is the self continuous over time?\n1\n",
        );
        result.expect("authoring should succeed");

        let calls = persister.calls.borrow();
        assert_eq!(calls.len(), 1);
        let (question, topic, link) = &calls[0];
        assert_eq!(question.title, "Is the self continuous over time?");
        assert_eq!(question.answer_kind, AnswerKind::YesNo);
        assert_eq!(topic, DEFAULT_USER);
        assert_eq!(*link, QuestionLink::Standalone);
        assert!(out.contains("Added Q-99 [yes-no]: Is the self continuous over time?"));
        assert!(out.contains("standalone seed"));
    }

    #[test]
    fn authors_a_free_text_question() {
        let persister = RecordingPersister::default();
        let (_out, result) = run(
            &config(),
            &[],
            &DeterministicNextQuestionStrategy,
            &persister,
            "What makes a choice free?\n2\n",
        );
        result.expect("authoring should succeed");
        assert_eq!(
            persister.calls.borrow()[0].0.answer_kind,
            AnswerKind::FreeText
        );
    }

    #[test]
    fn authors_a_multiple_choice_question() {
        let persister = RecordingPersister::default();
        let (out, result) = run(
            &config(),
            &[],
            &DeterministicNextQuestionStrategy,
            &persister,
            "Which value guides you?\n3\nHonesty\nKindness\n\n",
        );
        result.expect("authoring should succeed");
        assert_eq!(
            persister.calls.borrow()[0].0.answer_kind,
            AnswerKind::Choice(vec!["Honesty".to_string(), "Kindness".to_string()])
        );
        assert!(out.contains("Added Q-99"));
    }

    #[test]
    fn re_prompts_blank_question_text() {
        let persister = RecordingPersister::default();
        let (out, result) = run(
            &config(),
            &[],
            &DeterministicNextQuestionStrategy,
            &persister,
            "\n   \nDoes meaning require permanence?\n1\n",
        );
        result.expect("authoring should succeed");
        assert!(out.contains("Please enter the question text."));
        assert_eq!(
            persister.calls.borrow()[0].0.title,
            "Does meaning require permanence?"
        );
    }

    #[test]
    fn re_prompts_choice_with_too_few_options() {
        // One option then a blank line -> not enough -> re-prompt the shape,
        // where the user picks yes/no instead.
        let persister = RecordingPersister::default();
        let (out, result) = run(
            &config(),
            &[],
            &DeterministicNextQuestionStrategy,
            &persister,
            "Pick one?\n3\nOnly option\n\n1\n",
        );
        result.expect("authoring should succeed");
        assert!(out.contains("needs at least two options"));
        assert_eq!(persister.calls.borrow()[0].0.answer_kind, AnswerKind::YesNo);
    }

    // --- dedup path -------------------------------------------------------

    #[test]
    fn duplicate_reuse_skips_persistence() {
        let existing = vec![titled("Q-1", "Is the self continuous over time?")];
        let persister = RecordingPersister::default();
        // Author a near-duplicate, then decline to add it anyway (blank -> default No).
        let (out, result) = run(
            &config(),
            &existing,
            &DeterministicNextQuestionStrategy,
            &persister,
            "Over time, is the self continuous?\n1\n\n",
        );
        result.expect("flow should succeed");
        assert!(out.contains("near-duplicate already exists (Q-1"));
        assert!(out.contains("Reusing Q-1"));
        assert!(persister.calls.borrow().is_empty());
    }

    #[test]
    fn duplicate_override_persists_user_wording() {
        let existing = vec![titled("Q-1", "Is the self continuous over time?")];
        let persister = RecordingPersister::default();
        // Say "y" to add the user's wording anyway.
        let (_out, result) = run(
            &config(),
            &existing,
            &DeterministicNextQuestionStrategy,
            &persister,
            "Over time, is the self continuous?\n1\ny\n",
        );
        result.expect("flow should succeed");
        let calls = persister.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0.title, "Over time, is the self continuous?");
    }

    // --- refine path ------------------------------------------------------

    #[test]
    fn refinement_adopted_persists_refined_question() {
        let strategy = RefiningStrategy {
            proposal: Some(RefinementProposal {
                refined_title: "What makes a choice genuinely free?".to_string(),
                suggested_answer_kind: AnswerKind::FreeText,
                weak_socratic: false,
                rationale: "opens it up".to_string(),
            }),
        };
        let persister = RecordingPersister::default();
        // Author -> shape yes/no -> adopt the refinement (blank -> default Yes).
        let (out, result) = run(
            &config(),
            &[],
            &strategy,
            &persister,
            "Is a choice free?\n1\n\n",
        );
        result.expect("flow should succeed");
        assert!(out.contains("Suggested refinement:"));
        let calls = persister.calls.borrow();
        assert_eq!(calls[0].0.title, "What makes a choice genuinely free?");
        assert_eq!(calls[0].0.answer_kind, AnswerKind::FreeText);
    }

    #[test]
    fn refinement_rejected_keeps_user_wording() {
        let strategy = RefiningStrategy {
            proposal: Some(RefinementProposal {
                refined_title: "What makes a choice genuinely free?".to_string(),
                suggested_answer_kind: AnswerKind::FreeText,
                weak_socratic: true,
                rationale: "leading".to_string(),
            }),
        };
        let persister = RecordingPersister::default();
        // Reject the refinement with "n": keep the user's wording + shape.
        let (out, result) = run(
            &config(),
            &[],
            &strategy,
            &persister,
            "Is a choice free?\n1\nn\n",
        );
        result.expect("flow should succeed");
        assert!(out.contains("weak Socratic prompt"));
        let calls = persister.calls.borrow();
        assert_eq!(calls[0].0.title, "Is a choice free?");
        assert_eq!(calls[0].0.answer_kind, AnswerKind::YesNo);
    }

    // --- link wiring ------------------------------------------------------

    #[test]
    fn seed_config_wires_begets_link_through_persister() {
        let mut config = config();
        config.seed = Some("Q-5".to_string());
        let persister = RecordingPersister::default();
        let (_out, result) = run(
            &config,
            &[],
            &DeterministicNextQuestionStrategy,
            &persister,
            "Does the seed beget this?\n1\n",
        );
        result.expect("flow should succeed");
        assert_eq!(
            persister.calls.borrow()[0].2,
            QuestionLink::Begets {
                origin_id: "Q-5".to_string()
            }
        );
    }

    // --- graceful degradation --------------------------------------------

    #[test]
    fn closed_stdin_is_usage_error_not_a_hang() {
        let persister = NoopUserAuthoredQuestionPersister;
        let (_out, result) = run(
            &config(),
            &[],
            &DeterministicNextQuestionStrategy,
            &persister,
            "",
        );
        assert!(matches!(result, Err(QuizdomError::Usage(_))));
    }

    #[test]
    fn run_question_add_help_is_usage_error() {
        let result = run_question_add(
            strings(["question", "add", "--help"]),
            Cursor::new(Vec::new()),
            Vec::new(),
        );
        assert!(matches!(result, Err(QuizdomError::Usage(_))));
    }
}
