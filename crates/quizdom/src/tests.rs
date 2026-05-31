use crate::bank::*;
use crate::contradiction::*;
use crate::error::*;
use crate::honing::*;
use crate::input::*;
use crate::model::*;
use crate::persist::{
    AidaCliGeneratedQuestionPersister, AidaCliUserSpecificTermPersister, CommandRunner,
    UserSpecificTermPersister,
};
use crate::session::*;
use crate::strategy::*;
use llm::{AnthropicClient, LLMClient, LLMError, LLMFuture, Message, ToolDef};
use rustyline::EditMode;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{ExitStatus, Output};
use std::rc::Rc;

#[test]
fn parses_question_answer_kind_and_weight_from_aida_show() {
    let output = r#"ID: Q-99
Title: Pick a definition
Tags: topic:free-will, answer:choice[libertarian, compatibilist], weight:42
"#;

    let question = parse_question_show(output).unwrap();

    assert_eq!(question.id, "Q-99");
    assert_eq!(question.title, "Pick a definition");
    assert_eq!(
        question.answer_kind,
        AnswerKind::Choice(vec!["libertarian".to_string(), "compatibilist".to_string()])
    );
    assert_eq!(question.weight, 42);
}

#[test]
fn parses_begets_relationships_from_aida_rel_list() {
    let output = r#"FROM  TYPE    TO    TITLE
  Q-23  begets  Q-26  Do you mean the ability…
  Q-23  begets  Q-27  Can a choice be free?

2 edges
"#;

    let refs = parse_begets_rel_list(output);

    assert_eq!(
        refs,
        vec![
            QuestionRef {
                id: "Q-26".to_string()
            },
            QuestionRef {
                id: "Q-27".to_string()
            }
        ]
    );
}

#[test]
fn parses_probes_relationships_from_aida_rel_list() {
    let output = r#"FROM  TYPE    TO       TITLE
  Q-23  probes  TERM-24  free will / libertarian
  Q-23  probes  TERM-25  free will / compatibilist

2 edges
"#;

    let refs = parse_probes_rel_list(output);

    assert_eq!(
        refs,
        vec![
            TermRef {
                id: "TERM-24".to_string()
            },
            TermRef {
                id: "TERM-25".to_string()
            }
        ]
    );
}

#[test]
fn parses_term_definition_from_aida_show() {
    let output = r#"ID: TERM-24
Title: free will / libertarian
Tags: seed, definition:academic, topic:free-will, weight:60

source: libertarian free will.

definition: An agent has free will only if the agent could genuinely have chosen
otherwise under the same conditions.

scope: formal definition.
"#;

    let term = parse_term_show(output).unwrap();

    assert_eq!(term.id, "TERM-24");
    assert_eq!(term.title, "free will / libertarian");
    assert!(term.tags.contains(&"definition:academic".to_string()));
    assert_eq!(
            term.definition,
            "An agent has free will only if the agent could genuinely have chosen otherwise under the same conditions."
        );
}

#[test]
fn deterministic_strategy_uses_highest_weight_then_lowest_id() {
    let bank = FakeBank::new([
        question("Q-1", 0, AnswerKind::YesNo),
        question("Q-3", 80, AnswerKind::YesNo),
        question("Q-2", 80, AnswerKind::YesNo),
    ])
    .with_edges("Q-1", ["Q-3", "Q-2"]);

    let next = DeterministicNextQuestionStrategy
        .next_question(
            &bank.load_question("Q-1").unwrap(),
            &strategy_context("yes"),
            &bank,
        )
        .unwrap()
        .unwrap();

    assert_eq!(next.id, "Q-2");
}

// trace:STORY-48 | ai:claude
#[test]
fn deterministic_strategy_branches_on_triggering_answer() {
    let bank = FakeBank::new([
        question("Q-1", 0, AnswerKind::YesNo),
        question_with_tags(
            "Q-yes",
            80,
            AnswerKind::YesNo,
            ["weight:80", "from-answer:yes"],
        ),
        question_with_tags(
            "Q-no",
            90,
            AnswerKind::YesNo,
            ["weight:90", "from-answer:no"],
        ),
    ])
    .with_edges("Q-1", ["Q-yes", "Q-no"]);
    let current = bank.load_question("Q-1").unwrap();

    let on_yes = DeterministicNextQuestionStrategy
        .next_question(&current, &strategy_context("yes"), &bank)
        .unwrap()
        .unwrap();
    assert_eq!(on_yes.id, "Q-yes");

    // Even though Q-no carries a higher weight, "yes" must not branch into it.
    let on_no = DeterministicNextQuestionStrategy
        .next_question(&current, &strategy_context("no"), &bank)
        .unwrap()
        .unwrap();
    assert_eq!(on_no.id, "Q-no");
}

// trace:STORY-48 | ai:claude
#[test]
fn deterministic_strategy_prefers_matching_answer_over_unconditional() {
    let bank = FakeBank::new([
        question("Q-1", 0, AnswerKind::YesNo),
        question_with_tags("Q-any", 90, AnswerKind::YesNo, ["weight:90"]),
        question_with_tags(
            "Q-yes",
            50,
            AnswerKind::YesNo,
            ["weight:50", "from-answer:yes"],
        ),
    ])
    .with_edges("Q-1", ["Q-any", "Q-yes"]);
    let current = bank.load_question("Q-1").unwrap();

    // The answer-matched successor wins despite its lower weight.
    let on_yes = DeterministicNextQuestionStrategy
        .next_question(&current, &strategy_context("yes"), &bank)
        .unwrap()
        .unwrap();
    assert_eq!(on_yes.id, "Q-yes");

    // With no matching successor, the unconditional follow-on still applies.
    let on_no = DeterministicNextQuestionStrategy
        .next_question(&current, &strategy_context("no"), &bank)
        .unwrap()
        .unwrap();
    assert_eq!(on_no.id, "Q-any");
}

// trace:STORY-48 | ai:claude
#[test]
fn deterministic_strategy_excludes_mismatched_answer_successors() {
    let bank = FakeBank::new([
        question("Q-1", 0, AnswerKind::YesNo),
        question_with_tags(
            "Q-no",
            90,
            AnswerKind::YesNo,
            ["weight:90", "from-answer:no"],
        ),
    ])
    .with_edges("Q-1", ["Q-no"]);
    let current = bank.load_question("Q-1").unwrap();

    // The only successor is conditioned on "no", so "yes" has nowhere to go.
    let next = DeterministicNextQuestionStrategy
        .next_question(&current, &strategy_context("yes"), &bank)
        .unwrap();
    assert!(next.is_none());
}

/// A deterministic `WeightSampler` for tests: returns a fixed roll (reduced
/// into range) so weighted selection becomes a pure function of the seed.
struct FixedSampler(u64);

impl WeightSampler for FixedSampler {
    fn roll(&self, total: u64) -> u64 {
        self.0 % total
    }
}

// trace:STORY-67 | ai:claude
#[test]
fn weighted_strategy_samples_in_proportion_to_weight() {
    let bank = FakeBank::new([
        question("Q-1", 0, AnswerKind::YesNo),
        question("Q-a", 3, AnswerKind::YesNo),
        question("Q-b", 1, AnswerKind::YesNo),
    ])
    .with_edges("Q-1", ["Q-a", "Q-b"]);
    let current = bank.load_question("Q-1").unwrap();

    // Sweeping every roll across the full weight line yields one selection per
    // unit of weight: Q-a (weight 3) is chosen three times, Q-b (weight 1) once.
    let mut counts = HashMap::new();
    for roll in 0..4 {
        let strategy = WeightedNextQuestionStrategy::with_sampler(FixedSampler(roll));
        let next = strategy
            .next_question(&current, &strategy_context("yes"), &bank)
            .unwrap()
            .unwrap();
        *counts.entry(next.id).or_insert(0u32) += 1;
    }
    assert_eq!(counts.get("Q-a"), Some(&3));
    assert_eq!(counts.get("Q-b"), Some(&1));
}

// trace:STORY-67 | ai:claude
#[test]
fn weighted_strategy_never_selects_zero_weight_successors() {
    let bank = FakeBank::new([
        question("Q-1", 0, AnswerKind::YesNo),
        question("Q-zero", 0, AnswerKind::YesNo),
        question("Q-live", 5, AnswerKind::YesNo),
    ])
    .with_edges("Q-1", ["Q-zero", "Q-live"]);
    let current = bank.load_question("Q-1").unwrap();

    // No roll can land on the excluded zero-weight successor.
    for roll in 0..5 {
        let strategy = WeightedNextQuestionStrategy::with_sampler(FixedSampler(roll));
        let next = strategy
            .next_question(&current, &strategy_context("yes"), &bank)
            .unwrap()
            .unwrap();
        assert_eq!(next.id, "Q-live");
    }
}

// trace:STORY-67 | ai:claude
#[test]
fn weighted_strategy_returns_none_when_all_successors_are_zero_weight() {
    let bank = FakeBank::new([
        question("Q-1", 0, AnswerKind::YesNo),
        question("Q-z1", 0, AnswerKind::YesNo),
        question("Q-z2", 0, AnswerKind::YesNo),
    ])
    .with_edges("Q-1", ["Q-z1", "Q-z2"]);
    let current = bank.load_question("Q-1").unwrap();

    let strategy = WeightedNextQuestionStrategy::with_sampler(FixedSampler(0));
    let next = strategy
        .next_question(&current, &strategy_context("yes"), &bank)
        .unwrap();
    assert!(next.is_none());
}

// trace:STORY-67 | ai:claude
#[test]
fn weighted_strategy_honors_from_answer_filter() {
    let bank = FakeBank::new([
        question("Q-1", 0, AnswerKind::YesNo),
        question_with_tags(
            "Q-yes",
            5,
            AnswerKind::YesNo,
            ["weight:5", "from-answer:yes"],
        ),
        question_with_tags(
            "Q-no",
            90,
            AnswerKind::YesNo,
            ["weight:90", "from-answer:no"],
        ),
    ])
    .with_edges("Q-1", ["Q-yes", "Q-no"]);
    let current = bank.load_question("Q-1").unwrap();

    // "yes" must never branch into the heavier "no"-conditioned successor,
    // whatever the roll — STORY-48 filtering runs before sampling.
    for roll in 0..5 {
        let strategy = WeightedNextQuestionStrategy::with_sampler(FixedSampler(roll));
        let next = strategy
            .next_question(&current, &strategy_context("yes"), &bank)
            .unwrap()
            .unwrap();
        assert_eq!(next.id, "Q-yes");
    }
}

// trace:STORY-67 | ai:claude
#[test]
fn weighted_strategy_samples_only_within_the_top_relevance_tier() {
    let bank = FakeBank::new([
        question("Q-1", 0, AnswerKind::YesNo),
        question_with_tags("Q-any", 90, AnswerKind::YesNo, ["weight:90"]),
        question_with_tags(
            "Q-yes",
            5,
            AnswerKind::YesNo,
            ["weight:5", "from-answer:yes"],
        ),
    ])
    .with_edges("Q-1", ["Q-any", "Q-yes"]);
    let current = bank.load_question("Q-1").unwrap();

    // An answer-conditioned match outranks the heavier unconditional follow-on:
    // only the top relevance tier is sampled, so Q-yes always wins for "yes".
    for roll in 0..5 {
        let strategy = WeightedNextQuestionStrategy::with_sampler(FixedSampler(roll));
        let next = strategy
            .next_question(&current, &strategy_context("yes"), &bank)
            .unwrap()
            .unwrap();
        assert_eq!(next.id, "Q-yes");
    }

    // With no matching successor, the unconditional follow-on is sampled.
    let strategy = WeightedNextQuestionStrategy::with_sampler(FixedSampler(0));
    let on_no = strategy
        .next_question(&current, &strategy_context("no"), &bank)
        .unwrap()
        .unwrap();
    assert_eq!(on_no.id, "Q-any");
}

// trace:STORY-67 | ai:claude
#[test]
fn parse_strategy_accepts_weighted() {
    assert_eq!(parse_strategy("weighted").unwrap(), StrategyKind::Weighted);
}

#[test]
fn parses_llm_backend_selection() {
    assert_eq!(
        parse_llm_backend("claude-cli").unwrap(),
        LlmBackendKind::ClaudeCli
    );
    assert_eq!(
        parse_llm_backend("anthropic").unwrap(),
        LlmBackendKind::Anthropic
    );
    assert!(parse_llm_backend("other").is_err());
}

#[test]
fn llm_strategy_selects_existing_candidate_from_model_json() {
    let bank = FakeBank::new([
        question("Q-1", 0, AnswerKind::YesNo),
        question("Q-2", 40, AnswerKind::FreeText),
        question("Q-3", 10, AnswerKind::YesNo),
    ])
    .with_edges("Q-1", ["Q-2", "Q-3"]);
    let strategy = LlmNextQuestionStrategy::new(MockLlm::ok(r#"{"action":"select","id":"Q-3"}"#));

    let next = strategy
        .next_question(
            &bank.load_question("Q-1").unwrap(),
            &strategy_context("because it matters"),
            &bank,
        )
        .unwrap()
        .unwrap();

    assert_eq!(next.id, "Q-3");
}

#[test]
fn llm_strategy_returns_generated_question_in_memory() {
    let bank = FakeBank::new([question("Q-1", 0, AnswerKind::YesNo)]);
    let strategy = LlmNextQuestionStrategy::new(MockLlm::ok(
        r#"{"action":"generate","question":"What do you mean by responsibility?","answer_mode":"free-text"}"#,
    ));

    let next = strategy
        .next_question(
            &bank.load_question("Q-1").unwrap(),
            &strategy_context("yes"),
            &bank,
        )
        .unwrap()
        .unwrap();

    assert_eq!(next.id, "generated:llm");
    assert_eq!(next.title, "What do you mean by responsibility?");
    assert_eq!(next.answer_kind, AnswerKind::FreeText);
}

#[test]
fn llm_strategy_persists_generated_question_when_configured() {
    let bank = FakeBank::new([question_with_tags(
        "Q-1",
        0,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    )]);
    let runner = RecordingCommandRunner::new([
        command_output(true, "Added: Q-42\n", ""),
        command_output(true, "relationship added\n", ""),
    ]);
    let strategy = LlmNextQuestionStrategy::with_generated_question_persister(
        MockLlm::ok(
            r#"{"action":"generate","question":"What definition of responsibility are you using?","answer_mode":"free-text"}"#,
        ),
        AidaCliGeneratedQuestionPersister::new("aida", runner.clone()),
    );

    let next = strategy
        .next_question(
            &bank.load_question("Q-1").unwrap(),
            &strategy_context("yes"),
            &bank,
        )
        .unwrap()
        .unwrap();

    assert_eq!(next.id, "Q-42");
    assert_eq!(
        next.tags,
        vec![
            "topic:free-will".to_string(),
            "answer:free-text".to_string(),
            "weight:50".to_string(),
            "seed".to_string(),
            "from-answer:yes".to_string()
        ]
    );
    assert_eq!(
            runner.calls(),
            vec![
                strings([
                    "aida",
                    "add",
                    "--prefix",
                    "Q",
                    "--type",
                    "functional",
                    "--status",
                    "approved",
                    "--priority",
                    "medium",
                    "--title",
                    "What definition of responsibility are you using?",
                    "--description",
                    "LLM-generated quizdom question.\n\nanswer: free-text\norigin: Q-1\n\nGenerated from origin question: Q-1",
                    "--tags",
                    "topic:free-will,answer:free-text,weight:50,seed,from-answer:yes",
                ]),
                strings([
                    "aida", "rel", "add", "--from", "Q-1", "--to", "Q-42", "--type", "begets",
                ]),
            ]
        );
}

// trace:STORY-48 | ai:claude
#[test]
fn llm_strategy_leaves_free_text_followon_unconditional() {
    let bank = FakeBank::new([question_with_tags(
        "Q-1",
        0,
        AnswerKind::FreeText,
        ["topic:free-will", "answer:free-text", "weight:70"],
    )]);
    let runner = RecordingCommandRunner::new([
        command_output(true, "Added: Q-42\n", ""),
        command_output(true, "relationship added\n", ""),
    ]);
    let strategy = LlmNextQuestionStrategy::with_generated_question_persister(
        MockLlm::ok(
            r#"{"action":"generate","question":"What definition of responsibility are you using?","answer_mode":"free-text"}"#,
        ),
        AidaCliGeneratedQuestionPersister::new("aida", runner.clone()),
    );

    let next = strategy
        .next_question(
            &bank.load_question("Q-1").unwrap(),
            &strategy_context("freedom from coercion"),
            &bank,
        )
        .unwrap()
        .unwrap();

    // An open-ended answer does not condition the follow-on, so no from-answer tag.
    assert!(!next.tags.iter().any(|tag| tag.starts_with("from-answer:")));
    assert!(!runner.calls()[0]
        .iter()
        .any(|arg| arg.contains("from-answer:")));
}

#[test]
fn llm_strategy_prefers_near_identical_existing_candidate_over_duplicate() {
    let bank = FakeBank::new([
        question("Q-1", 0, AnswerKind::YesNo),
        Question {
            id: "Q-2".to_string(),
            title: "What definition of responsibility are you using?".to_string(),
            tags: vec!["topic:free-will".to_string(), "weight:50".to_string()],
            answer_kind: AnswerKind::FreeText,
            weight: 50,
        },
    ])
    .with_edges("Q-1", ["Q-2"]);
    let runner = RecordingCommandRunner::new([]);
    let strategy = LlmNextQuestionStrategy::with_generated_question_persister(
        MockLlm::ok(
            r#"{"action":"generate","question":"  What definition of responsibility are you using?  ","answer_mode":"free-text"}"#,
        ),
        AidaCliGeneratedQuestionPersister::new("aida", runner.clone()),
    );

    let next = strategy
        .next_question(
            &bank.load_question("Q-1").unwrap(),
            &strategy_context("yes"),
            &bank,
        )
        .unwrap()
        .unwrap();

    assert_eq!(next.id, "Q-2");
    assert!(runner.calls().is_empty());
}

#[test]
fn llm_strategy_falls_back_to_deterministic_on_model_error() {
    let bank = FakeBank::new([
        question("Q-1", 0, AnswerKind::YesNo),
        question("Q-3", 80, AnswerKind::YesNo),
        question("Q-2", 80, AnswerKind::YesNo),
    ])
    .with_edges("Q-1", ["Q-3", "Q-2"]);
    let strategy = LlmNextQuestionStrategy::new(MockLlm::err(LLMError::Provider(
        "provider unavailable".to_string(),
    )));

    let next = strategy
        .next_question(
            &bank.load_question("Q-1").unwrap(),
            &strategy_context("yes"),
            &bank,
        )
        .unwrap()
        .unwrap();

    assert_eq!(next.id, "Q-2");
}

#[test]
fn llm_strategy_flags_loaded_terms_from_free_text_answer() {
    let strategy = LlmNextQuestionStrategy::new(MockLlm::ok(r#"{"loaded_terms":["free will"]}"#));
    let current = question("Q-1", 0, AnswerKind::FreeText);

    let terms = strategy
        .loaded_terms(
            &current,
            &Answer {
                raw: "I mean freedom from coercion".to_string(),
                normalized: "I mean freedom from coercion".to_string(),
            },
        )
        .unwrap();

    assert_eq!(terms, vec!["free will".to_string()]);
}

#[test]
fn session_surfaces_probed_competing_definitions() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-41-test-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question_with_tags(
        "Q-23",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    )])
    .with_probes("Q-23", ["TERM-24", "TERM-25"])
    .with_terms([
        term(
            "TERM-24",
            "free will / libertarian",
            "An agent could genuinely have chosen otherwise.",
        ),
        term(
            "TERM-25",
            "free will / compatibilist",
            "The action flows from the agent's own reasons without coercion.",
        ),
    ]);
    let config = test_config(&path, "Q-23");
    let mut output = Vec::new();

    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "yes\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Terms to distinguish:"));
    assert!(output.contains("free will / libertarian"));
    assert!(output.contains("free will / compatibilist"));
    assert!(output.contains("chosen otherwise"));
    assert!(output.contains("without coercion"));

    let _ = fs::remove_file(path);
}

#[test]
fn loaded_term_definitions_filter_by_llm_flag() {
    let definitions = vec![
        term(
            "TERM-24",
            "free will / libertarian",
            "could choose otherwise",
        ),
        term(
            "TERM-30",
            "responsibility / moral",
            "accountable for an act",
        ),
    ];

    let filtered = definitions_for_loaded_terms(&definitions, &["free will".to_string()]);

    assert_eq!(filtered, vec![definitions[0].clone()]);
}

#[test]
fn llm_strategy_maps_user_meaning_to_closest_term_definition() {
    let definitions = free_will_terms();
    let strategy = LlmNextQuestionStrategy::new(MockLlm::ok(
        r#"{"term_id":"TERM-25","rationale":"The user emphasized acting from reasons without coercion."}"#,
    ));

    let proposal = strategy
        .map_term_meaning(
            "free will",
            "I mean acting from my own reasons without being forced.",
            &definitions,
        )
        .unwrap()
        .unwrap();

    assert_eq!(proposal.term_id, "TERM-25");
    assert_eq!(proposal.term_title, "free will / compatibilist");
    assert!(proposal.definition.contains("without coercion"));
    assert!(proposal.rationale.contains("without coercion"));
}

#[test]
fn deterministic_strategy_skips_term_mapping_offline() {
    let proposal = DeterministicNextQuestionStrategy
        .map_term_meaning(
            "free will",
            "I mean acting from my own reasons.",
            &free_will_terms(),
        )
        .unwrap();

    assert_eq!(proposal, None);
}

#[test]
fn session_asks_user_meaning_and_renders_mapping_proposal() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-42-test-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question_with_tags(
        "Q-23",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    )])
    .with_probes("Q-23", ["TERM-24", "TERM-25"])
    .with_terms(free_will_terms());
    let strategy = LlmNextQuestionStrategy::new(MockLlm::ok(
        r#"{"term_id":"TERM-25","rationale":"The user emphasized reasons without coercion."}"#,
    ));
    let config = test_config(&path, "Q-23");
    let mut output = Vec::new();

    run_session(
        &config,
        &bank,
        &strategy,
        "x\nActing from my own reasons without coercion.\nyes\nyes\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("What do you mean by free will?"));
    assert!(output.contains("That sounds closest to free will / compatibilist"));
    assert!(output.contains("Does this capture it?"));
    assert!(output.contains("Adopted free will / compatibilist."));
    assert!(output.contains("without coercion"));
    let log = fs::read_to_string(&path).unwrap();
    assert!(log.contains(r#""event_type":"term_interpreted""#));
    assert!(log.contains(r#""term_ref":"TERM-25""#));
    assert!(log.contains(r#""raw_definition":"Acting from my own reasons without coercion.""#));

    let _ = fs::remove_file(path);
}

#[test]
fn explore_runs_honing_then_reasks_same_question() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-52-test-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question_with_tags(
        "Q-23",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    )])
    .with_probes("Q-23", ["TERM-24", "TERM-25"])
    .with_terms(free_will_terms());
    let strategy = LlmNextQuestionStrategy::new(MockLlm::ok(
        r#"{"term_id":"TERM-25","rationale":"The user emphasized reasons without coercion."}"#,
    ));
    let config = test_config(&path, "Q-23");
    let mut output = Vec::new();

    run_session(
        &config,
        &bank,
        &strategy,
        "x\nActing from my own reasons without coercion.\nyes\nyes\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("What do you mean by free will?"));
    assert!(output.contains("Adopted free will / compatibilist."));
    assert_eq!(output.matches("\nQ-23\n").count(), 2);

    let log = fs::read_to_string(&path).unwrap();
    assert!(log.contains(r#""event_type":"term_interpreted""#));
    assert!(!log.contains(r#""normalized_answer":"explore""#));
    assert!(log.contains(r#""normalized_answer":"yes""#));

    let _ = fs::remove_file(path);
}

#[test]
fn settled_definition_is_reapplied_downstream() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-44-test-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([
        question_with_tags(
            "Q-23",
            70,
            AnswerKind::YesNo,
            ["topic:free-will", "answer:yes-no", "weight:70"],
        ),
        question_with_tags(
            "Q-27",
            60,
            AnswerKind::YesNo,
            ["topic:free-will", "answer:yes-no", "weight:60"],
        ),
    ])
    .with_edges("Q-23", ["Q-27"])
    .with_probes("Q-23", ["TERM-24", "TERM-25"])
    .with_probes("Q-27", ["TERM-24", "TERM-25"])
    .with_terms(free_will_terms());
    let strategy = LlmNextQuestionStrategy::new(MockLlm::ok(
        r#"{"term_id":"TERM-25","rationale":"The user emphasized reasons without coercion."}"#,
    ));
    let config = test_config(&path, "Q-23");
    let mut output = Vec::new();

    run_session(
        &config,
        &bank,
        &strategy,
        "x\nActing from my own reasons without coercion.\nyes\nyes\nno\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert_eq!(output.matches("What do you mean by free will?").count(), 1);
    assert!(output.contains("Settled meaning for free will:"));
    assert!(output.contains("free will / compatibilist"));
    assert!(output.contains("without coercion"));

    let log = fs::read_to_string(&path).unwrap();
    assert!(log.contains(r#""event_type":"term_interpreted""#));
    assert!(log.contains(r#""term_ref":"TERM-25""#));

    let _ = fs::remove_file(path);
}

#[test]
fn rejected_mapping_mints_user_specific_term_after_steering() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-43-test-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question_with_tags(
        "Q-23",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    )])
    .with_probes("Q-23", ["TERM-24", "TERM-25"])
    .with_terms(free_will_terms());
    let strategy = LlmNextQuestionStrategy::new(MockLlm::ok(
        r#"{"term_id":"TERM-25","rationale":"The user emphasized reasons without coercion."}"#,
    ));
    let runner = RecordingCommandRunner::new([command_output(true, "Added: TERM-99\n", "")]);
    let persister = AidaCliUserSpecificTermPersister::new("aida", runner.clone());
    let config = test_config(&path, "Q-23");
    let mut output = Vec::new();

    run_session_with_term_persister(
        &config,
        &bank,
        &strategy,
        &persister,
        "x\nA self-authored cause.\nno\nIt must originate outside the causal chain.\nyes\n"
            .as_bytes(),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("What would make the shared definition fit better?"));
    assert!(output.contains("Recorded a user-specific definition"));
    let log = fs::read_to_string(&path).unwrap();
    assert!(log.contains(r#""event_type":"term_interpreted""#));
    assert!(log.contains(r#""term_ref":"TERM-99""#));
    assert!(log.contains(r#""raw_definition":"It must originate outside the causal chain.""#));
    assert_eq!(
            runner.calls(),
            vec![strings([
                "aida",
                "add",
                "--type",
                "term",
                "--status",
                "approved",
                "--priority",
                "medium",
                "--title",
                "free will / user-specific",
                "--description",
                "source: user-specific quizdom steering fallback.\n\ndefinition: It must originate outside the causal chain.\n\nscope: user-specific definition captured only after shared bank definitions did not fit.",
                "--tags",
                "topic:free-will,definition:user-specific,weight:40",
            ])]
        );

    let _ = fs::remove_file(path);
}

#[test]
fn user_specific_term_persister_maps_aida_add_output() {
    let runner = RecordingCommandRunner::new([command_output(true, "Added: TERM-88\n", "")]);
    let persister = AidaCliUserSpecificTermPersister::new("aida", runner);

    let term = persister
        .persist_user_specific_term(
            "free will",
            "Free will means being an uncaused source.",
            &free_will_terms(),
        )
        .unwrap();

    assert_eq!(term.id, "TERM-88");
    assert_eq!(term.title, "free will / user-specific");
    assert!(term.tags.contains(&"definition:user-specific".to_string()));
    assert_eq!(term.definition, "Free will means being an uncaused source.");
}

#[test]
#[ignore = "requires ANTHROPIC_API_KEY and makes a live provider call"]
fn live_llm_strategy_smoke() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let bank = FakeBank::new([
        question("Q-1", 0, AnswerKind::YesNo),
        question("Q-2", 10, AnswerKind::FreeText),
    ])
    .with_edges("Q-1", ["Q-2"]);
    let strategy = LlmNextQuestionStrategy::new(AnthropicClient::from_env().unwrap());

    let next = strategy
        .next_question(
            &bank.load_question("Q-1").unwrap(),
            &strategy_context("yes"),
            &bank,
        )
        .unwrap();

    assert!(next.is_some());
}

#[test]
fn accepts_all_answer_kinds() {
    assert_eq!(
        normalize_answer(&AnswerKind::YesNo, "Y"),
        Some("yes".to_string())
    );
    assert_eq!(
        normalize_answer(
            &AnswerKind::Choice(vec!["one".to_string(), "two".to_string()]),
            "2"
        ),
        Some("two".to_string())
    );
    assert_eq!(
        normalize_answer(&AnswerKind::FreeText, "  because  "),
        Some("because".to_string())
    );
}

#[test]
fn accepts_explore_and_punt_controls() {
    assert_eq!(
        normalize_answer(&AnswerKind::YesNo, "x"),
        Some("explore".to_string())
    );
    assert_eq!(
        normalize_answer(&AnswerKind::YesNo, "P"),
        Some("punt".to_string())
    );
    assert_eq!(
        normalize_answer(
            &AnswerKind::Choice(vec!["one".to_string(), "two".to_string()]),
            "/x"
        ),
        Some("explore".to_string())
    );
    assert_eq!(
        normalize_answer(&AnswerKind::FreeText, "/p"),
        Some("punt".to_string())
    );
}

#[test]
fn accepts_quit_end_commands() {
    assert!(is_end_command("/end"));
    assert!(is_end_command("q"));
    assert!(is_end_command("Q"));
    assert!(is_end_command("quit"));
}

#[test]
fn editor_mode_uses_vi_for_vi_family_editors() {
    assert_eq!(edit_mode_from_editor("nvim"), EditMode::Vi);
    assert_eq!(edit_mode_from_editor("/usr/bin/vim"), EditMode::Vi);
}

#[test]
fn editor_mode_defaults_to_emacs_for_other_editors() {
    assert_eq!(edit_mode_from_editor("code"), EditMode::Emacs);
    assert_eq!(edit_mode_from_editor(""), EditMode::Emacs);
}

#[test]
fn renders_all_question_kinds() {
    let cases = [
        (
            AnswerKind::YesNo,
            "[Y] Yes  [N] No  [X] eXplore  [P] Punt  [B] Back  [Q] Quit",
        ),
        (
            AnswerKind::Choice(vec!["libertarian".to_string(), "compatibilist".to_string()]),
            "[1-2] Choose  [X] eXplore  [P] Punt  [B] Back  [Q] Quit",
        ),
        (AnswerKind::FreeText, "Answer in your own words, or Q/Quit"),
    ];

    for (answer_kind, expected) in cases {
        let mut output = Vec::new();
        render_question(&question("Q-test", 0, answer_kind), &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains(expected), "{output}");
    }
}

#[test]
fn resume_replays_exact_answered_path_and_continues_from_saved_next_question() {
    let log = [
            r#"{"event_id":"evt-000001","event_type":"session_started","occurred_at":"2026-05-31T17:00:00Z","session_id":"sess-test","user_id":"user","seed_question_ref":"Q-1"}"#,
            r#"{"event_id":"evt-000002","event_type":"question_presented","occurred_at":"2026-05-31T17:00:01Z","session_id":"sess-test","user_id":"user","turn":0,"question_ref":"Q-1","question_text":"First question?","answer_mode":"yes-no"}"#,
            r#"{"event_id":"evt-000003","event_type":"answer_recorded","occurred_at":"2026-05-31T17:00:02Z","session_id":"sess-test","user_id":"user","turn":0,"question_ref":"Q-1","answer_mode":"yes-no","raw_answer":"yes","normalized_answer":"yes"}"#,
            r#"{"event_id":"evt-000004","event_type":"next_question_selected","occurred_at":"2026-05-31T17:00:03Z","session_id":"sess-test","user_id":"user","turn":0,"question_ref":"Q-1","selected_next_question_ref":"Q-2","selection_reason":"test"}"#,
            r#"{"event_id":"evt-000005","event_type":"session_ended","occurred_at":"2026-05-31T17:00:04Z","session_id":"sess-test","user_id":"user","turn":0,"summary":"User ended session."}"#,
        ]
        .join("\n");
    let replay = SessionReplay::from_reader(log.as_bytes(), "main").unwrap();
    let mut output = Vec::new();

    replay.render(&mut output).unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("First question?"));
    assert!(output.contains("answer: yes"));
    assert_eq!(replay.next_question_ref, Some("Q-2".to_string()));
    assert_eq!(replay.next_turn, 1);
}

#[test]
fn answered_saved_next_question_clears_resume_target() {
    let log = [
            r#"{"event_id":"evt-000001","event_type":"question_presented","occurred_at":"2026-05-31T17:00:01Z","session_id":"sess-test","user_id":"user","turn":0,"question_ref":"Q-1","question_text":"First question?","answer_mode":"yes-no"}"#,
            r#"{"event_id":"evt-000002","event_type":"answer_recorded","occurred_at":"2026-05-31T17:00:02Z","session_id":"sess-test","user_id":"user","turn":0,"question_ref":"Q-1","answer_mode":"yes-no","raw_answer":"yes","normalized_answer":"yes"}"#,
            r#"{"event_id":"evt-000003","event_type":"next_question_selected","occurred_at":"2026-05-31T17:00:03Z","session_id":"sess-test","user_id":"user","turn":0,"question_ref":"Q-1","selected_next_question_ref":"Q-2","selection_reason":"test"}"#,
            r#"{"event_id":"evt-000004","event_type":"question_presented","occurred_at":"2026-05-31T17:00:04Z","session_id":"sess-test","user_id":"user","turn":1,"question_ref":"Q-2","question_text":"Second question?","answer_mode":"yes-no"}"#,
            r#"{"event_id":"evt-000005","event_type":"answer_recorded","occurred_at":"2026-05-31T17:00:05Z","session_id":"sess-test","user_id":"user","turn":1,"question_ref":"Q-2","answer_mode":"yes-no","raw_answer":"no","normalized_answer":"no"}"#,
        ]
        .join("\n");

    let replay = SessionReplay::from_reader(log.as_bytes(), "main").unwrap();

    assert_eq!(replay.next_question_ref, None);
    assert_eq!(replay.next_turn, 2);
}

#[test]
fn session_summaries_list_resumable_sessions_by_last_active() {
    let dir =
        std::env::temp_dir().join(format!("quizdom-story-65-list-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("older.jsonl"),
        [
            r#"{"event_id":"evt-000001","event_type":"session_started","occurred_at":"2026-05-31T10:00:00Z","session_id":"older","user_id":"user","branch_id":"main","seed_question_ref":"Q-1"}"#,
            r#"{"event_id":"evt-000002","event_type":"question_presented","occurred_at":"2026-05-31T10:01:00Z","session_id":"older","user_id":"user","branch_id":"main","turn":0,"question_ref":"Q-1","question_text":"Older question?","answer_mode":"yes-no"}"#,
            r#"{"event_id":"evt-000003","event_type":"answer_recorded","occurred_at":"2026-05-31T10:02:00Z","session_id":"older","user_id":"user","branch_id":"main","turn":0,"question_ref":"Q-1","answer_mode":"yes-no","raw_answer":"yes","normalized_answer":"yes"}"#,
        ]
        .join("\n"),
    )
    .unwrap();
    fs::write(
        dir.join("newer.jsonl"),
        [
            r#"{"event_id":"evt-000001","event_type":"session_started","occurred_at":"2026-05-31T11:00:00Z","session_id":"newer","user_id":"user","branch_id":"main","seed_question_ref":"Q-2"}"#,
            r#"{"event_id":"evt-000002","event_type":"question_presented","occurred_at":"2026-05-31T11:01:00Z","session_id":"newer","user_id":"user","branch_id":"agree","turn":0,"question_ref":"Q-2","question_text":"Newer question?","answer_mode":"yes-no"}"#,
            r#"{"event_id":"evt-000003","event_type":"answer_recorded","occurred_at":"2026-05-31T11:03:00Z","session_id":"newer","user_id":"user","branch_id":"agree","turn":0,"question_ref":"Q-2","answer_mode":"yes-no","raw_answer":"no","normalized_answer":"no"}"#,
        ]
        .join("\n"),
    )
    .unwrap();

    let summaries = session_summaries(&dir).unwrap();

    assert_eq!(summaries[0].session_id, "newer");
    assert_eq!(summaries[0].branch_id.as_deref(), Some("agree"));
    assert_eq!(
        summaries[0].last_question_answered.as_deref(),
        Some("Newer question?")
    );
    assert_eq!(summaries[1].session_id, "older");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn list_sessions_renders_resumable_sessions() {
    let user = format!("story-65-list-user-{}", std::process::id());
    let root = Path::new("data").join("users").join(&user);
    let dir = root.join("sessions");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("sess-list.jsonl"),
        [
            r#"{"event_id":"evt-000001","event_type":"session_started","occurred_at":"2026-05-31T13:00:00Z","session_id":"sess-list","user_id":"user","branch_id":"main","seed_question_ref":"Q-1"}"#,
            r#"{"event_id":"evt-000002","event_type":"question_presented","occurred_at":"2026-05-31T13:01:00Z","session_id":"sess-list","user_id":"user","branch_id":"main","turn":0,"question_ref":"Q-1","question_text":"Listed question?","answer_mode":"yes-no"}"#,
            r#"{"event_id":"evt-000003","event_type":"answer_recorded","occurred_at":"2026-05-31T13:02:00Z","session_id":"sess-list","user_id":"user","branch_id":"main","turn":0,"question_ref":"Q-1","answer_mode":"yes-no","raw_answer":"yes","normalized_answer":"yes"}"#,
        ]
        .join("\n"),
    )
    .unwrap();
    let config = CliConfig::parse([
        "session".to_string(),
        "list".to_string(),
        "--user".to_string(),
        user.clone(),
    ])
    .unwrap();
    let mut output = Vec::new();

    list_sessions(&config, &mut output).unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Sessions for user"));
    assert!(output.contains("sess-list"));
    assert!(output.contains("Listed question?"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn resume_without_session_uses_latest_session_log() {
    let user = format!("story-65-user-{}", std::process::id());
    let dir = Path::new("data").join("users").join(&user).join("sessions");
    let _ = fs::remove_dir_all(Path::new("data").join("users").join(&user));
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("latest.jsonl"),
        r#"{"event_id":"evt-000001","event_type":"session_started","occurred_at":"2026-05-31T12:00:00Z","session_id":"latest","user_id":"user","branch_id":"main","seed_question_ref":"Q-1"}"#,
    )
    .unwrap();
    let config = CliConfig::parse([
        "session".to_string(),
        "resume".to_string(),
        "--user".to_string(),
        user.clone(),
    ])
    .unwrap();

    let resolved = resolve_resume_config(config).unwrap();

    assert_eq!(resolved.session_id, "latest");
    assert_eq!(resolved.log_path, dir.join("latest.jsonl"));

    let _ = fs::remove_dir_all(Path::new("data").join("users").join(user));
}

// trace:BUG-70 | ai:codex
#[test]
fn resume_accepts_positional_session_id_with_or_without_prefix() {
    let prefixed = CliConfig::parse([
        "session".to_string(),
        "resume".to_string(),
        "sess-1780256438".to_string(),
    ])
    .unwrap();
    assert_eq!(prefixed.command, SessionCommand::Resume);
    assert_eq!(prefixed.session_id, "sess-1780256438");
    assert!(prefixed.session_id_provided);

    let bare = CliConfig::parse([
        "session".to_string(),
        "resume".to_string(),
        "1780256438".to_string(),
    ])
    .unwrap();
    assert_eq!(bare.command, SessionCommand::Resume);
    assert_eq!(bare.session_id, "sess-1780256438");
    assert!(bare.session_id_provided);
}

// trace:BUG-70 | ai:codex
#[test]
fn session_id_before_resume_is_accepted_positionally() {
    let config = CliConfig::parse([
        "session".to_string(),
        "1780256438".to_string(),
        "resume".to_string(),
    ])
    .unwrap();

    assert_eq!(config.command, SessionCommand::Resume);
    assert_eq!(config.session_id, "sess-1780256438");
    assert!(config.session_id_provided);
}

// trace:BUG-70 | ai:codex
#[test]
fn resume_session_flag_remains_supported_and_normalizes_bare_id() {
    let config = CliConfig::parse([
        "session".to_string(),
        "resume".to_string(),
        "--session".to_string(),
        "1780256438".to_string(),
    ])
    .unwrap();

    assert_eq!(config.command, SessionCommand::Resume);
    assert_eq!(config.session_id, "sess-1780256438");
    assert!(config.session_id_provided);
}

// trace:BUG-70 | ai:codex
#[test]
fn bare_resume_keeps_latest_session_resolution() {
    let config = CliConfig::parse(["session".to_string(), "resume".to_string()]).unwrap();

    assert_eq!(config.command, SessionCommand::Resume);
    assert!(!config.session_id_provided);
}

// trace:BUG-70 | ai:codex
#[test]
fn session_help_lists_commands_flags_and_resume_examples() {
    let error = CliConfig::parse(["session".to_string(), "--help".to_string()]).unwrap_err();
    let QuizdomError::Usage(help) = error else {
        panic!("expected usage help");
    };

    assert!(help.contains("Commands:"));
    assert!(help.contains("start"));
    assert!(help.contains("resume [session-id]"));
    assert!(help.contains("list"));
    assert!(help.contains("fork"));
    assert!(help.contains("--session sess-id"));
    assert!(help.contains("quizdom session resume sess-1780256438"));
    assert!(help.contains("quizdom session resume 1780256438"));

    let resume_error = CliConfig::parse([
        "session".to_string(),
        "resume".to_string(),
        "--help".to_string(),
    ])
    .unwrap_err();
    let QuizdomError::Usage(resume_help) = resume_error else {
        panic!("expected resume usage help");
    };
    assert_eq!(resume_help, help);
}

#[test]
fn start_end_resume_round_trip_replays_path_and_finishes() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-20-test-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([
        question("Q-1", 10, AnswerKind::YesNo),
        question("Q-2", 5, AnswerKind::YesNo),
    ])
    .with_edges("Q-1", ["Q-2"]);
    let strategy = DeterministicNextQuestionStrategy;
    let config = CliConfig {
        command: SessionCommand::Start,
        seed: "Q-1".to_string(),
        user_id: "test-user".to_string(),
        session_id: "sess-test".to_string(),
        session_id_provided: true,
        log_path: path.clone(),
        log_path_provided: true,
        branch_id: "main".to_string(),
        proposition: None,
        agree_seed: None,
        disagree_seed: None,
        strategy: StrategyKind::Deterministic,
    };
    let mut start_output = Vec::new();

    run_session(
        &config,
        &bank,
        &strategy,
        "yes\n/end\n".as_bytes(),
        &mut start_output,
    )
    .unwrap();

    let mut resume_output = Vec::new();
    let mut resume_config = config.clone();
    resume_config.command = SessionCommand::Resume;
    resume_session(
        &resume_config,
        &bank,
        &strategy,
        "no\n".as_bytes(),
        &mut resume_output,
    )
    .unwrap();

    let resume_output = String::from_utf8(resume_output).unwrap();
    assert!(resume_output.contains("RECAP:"));
    assert!(resume_output.contains("last question: Q-1"));
    assert!(resume_output.contains("your answer: yes"));
    assert!(resume_output.contains("Replaying previous session path for branch 'main':"));
    assert!(resume_output.contains("[turn 0] Q-1"));
    assert!(resume_output.contains("answer: yes"));
    assert!(resume_output.contains("Q-2"));

    let log = fs::read_to_string(&path).unwrap();
    assert!(log.contains(r#""question_ref":"Q-1""#));
    assert!(log.contains(r#""question_ref":"Q-2""#));
    assert!(log.contains(r#""normalized_answer":"no""#));

    let _ = fs::remove_file(path);
}

#[test]
fn live_session_surfaces_graph_contradiction_as_follow_up_question() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-58-test-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([
        question("Q-1", 10, AnswerKind::YesNo),
        question("Q-2", 5, AnswerKind::YesNo),
    ])
    .with_edges("Q-1", ["Q-2"]);
    let edges = FakeEdges::new().with("Q-1", ["Q-2"]);
    let strategy = DeterministicNextQuestionStrategy;
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    run_session_with_contradiction_edges(
        &config,
        &bank,
        &strategy,
        &edges,
        "yes\nyes\nI would refine one of them.\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("You leaned Q-1"));
    assert!(output.contains("and also Q-2"));
    assert!(output.contains("which holds, or how do you reconcile them?"));

    let log = fs::read_to_string(&path).unwrap();
    assert!(log.contains(r#""question_ref":"contradiction-2""#));
    assert!(log.contains(r#""raw_answer":"I would refine one of them.""#));

    let _ = fs::remove_file(path);
}

// trace:STORY-69 | ai:codex
#[test]
fn back_and_forward_browse_answered_path_without_truncating() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-69-browse-test-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = story_69_branching_bank();
    let strategy = DeterministicNextQuestionStrategy;
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    run_session(
        &config,
        &bank,
        &strategy,
        "yes\nyes\nb\nb\nf\nf\n/end\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Reviewing answer 2/2:"));
    assert!(output.contains("saved answer: yes"));
    assert!(output.contains("Reviewing answer 1/2:"));
    assert!(
        output.contains("[Y] Yes  [N] No  [X] eXplore  [P] Punt  [B] Back  [F] Forward  [Q] Quit")
    );

    let log = fs::read_to_string(&path).unwrap();
    assert!(!log.contains(r#""event_type":"path_truncated""#));
    let replay = SessionReplay::load(&path, "main").unwrap();
    assert_eq!(replay.answers.len(), 2);
    assert_eq!(replay.answers[0].question_ref, "Q-1");
    assert_eq!(replay.answers[1].question_ref, "Q-yes");
    assert_eq!(replay.next_question_ref.as_deref(), Some("Q-3"));

    let _ = fs::remove_file(path);
}

// trace:STORY-69 | ai:codex
#[test]
fn revising_reviewed_answer_truncates_tail_and_resume_replays_revised_path() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-69-revise-test-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = story_69_branching_bank();
    let strategy = DeterministicNextQuestionStrategy;
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    run_session(
        &config,
        &bank,
        &strategy,
        "yes\nyes\nb\nb\nno\nno\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Reviewing answer 1/2:"));
    assert!(output.contains("Q-no"));

    let log = fs::read_to_string(&path).unwrap();
    assert!(log.contains(r#""event_type":"path_truncated""#));
    assert!(log.contains(r#""from_turn":0"#));

    let replay = SessionReplay::load(&path, "main").unwrap();
    assert_eq!(replay.answers.len(), 2);
    assert_eq!(replay.answers[0].question_ref, "Q-1");
    assert_eq!(replay.answers[0].normalized_answer, "no");
    assert_eq!(replay.answers[1].question_ref, "Q-no");
    assert_eq!(replay.answers[1].normalized_answer, "no");
    assert_eq!(replay.next_question_ref, None);

    let mut resume_output = Vec::new();
    let mut resume_config = config.clone();
    resume_config.command = SessionCommand::Resume;
    resume_session(
        &resume_config,
        &bank,
        &strategy,
        "/end\n".as_bytes(),
        &mut resume_output,
    )
    .unwrap();

    let resume_output = String::from_utf8(resume_output).unwrap();
    assert!(resume_output.contains("[turn 0] Q-1"));
    assert!(resume_output.contains("answer: no"));
    assert!(resume_output.contains("[turn 1] Q-no"));
    assert!(!resume_output.contains("[turn 1] Q-yes"));

    let _ = fs::remove_file(path);
}

#[test]
fn forked_agree_and_disagree_branches_are_recoverable_independently() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-19-test-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([
        question("Q-agree", 10, AnswerKind::YesNo),
        question("Q-disagree", 10, AnswerKind::YesNo),
    ]);
    let strategy = DeterministicNextQuestionStrategy;
    let fork_config = CliConfig {
        command: SessionCommand::Fork,
        seed: "Q-1".to_string(),
        user_id: "test-user".to_string(),
        session_id: "sess-test".to_string(),
        session_id_provided: true,
        log_path: path.clone(),
        log_path_provided: true,
        branch_id: "main".to_string(),
        proposition: Some("Free will requires alternatives".to_string()),
        agree_seed: Some("Q-agree".to_string()),
        disagree_seed: Some("Q-disagree".to_string()),
        strategy: StrategyKind::Deterministic,
    };
    let mut fork_output = Vec::new();
    fork_session(&fork_config, &mut fork_output).unwrap();

    let mut agree_config = fork_config.clone();
    agree_config.command = SessionCommand::Resume;
    agree_config.branch_id = "agree".to_string();
    let mut agree_output = Vec::new();
    resume_session(
        &agree_config,
        &bank,
        &strategy,
        "yes\n".as_bytes(),
        &mut agree_output,
    )
    .unwrap();

    let mut disagree_config = fork_config.clone();
    disagree_config.command = SessionCommand::Resume;
    disagree_config.branch_id = "disagree".to_string();
    let mut disagree_output = Vec::new();
    resume_session(
        &disagree_config,
        &bank,
        &strategy,
        "no\n".as_bytes(),
        &mut disagree_output,
    )
    .unwrap();

    let agree_output = String::from_utf8(agree_output).unwrap();
    let disagree_output = String::from_utf8(disagree_output).unwrap();
    assert!(agree_output.contains("branch 'agree'"));
    assert!(agree_output.contains("Q-agree"));
    assert!(disagree_output.contains("branch 'disagree'"));
    assert!(disagree_output.contains("Q-disagree"));

    let agree_replay = SessionReplay::load(&path, "agree").unwrap();
    let disagree_replay = SessionReplay::load(&path, "disagree").unwrap();
    assert_eq!(agree_replay.answers[0].question_ref, "Q-agree");
    assert_eq!(agree_replay.answers[0].normalized_answer, "yes");
    assert_eq!(disagree_replay.answers[0].question_ref, "Q-disagree");
    assert_eq!(disagree_replay.answers[0].normalized_answer, "no");

    let _ = fs::remove_file(path);
}

fn question(id: &str, weight: u32, answer_kind: AnswerKind) -> Question {
    question_with_tags(id, weight, answer_kind, [format!("weight:{weight}")])
}

fn question_with_tags(
    id: &str,
    weight: u32,
    answer_kind: AnswerKind,
    tags: impl IntoIterator<Item = impl Into<String>>,
) -> Question {
    Question {
        id: id.to_string(),
        title: id.to_string(),
        tags: tags.into_iter().map(Into::into).collect(),
        answer_kind,
        weight,
    }
}

fn term(id: &str, title: &str, definition: &str) -> TermDefinition {
    TermDefinition {
        id: id.to_string(),
        title: title.to_string(),
        tags: vec![
            "topic:free-will".to_string(),
            "definition:academic".to_string(),
        ],
        definition: definition.to_string(),
    }
}

fn free_will_terms() -> Vec<TermDefinition> {
    vec![
        term(
            "TERM-24",
            "free will / libertarian",
            "An agent could genuinely have chosen otherwise.",
        ),
        term(
            "TERM-25",
            "free will / compatibilist",
            "The action flows from the agent's own reasons without coercion.",
        ),
    ]
}

fn story_69_branching_bank() -> FakeBank {
    FakeBank::new([
        question("Q-1", 10, AnswerKind::YesNo),
        question_with_tags(
            "Q-yes",
            10,
            AnswerKind::YesNo,
            ["weight:10", "from-answer:yes"],
        ),
        question_with_tags(
            "Q-no",
            10,
            AnswerKind::YesNo,
            ["weight:10", "from-answer:no"],
        ),
        question("Q-3", 1, AnswerKind::YesNo),
    ])
    .with_edges("Q-1", ["Q-yes", "Q-no"])
    .with_edges("Q-yes", ["Q-3"])
}

fn test_config(path: &Path, seed: &str) -> CliConfig {
    CliConfig {
        command: SessionCommand::Start,
        seed: seed.to_string(),
        user_id: "test-user".to_string(),
        session_id: "sess-test".to_string(),
        session_id_provided: true,
        log_path: path.to_path_buf(),
        log_path_provided: true,
        branch_id: "main".to_string(),
        proposition: None,
        agree_seed: None,
        disagree_seed: None,
        strategy: StrategyKind::Deterministic,
    }
}

fn strategy_context(raw: &str) -> StrategyContext {
    StrategyContext {
        answer: Answer {
            raw: raw.to_string(),
            normalized: raw.to_string(),
        },
        recent_path: Vec::new(),
    }
}

#[derive(Clone)]
struct MockLlm {
    result: std::result::Result<(String, Vec<llm::ToolCall>), LLMError>,
}

impl MockLlm {
    fn ok(text: &str) -> Self {
        Self {
            result: Ok((text.to_string(), Vec::new())),
        }
    }

    fn err(error: LLMError) -> Self {
        Self { result: Err(error) }
    }
}

impl LLMClient for MockLlm {
    fn call<'a>(
        &'a self,
        _system: &'a str,
        _messages: &'a [Message],
        _tools: &'a [ToolDef],
    ) -> LLMFuture<'a> {
        Box::pin(std::future::ready(self.result.clone()))
    }
}

#[derive(Clone)]
struct RecordingCommandRunner {
    calls: Rc<RefCell<Vec<Vec<String>>>>,
    outputs: Rc<RefCell<Vec<Output>>>,
}

impl RecordingCommandRunner {
    fn new(outputs: impl IntoIterator<Item = Output>) -> Self {
        Self {
            calls: Rc::new(RefCell::new(Vec::new())),
            outputs: Rc::new(RefCell::new(outputs.into_iter().collect())),
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.borrow().clone()
    }
}

impl CommandRunner for RecordingCommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<Output> {
        let mut call = vec![program.to_string()];
        call.extend(args.iter().cloned());
        self.calls.borrow_mut().push(call);
        if self.outputs.borrow().is_empty() {
            return Err(QuizdomError::Aida("unexpected command".to_string()));
        }
        Ok(self.outputs.borrow_mut().remove(0))
    }
}

fn command_output(success: bool, stdout: &str, stderr: &str) -> Output {
    Output {
        status: if success {
            ExitStatus::from_raw(0)
        } else {
            ExitStatus::from_raw(1)
        },
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

fn strings(items: impl IntoIterator<Item = &'static str>) -> Vec<String> {
    items.into_iter().map(str::to_string).collect()
}

struct FakeBank {
    questions: HashMap<String, Question>,
    edges: HashMap<String, Vec<QuestionRef>>,
    probes: HashMap<String, Vec<TermRef>>,
    terms: HashMap<String, TermDefinition>,
}

impl FakeBank {
    fn new(questions: impl IntoIterator<Item = Question>) -> Self {
        Self {
            questions: questions
                .into_iter()
                .map(|question| (question.id.clone(), question))
                .collect(),
            edges: HashMap::new(),
            probes: HashMap::new(),
            terms: HashMap::new(),
        }
    }

    fn with_edges(mut self, from: &str, to: impl IntoIterator<Item = &'static str>) -> Self {
        self.edges.insert(
            from.to_string(),
            to.into_iter()
                .map(|id| QuestionRef { id: id.to_string() })
                .collect(),
        );
        self
    }

    fn with_probes(mut self, from: &str, to: impl IntoIterator<Item = &'static str>) -> Self {
        self.probes.insert(
            from.to_string(),
            to.into_iter()
                .map(|id| TermRef { id: id.to_string() })
                .collect(),
        );
        self
    }

    fn with_terms(mut self, terms: impl IntoIterator<Item = TermDefinition>) -> Self {
        self.terms = terms
            .into_iter()
            .map(|term| (term.id.clone(), term))
            .collect();
        self
    }
}

impl QuestionBank for FakeBank {
    fn load_question(&self, id: &str) -> Result<Question> {
        self.questions
            .get(id)
            .cloned()
            .ok_or_else(|| QuizdomError::Parse(format!("missing {id}")))
    }

    fn begets(&self, id: &str) -> Result<Vec<QuestionRef>> {
        Ok(self.edges.get(id).cloned().unwrap_or_default())
    }

    fn probes(&self, id: &str) -> Result<Vec<TermRef>> {
        Ok(self.probes.get(id).cloned().unwrap_or_default())
    }

    fn load_term(&self, id: &str) -> Result<TermDefinition> {
        self.terms
            .get(id)
            .cloned()
            .ok_or_else(|| QuizdomError::Parse(format!("missing {id}")))
    }
}

// trace:EPIC-9 | ai:claude
struct FakeEdges {
    edges: HashMap<String, Vec<String>>,
}

impl FakeEdges {
    fn new() -> Self {
        Self {
            edges: HashMap::new(),
        }
    }

    fn with(mut self, from: &str, to: impl IntoIterator<Item = &'static str>) -> Self {
        self.edges.insert(
            from.to_string(),
            to.into_iter().map(str::to_string).collect(),
        );
        self
    }
}

impl ContradictsEdges for FakeEdges {
    fn contradicts(&self, belief_id: &str) -> Result<Vec<String>> {
        Ok(self.edges.get(belief_id).cloned().unwrap_or_default())
    }
}

fn adopted(id: Option<&str>, statement: &str) -> AdoptedBelief {
    AdoptedBelief {
        id: id.map(str::to_string),
        statement: statement.to_string(),
        source: id.unwrap_or("session").to_string(),
    }
}

#[test]
fn parses_contradicts_rel_list_to_targets() {
    let output = r#"FROM       TYPE          TO         TITLE
  BELIEF-1   contradicts   BELIEF-2   Free will is compatible with determinism
  BELIEF-1   agrees        BELIEF-3   Some other belief

2 edges
"#;

    let targets = parse_contradicts_rel_list(output);

    assert_eq!(targets, vec!["BELIEF-2".to_string()]);
}

#[test]
fn parses_empty_contradicts_rel_list() {
    assert!(parse_contradicts_rel_list("(no outgoing edges)\n").is_empty());
}

#[test]
fn graph_detection_flags_adopted_pair_joined_by_contradicts_edge() {
    let beliefs = vec![
        adopted(Some("BELIEF-1"), "Free will requires alternatives"),
        adopted(Some("BELIEF-2"), "Free will is compatible with determinism"),
    ];
    let edges = FakeEdges::new().with("BELIEF-1", ["BELIEF-2"]);

    let found = detect_graph_contradictions(&beliefs, &edges).unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].kind, ContradictionKind::Graph);
    assert_eq!(found[0].left, "Free will requires alternatives");
    assert_eq!(found[0].right, "Free will is compatible with determinism");
}

#[test]
fn graph_detection_ignores_edges_to_unadopted_beliefs() {
    let beliefs = vec![adopted(Some("BELIEF-1"), "Adopted belief")];
    let edges = FakeEdges::new().with("BELIEF-1", ["BELIEF-9"]);

    let found = detect_graph_contradictions(&beliefs, &edges).unwrap();

    assert!(found.is_empty());
}

#[test]
fn graph_detection_dedupes_reciprocal_edges() {
    let beliefs = vec![
        adopted(Some("BELIEF-1"), "One"),
        adopted(Some("BELIEF-2"), "Two"),
    ];
    let edges = FakeEdges::new()
        .with("BELIEF-1", ["BELIEF-2"])
        .with("BELIEF-2", ["BELIEF-1"]);

    let found = detect_graph_contradictions(&beliefs, &edges).unwrap();

    assert_eq!(found.len(), 1);
}

#[test]
fn semantic_detection_maps_llm_indices_to_statements() {
    let beliefs = vec![
        adopted(None, "Morality is objective"),
        adopted(None, "All values are subjective"),
    ];
    let client = MockLlm::ok(
        r#"{"contradictions":[{"a":0,"b":1,"explanation":"objective vs subjective values"}]}"#,
    );

    let found = detect_semantic_contradictions(&client, &beliefs).unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].kind, ContradictionKind::Semantic);
    assert_eq!(found[0].left, "Morality is objective");
    assert_eq!(found[0].right, "All values are subjective");
    assert_eq!(found[0].explanation, "objective vs subjective values");
}

#[test]
fn semantic_detection_skips_when_fewer_than_two_beliefs() {
    let beliefs = vec![adopted(None, "Only one belief")];
    let client = MockLlm::ok(r#"{"contradictions":[{"a":0,"b":0}]}"#);

    let found = detect_semantic_contradictions(&client, &beliefs).unwrap();

    assert!(found.is_empty());
}

#[test]
fn semantic_parser_ignores_out_of_range_and_self_pairs() {
    let beliefs = vec![adopted(None, "A"), adopted(None, "B")];
    let text = r#"{"contradictions":[{"a":0,"b":0},{"a":0,"b":5},{"a":0,"b":1}]}"#;

    let found = parse_semantic_contradictions(text, &beliefs).unwrap();

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].left, "A");
    assert_eq!(found[0].right, "B");
}

#[test]
fn semantic_parser_rejects_invalid_json() {
    let beliefs = vec![adopted(None, "A"), adopted(None, "B")];
    assert!(parse_semantic_contradictions("not json", &beliefs).is_err());
}

#[test]
fn semantic_prompt_lists_beliefs_with_indices() {
    let beliefs = vec![adopted(None, "First"), adopted(None, "Second")];
    let prompt = semantic_prompt(&beliefs);

    assert!(prompt.contains("[0] First"));
    assert!(prompt.contains("[1] Second"));
    assert!(prompt.contains("\"contradictions\""));
}

#[test]
fn merge_prefers_graph_over_semantic_for_same_pair() {
    let graph = vec![Contradiction {
        kind: ContradictionKind::Graph,
        left: "X".to_string(),
        right: "Y".to_string(),
        explanation: "edge".to_string(),
    }];
    let semantic = vec![
        Contradiction {
            kind: ContradictionKind::Semantic,
            left: "Y".to_string(),
            right: "X".to_string(),
            explanation: "semantic".to_string(),
        },
        Contradiction {
            kind: ContradictionKind::Semantic,
            left: "P".to_string(),
            right: "Q".to_string(),
            explanation: "other".to_string(),
        },
    ];

    let merged = merge_contradictions(graph, semantic);

    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].kind, ContradictionKind::Graph);
    assert_eq!(merged[1].kind, ContradictionKind::Semantic);
    assert_eq!(merged[1].left, "P");
}

#[test]
fn beliefs_from_session_log_pairs_questions_with_answers() {
    let log = concat!(
        r#"{"event_type":"session_started","branch_id":"main"}"#,
        "\n",
        r#"{"event_type":"question_presented","branch_id":"main","turn":0,"question_text":"Do you believe in free will?","question_ref":"Q-23"}"#,
        "\n",
        r#"{"event_type":"answer_recorded","branch_id":"main","turn":0,"question_ref":"Q-23","raw_answer":"yes"}"#,
        "\n",
    );

    let beliefs = beliefs_from_session_log(log.as_bytes(), None).unwrap();

    assert_eq!(beliefs.len(), 1);
    assert_eq!(beliefs[0].id.as_deref(), Some("Q-23"));
    assert_eq!(beliefs[0].statement, "Do you believe in free will? → yes");
}

#[test]
fn beliefs_from_session_log_filters_by_branch() {
    let log = concat!(
        r#"{"event_type":"answer_recorded","branch_id":"agree","turn":0,"question_ref":"Q-1","raw_answer":"yes"}"#,
        "\n",
        r#"{"event_type":"answer_recorded","branch_id":"disagree","turn":1,"question_ref":"Q-2","raw_answer":"no"}"#,
        "\n",
    );

    let beliefs = beliefs_from_session_log(log.as_bytes(), Some("agree")).unwrap();

    assert_eq!(beliefs.len(), 1);
    assert_eq!(beliefs[0].id.as_deref(), Some("Q-1"));
}

#[test]
fn run_contradictions_reports_when_no_beliefs_found() {
    let mut output = Vec::new();
    run_contradictions(
        strings(["contradictions", "--user", "nonexistent-user", "--no-llm"]),
        &mut output,
    )
    .unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("No adopted beliefs found to analyze."));
}
