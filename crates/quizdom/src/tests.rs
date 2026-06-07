use crate::bank::*;
use crate::contradiction::*;
use crate::error::*;
use crate::honing::*;
use crate::input::*;
use crate::model::*;
use crate::persist::{
    AidaCliGeneratedQuestionPersister, AidaCliUserSpecificTermPersister, CommandRunner,
    QuestionLink, QuestionReweighter, UserAuthoredQuestionPersister, UserSpecificTermPersister,
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

// trace:STORY-53 | ai:codex
#[test]
fn parses_question_ids_from_aida_list() {
    let output = r#"ID             Type         Status     Priority   Title
──────────────────────────────────────────────────────────────────────────
Q-23           Functional   Approved   High       Do you believe in free will?
BELIEF-28      Functional   Approved   Medium     Free will requires genuine alternatives
Q-27           Functional   Approved   High       Can a choice be free if caused?

3 requirements
"#;

    assert_eq!(
        parse_question_list_ids(output),
        vec!["Q-23".to_string(), "Q-27".to_string()]
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

// trace:STORY-53 | ai:codex
#[test]
fn punt_selection_skips_current_thread_and_current_topic() {
    let bank = FakeBank::new([
        question_with_tags(
            "Q-1",
            70,
            AnswerKind::YesNo,
            ["topic:free-will", "answer:yes-no", "weight:70"],
        ),
        question_with_tags(
            "Q-thread",
            90,
            AnswerKind::YesNo,
            ["topic:ethics", "answer:yes-no", "weight:90"],
        ),
        question_with_tags(
            "Q-same-topic",
            80,
            AnswerKind::YesNo,
            ["topic:free-will", "answer:yes-no", "weight:80"],
        ),
        question_with_tags(
            "Q-other",
            60,
            AnswerKind::YesNo,
            ["topic:meaning", "answer:yes-no", "weight:60"],
        ),
    ])
    .with_edges("Q-1", ["Q-thread"]);

    let next =
        different_topic_punt_question(&bank.load_question("Q-1").unwrap(), &[], &bank).unwrap();

    assert_eq!(next.unwrap().id, "Q-other");
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

// trace:STORY-86 | ai:claude
/// A bank question with a real title (the shared `question` helper sets
/// title == id, which is useless for dedup similarity).
fn titled_question(id: &str, title: &str, weight: u32) -> Question {
    Question {
        id: id.to_string(),
        title: title.to_string(),
        tags: vec!["answer:yes-no".to_string(), format!("weight:{weight}")],
        answer_kind: AnswerKind::YesNo,
        weight,
    }
}

// trace:STORY-86 | ai:claude
#[test]
fn llm_refine_proposes_improved_phrasing_for_approval() {
    let strategy = LlmNextQuestionStrategy::new(MockLlm::ok(
        r#"{"refined":"What makes a choice genuinely free?","answer_mode":"free-text","weak_socratic":false,"rationale":"opens the question up"}"#,
    ));

    let proposal = strategy
        .refine_user_question("Is a choice free?", &AnswerKind::YesNo)
        .unwrap()
        .expect("LLM returned a refinement to approve");

    assert_eq!(
        proposal.refined_title,
        "What makes a choice genuinely free?"
    );
    assert_eq!(proposal.suggested_answer_kind, AnswerKind::FreeText);
    assert!(!proposal.weak_socratic);
    assert_eq!(proposal.rationale, "opens the question up");
}

// trace:STORY-86 | ai:claude
#[test]
fn llm_refine_flags_weak_socratic_question() {
    let strategy = LlmNextQuestionStrategy::new(MockLlm::ok(
        r#"{"refined":"Is Paris the capital of France?","answer_mode":"yes-no","weak_socratic":true,"rationale":"purely factual, not a belief prompt"}"#,
    ));

    let proposal = strategy
        .refine_user_question("Is Paris the capital of France?", &AnswerKind::YesNo)
        .unwrap()
        .expect("a weak-Socratic flag is surfaced even when wording is unchanged");

    assert!(proposal.weak_socratic);
    assert_eq!(proposal.suggested_answer_kind, AnswerKind::YesNo);
}

// trace:STORY-86 | ai:claude
#[test]
fn llm_refine_no_op_returns_none() {
    // Same wording, same shape, not flagged -> nothing to approve.
    let strategy = LlmNextQuestionStrategy::new(MockLlm::ok(
        r#"{"refined":"Is the self continuous?","answer_mode":"yes-no","weak_socratic":false,"rationale":""}"#,
    ));
    let proposal = strategy
        .refine_user_question("Is the self continuous?", &AnswerKind::YesNo)
        .unwrap();
    assert!(proposal.is_none());
}

// trace:STORY-86 | ai:claude
#[test]
fn llm_refine_degrades_to_none_when_offline() {
    let strategy = LlmNextQuestionStrategy::new(MockLlm::err(LLMError::Provider(
        "network unreachable".to_string(),
    )));
    let proposal = strategy
        .refine_user_question("Is the self continuous?", &AnswerKind::YesNo)
        .unwrap();
    assert!(proposal.is_none());
}

// trace:STORY-86 | ai:claude
#[test]
fn assist_offers_existing_bank_question_for_reuse() {
    let bank = vec![
        titled_question("Q-1", "Is the self continuous over time?", 50),
        titled_question("Q-2", "Does morality depend on consequences?", 50),
    ];
    // A near-duplicate short-circuits before the LLM is ever consulted, so a
    // failing client must not change the outcome.
    let strategy = LlmNextQuestionStrategy::new(MockLlm::err(LLMError::Provider(
        "should not be called".to_string(),
    )));

    let outcome = assist_user_question(
        &strategy,
        "Over time, is the self continuous?",
        &AnswerKind::YesNo,
        &bank,
    );

    match outcome {
        UserQuestionAssist::Duplicate(found) => {
            assert_eq!(found.question.id, "Q-1");
            assert!(found.similarity >= DEDUP_SIMILARITY_THRESHOLD);
        }
        other => panic!("expected a duplicate offer, got {other:?}"),
    }
}

// trace:STORY-86 | ai:claude
#[test]
fn assist_returns_refinement_when_no_duplicate() {
    let bank = vec![titled_question(
        "Q-1",
        "Does morality depend on outcomes?",
        50,
    )];
    let strategy = LlmNextQuestionStrategy::new(MockLlm::ok(
        r#"{"refined":"What makes a choice genuinely free?","answer_mode":"free-text","weak_socratic":false,"rationale":"opens it up"}"#,
    ));

    let outcome = assist_user_question(&strategy, "Is a choice free?", &AnswerKind::YesNo, &bank);

    match outcome {
        UserQuestionAssist::Refinement(proposal) => {
            assert_eq!(
                proposal.refined_title,
                "What makes a choice genuinely free?"
            );
            assert_eq!(proposal.suggested_answer_kind, AnswerKind::FreeText);
        }
        other => panic!("expected a refinement proposal, got {other:?}"),
    }
}

// trace:STORY-86 | ai:claude
#[test]
fn assist_adds_verbatim_when_offline_and_no_duplicate() {
    let bank = vec![titled_question(
        "Q-1",
        "Does morality depend on outcomes?",
        50,
    )];
    // No duplicate + a failing LLM (offline) -> add the question as written.
    let strategy = LlmNextQuestionStrategy::new(MockLlm::err(LLMError::Provider(
        "network unreachable".to_string(),
    )));

    let outcome = assist_user_question(&strategy, "Is a choice free?", &AnswerKind::YesNo, &bank);

    assert_eq!(outcome, UserQuestionAssist::Verbatim);
}

// trace:STORY-86 | ai:claude
#[test]
fn assist_with_deterministic_strategy_adds_verbatim() {
    // The deterministic (non-LLM) strategy never refines: no duplicate -> verbatim.
    let bank = vec![titled_question(
        "Q-1",
        "Does morality depend on outcomes?",
        50,
    )];
    let outcome = assist_user_question(
        &DeterministicNextQuestionStrategy,
        "Is a choice free?",
        &AnswerKind::YesNo,
        &bank,
    );
    assert_eq!(outcome, UserQuestionAssist::Verbatim);
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

// trace:STORY-53 | ai:codex
#[test]
fn punt_jumps_to_different_topic_and_records_signal() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-53-test-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([
        question_with_tags(
            "Q-1",
            70,
            AnswerKind::YesNo,
            ["topic:free-will", "answer:yes-no", "weight:70"],
        ),
        question_with_tags(
            "Q-thread",
            90,
            AnswerKind::YesNo,
            ["topic:free-will", "answer:yes-no", "weight:90"],
        ),
        question_with_tags(
            "Q-other",
            60,
            AnswerKind::YesNo,
            ["topic:meaning", "answer:yes-no", "weight:60"],
        ),
    ])
    .with_edges("Q-1", ["Q-thread"]);
    let config = test_config(&path, "Q-1");
    let reweighter = RecordingQuestionReweighter::default();
    let mut output = Vec::new();

    run_session_with_question_reweighter(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        &reweighter,
        "p\n/end\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Q-other"));
    assert!(!output.contains("Q-thread"));
    assert_eq!(
        reweighter.calls.borrow().as_slice(),
        &[("Q-1".to_string(), QualitySignal::Punted)]
    );
    let log = fs::read_to_string(&path).unwrap();
    assert!(log.contains(r#""normalized_answer":"punt""#));
    assert!(log.contains(r#""selected_next_question_ref":"Q-other""#));
    assert!(log.contains("Punt selected a different-topic question."));

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

// trace:STORY-88 | ai:claude
#[test]
fn accepts_quick_add_commands() {
    assert!(is_add_command("a"));
    assert!(is_add_command("A"));
    assert!(is_add_command("/a"));
    assert!(is_add_command("/add"));
    assert!(is_add_command(" add "));
    // Not an add command.
    assert!(!is_add_command("answer"));
    assert!(!is_add_command("yes"));
}

// trace:BUG-98 | ai:claude
#[test]
fn free_text_prompt_lists_frontier_controls_as_slash_commands() {
    let mut output = Vec::new();
    render_question_for(
        &question("Q-free", 0, AnswerKind::FreeText),
        InputContext::Frontier,
        &mut output,
    )
    .unwrap();
    let output = String::from_utf8(output).unwrap();
    // Same control set as the frontier single-key prompt, as slash-commands.
    assert!(output.contains("/explore"), "{output}");
    assert!(output.contains("/add"), "{output}");
    assert!(output.contains("/punt"), "{output}");
    assert!(output.contains("/back"), "{output}");
    assert!(output.contains("/quit"), "{output}");
    // Frontier free-text never offers /forward.
    assert!(!output.contains("/forward"), "{output}");
}

// trace:BUG-98 | ai:claude
#[test]
fn free_text_prompt_lists_review_controls_as_slash_commands() {
    let mut output = Vec::new();
    render_question_for(
        &question("Q-free", 0, AnswerKind::FreeText),
        InputContext::Review,
        &mut output,
    )
    .unwrap();
    let output = String::from_utf8(output).unwrap();
    // Same control set as the review single-key prompt, as slash-commands.
    assert!(output.contains("/explore"), "{output}");
    assert!(output.contains("/punt"), "{output}");
    assert!(output.contains("/back"), "{output}");
    assert!(output.contains("/forward"), "{output}");
    assert!(output.contains("/quit"), "{output}");
    // Review is for revising the saved path, not authoring: no /add.
    assert!(!output.contains("/add"), "{output}");
}

// trace:BUG-98 | ai:claude
#[test]
fn free_text_slash_commands_map_to_navigation_actions() {
    // Back / Forward / Quit short-circuit before normalize_answer.
    assert!(is_back_command("/back"));
    assert!(is_forward_command("/forward"));
    assert!(is_end_command("/quit"));
    assert!(is_add_command("/add"));
    // Explore / Punt route through normalize_answer's free-text branch.
    assert_eq!(
        normalize_answer(&AnswerKind::FreeText, "/explore"),
        Some("explore".to_string())
    );
    assert_eq!(
        normalize_answer(&AnswerKind::FreeText, "/punt"),
        Some("punt".to_string())
    );
}

// trace:BUG-98 | ai:claude
#[test]
fn free_text_normal_answer_is_not_a_command() {
    // A plain answer that merely contains a control word stays an answer.
    assert!(!is_back_command("back to basics"));
    assert!(!is_end_command("quitting smoking matters"));
    assert_eq!(
        normalize_answer(&AnswerKind::FreeText, "explore my options"),
        Some("explore my options".to_string())
    );
    assert_eq!(
        normalize_answer(&AnswerKind::FreeText, "because freedom"),
        Some("because freedom".to_string())
    );
}

// trace:BUG-98 | ai:claude
#[test]
fn free_text_command_vs_answer_parsing_round_trip() {
    // Reading a slash-command line returns the matching navigation action,
    // while a normal line returns it as the free-text answer — frontier.
    let mut free_text = FreeTextInput::Plain;
    let mut out = Vec::new();
    let action = read_answer_or_end(
        &AnswerKind::FreeText,
        InputContext::Frontier,
        &mut "/back\n".as_bytes(),
        &mut free_text,
        &mut out,
    )
    .unwrap();
    assert!(matches!(action, AnswerInput::Back));

    let mut out = Vec::new();
    let action = read_answer_or_end(
        &AnswerKind::FreeText,
        InputContext::Frontier,
        &mut "/add\n".as_bytes(),
        &mut free_text,
        &mut out,
    )
    .unwrap();
    assert!(matches!(action, AnswerInput::Add));

    let mut out = Vec::new();
    let action = read_answer_or_end(
        &AnswerKind::FreeText,
        InputContext::Frontier,
        &mut "/explore\n".as_bytes(),
        &mut free_text,
        &mut out,
    )
    .unwrap();
    match action {
        AnswerInput::Answer(answer) => assert_eq!(answer.normalized, "explore"),
        _ => panic!("expected explore answer"),
    }

    let mut out = Vec::new();
    let action = read_answer_or_end(
        &AnswerKind::FreeText,
        InputContext::Frontier,
        &mut "because freedom\n".as_bytes(),
        &mut free_text,
        &mut out,
    )
    .unwrap();
    match action {
        AnswerInput::Answer(answer) => {
            assert_eq!(answer.normalized, "because freedom");
            assert_eq!(answer.raw, "because freedom");
        }
        _ => panic!("expected free-text answer"),
    }

    // Review context: /forward maps to Forward.
    let mut out = Vec::new();
    let action = read_answer_or_end(
        &AnswerKind::FreeText,
        InputContext::Review,
        &mut "/forward\n".as_bytes(),
        &mut free_text,
        &mut out,
    )
    .unwrap();
    assert!(matches!(action, AnswerInput::Forward));
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
        // trace:STORY-128 | ai:claude — `[S] Synopsis` joins the advertised set.
        // trace:STORY-176 | ai:claude — observe is `[o]` now (moved off `?`); `[?]`
        // shows the keyboard cheat-sheet, advertised at the end of the control set.
        (
            AnswerKind::YesNo,
            "[Y] Yes  [N] No  [o] Observe  [S] Synopsis  [X] eXplore  [A] Add  [P] Punt  [B] Back  [Q] Quit  [?] keys",
        ),
        (
            AnswerKind::Choice(vec!["libertarian".to_string(), "compatibilist".to_string()]),
            "[1-2] Choose  [o] Observe  [S] Synopsis  [X] eXplore  [A] Add  [P] Punt  [B] Back  [Q] Quit  [?] keys",
        ),
        // trace:BUG-98 | ai:claude — free-text (frontier) now advertises the
        // same control set as the single-key prompt, expressed as slash-commands.
        // trace:STORY-127 | ai:claude — `/observe` joins the advertised set.
        // trace:STORY-128 | ai:claude — `/synopsis` joins the advertised set.
        // trace:STORY-159 | ai:claude — `/goal` joins the advertised set.
        // trace:STORY-160 | ai:claude — `/rest` (rest your case) joins the set.
        // trace:STORY-161 | ai:claude — `/mode` joins the advertised set.
        // trace:STORY-163 | ai:claude — `/` (palette), `/help`, and `/tutor` join
        // the advertised free-text control set.
        // trace:STORY-173 | ai:claude — `/request-goal` joins the advertised set.
        // trace:STORY-174 | ai:claude — `/score` (the gauge toggle) joins the set.
        // trace:STORY-175 | ai:claude — `/objection`, `/resolved`, `/judge` join.
        (
            AnswerKind::FreeText,
            "Answer in your own words, or / (palette), /help, /tutor, /observe, /synopsis, /score, /goal, /request-goal, /mode, /objection, /resolved, /judge, /rest, /explore, /add, /punt, /back, /quit",
        ),
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

// trace:STORY-81 | ai:claude
// Quitting a fresh session before answering ANY question must leave nothing on
// disk: no log to clutter `session list`, and no liveness marker either.
#[test]
fn quitting_before_answering_discards_the_empty_session() {
    let dir = std::env::temp_dir().join(format!("quizdom-story-81-discard-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sess-empty.jsonl");
    let bank = FakeBank::new([question("Q-1", 0, AnswerKind::YesNo)]);
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "/end\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Session ended."));
    // The empty log is discarded outright...
    assert!(!path.exists(), "empty session log should be discarded");
    // ...and its liveness marker goes with it.
    assert!(
        !path.with_extension("active").exists(),
        "active marker should be removed for a discarded session"
    );
    // Nothing the session would surface in `session list`.
    assert!(session_summaries(&dir).unwrap().is_empty());

    let _ = fs::remove_dir_all(dir);
}

// trace:STORY-81 | ai:claude
// One answer is enough to keep the session: its log survives and stays
// resumable / listable.
#[test]
fn answering_then_quitting_keeps_the_session() {
    let dir = std::env::temp_dir().join(format!("quizdom-story-81-keep-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sess-kept.jsonl");
    let bank = FakeBank::new([question("Q-1", 0, AnswerKind::YesNo)]);
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "yes\n/end\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    assert!(path.exists(), "answered session log must be kept");
    let log = fs::read_to_string(&path).unwrap();
    assert!(log.contains(r#""event_type":"answer_recorded""#));
    let summaries = session_summaries(&dir).unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].path, path);

    let _ = fs::remove_dir_all(dir);
}

// trace:STORY-80 | ai:claude
// Quitting (/end) after at least one answer must surface the session id AND the
// exact bare resume command (no --strategy flag, per BUG-71) so the session is
// never a dead end.
#[test]
fn quitting_prints_session_id_and_resume_command() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-80-quit-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question("Q-1", 0, AnswerKind::YesNo)]);
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "yes\n/end\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(
        output.contains("Session sess-test ended."),
        "quit should name the session id: {output}"
    );
    assert!(
        output.contains("Resume:  quizdom session resume sess-test"),
        "quit should print the exact resume command: {output}"
    );
    assert!(
        !output.contains("--strategy"),
        "resume command must not carry a --strategy flag: {output}"
    );

    let _ = fs::remove_file(path);
}

// trace:STORY-80 | ai:claude
// A discarded empty session (STORY-81) ends plainly with no resume hint: the
// log is gone, so pointing at a resume command would only mislead.
#[test]
fn discarded_empty_session_ends_without_a_resume_hint() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-80-empty-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question("Q-1", 0, AnswerKind::YesNo)]);
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "/end\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Session ended."), "{output}");
    assert!(
        !output.contains("Resume:"),
        "a discarded session must not suggest resume: {output}"
    );
    assert!(!path.exists(), "empty session log should be discarded");

    let _ = fs::remove_file(path);
}

// trace:STORY-80 | ai:claude
// trace:BUG-136 | ai:claude
// The natural-completion dead end (strategy yields no follow-up) now offers the
// dead-end menu instead of ending outright; quitting it still prints the id +
// resume command (the STORY-80 footer contract).
#[test]
fn no_follow_up_completion_prints_session_id_and_resume_command() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-80-complete-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    // A seed with no outgoing begets edge: answering it reaches the dead end.
    let bank = FakeBank::new([question("Q-1", 0, AnswerKind::YesNo)]);
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "yes\nq\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(
        output.contains("No further questions on this path"),
        "{output}"
    );
    assert!(output.contains("Session sess-test ended."), "{output}");
    assert!(
        output.contains("Resume:  quizdom session resume sess-test"),
        "{output}"
    );

    let _ = fs::remove_file(path);
}

// trace:STORY-80 | ai:claude
// trace:BUG-136 | ai:claude
// The punt dead end (no different-topic target) now offers the dead-end menu;
// quitting it still prints the id + resume command.
#[test]
fn punt_dead_end_prints_session_id_and_resume_command() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-80-punt-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question_with_tags(
        "Q-1",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    )]);
    let config = test_config(&path, "Q-1");
    let reweighter = RecordingQuestionReweighter::default();
    let mut output = Vec::new();

    run_session_with_question_reweighter(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        &reweighter,
        "punt\nq\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(
        output.contains("No further questions on this path"),
        "{output}"
    );
    assert!(output.contains("Session sess-test ended."), "{output}");
    assert!(
        output.contains("Resume:  quizdom session resume sess-test"),
        "{output}"
    );

    let _ = fs::remove_file(path);
}

// trace:STORY-80 | ai:claude
// Resuming a session whose saved path has no follow-up is itself an end path:
// it must print the id + resume command too.
#[test]
fn resume_dead_end_prints_session_id_and_resume_command() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-80-resume-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question("Q-1", 10, AnswerKind::YesNo)]);
    let strategy = DeterministicNextQuestionStrategy;
    let config = test_config(&path, "Q-1");

    // Start + answer the only question so the saved path has no follow-up.
    let mut start_output = Vec::new();
    run_session(
        &config,
        &bank,
        &strategy,
        "yes\n".as_bytes(),
        &mut start_output,
    )
    .unwrap();

    let mut resume_config = config.clone();
    resume_config.command = SessionCommand::Resume;
    let mut resume_output = Vec::new();
    resume_session(
        &resume_config,
        &bank,
        &strategy,
        "".as_bytes(),
        &mut resume_output,
    )
    .unwrap();

    let resume_output = String::from_utf8(resume_output).unwrap();
    // trace:BUG-136 | ai:claude — resuming a terminal path now offers the menu;
    // quitting it prints the footer rather than the old "Session complete." line.
    assert!(
        resume_output.contains("No further questions on this path"),
        "{resume_output}"
    );
    assert!(
        resume_output.contains("Session sess-test ended."),
        "{resume_output}"
    );
    assert!(
        resume_output.contains("Resume:  quizdom session resume sess-test"),
        "{resume_output}"
    );

    let _ = fs::remove_file(path);
}

// trace:BUG-136 | ai:claude
// Stands in for an LLM that always generates a fresh follow-up. Used to prove a
// terminal session CONTINUES into a generated question instead of dead-ending.
struct AlwaysGeneratesStrategy {
    generated: Question,
}

impl NextQuestionStrategy for AlwaysGeneratesStrategy {
    fn next_question(
        &self,
        _current: &Question,
        _context: &StrategyContext,
        _bank: &dyn QuestionBank,
    ) -> Result<Option<Question>> {
        Ok(Some(self.generated.clone()))
    }
}

// trace:BUG-136 | ai:claude
// Returns None on its first call (so the session reaches the dead-end menu),
// then a fresh question afterwards (so pressing [G] there continues).
struct GeneratesAfterDeadEndStrategy {
    generated: Question,
    calls: std::cell::Cell<u32>,
}

impl NextQuestionStrategy for GeneratesAfterDeadEndStrategy {
    fn next_question(
        &self,
        _current: &Question,
        _context: &StrategyContext,
        _bank: &dyn QuestionBank,
    ) -> Result<Option<Question>> {
        let n = self.calls.get();
        self.calls.set(n + 1);
        if n == 0 {
            Ok(None)
        } else {
            Ok(Some(self.generated.clone()))
        }
    }
}

// trace:BUG-136 | ai:claude
// Resuming a session whose saved path is terminal must NOT dead-end: it
// auto-attempts a fresh successor (an LLM generates one) and continues into it.
#[test]
fn resume_continues_terminal_session_by_generating_a_next_question() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-bug-136-resume-continue-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let q_gen = question("Q-GEN", 0, AnswerKind::YesNo);
    let bank = FakeBank::new([question("Q-1", 0, AnswerKind::YesNo), q_gen.clone()]);
    let config = test_config(&path, "Q-1");

    // Start + answer Q-1, then quit the dead-end menu: a terminal saved path.
    let mut start_output = Vec::new();
    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "yes\nq\n".as_bytes(),
        &mut start_output,
    )
    .unwrap();

    // Resume with a generating strategy: the terminal session must continue.
    let mut resume_config = config.clone();
    resume_config.command = SessionCommand::Resume;
    let generator = AlwaysGeneratesStrategy { generated: q_gen };
    let mut resume_output = Vec::new();
    resume_session(
        &resume_config,
        &bank,
        &generator,
        "/end\n".as_bytes(),
        &mut resume_output,
    )
    .unwrap();

    let resume_output = String::from_utf8(resume_output).unwrap();
    assert!(
        resume_output.contains("Q-GEN"),
        "resume should continue into the generated question: {resume_output}"
    );
    assert!(
        !resume_output.contains("No further questions on this path"),
        "a session that can generate should not show the dead-end menu: {resume_output}"
    );

    let _ = fs::remove_file(path);
}

// trace:BUG-136 | ai:claude
// At a genuine dead end the menu's [G] re-attempts generation; when it yields a
// question the session continues into it.
#[test]
fn dead_end_menu_generate_continues_into_a_fresh_question() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-bug-136-menu-generate-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let q_gen = question("Q-GEN", 0, AnswerKind::YesNo);
    let bank = FakeBank::new([question("Q-1", 0, AnswerKind::YesNo), q_gen.clone()]);
    let config = test_config(&path, "Q-1");
    let strategy = GeneratesAfterDeadEndStrategy {
        generated: q_gen,
        calls: std::cell::Cell::new(0),
    };
    let mut output = Vec::new();

    // Answer Q-1 (-> dead-end menu), press [G] (-> continue into Q-GEN), end.
    run_session(
        &config,
        &bank,
        &strategy,
        "yes\ng\n/end\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(
        output.contains("No further questions on this path"),
        "answering the only question should reach the dead-end menu: {output}"
    );
    assert!(
        output.contains("Q-GEN"),
        "[G] should continue into the generated question: {output}"
    );

    let _ = fs::remove_file(path);
}

// trace:BUG-136 | ai:claude
// The dead-end menu degrades gracefully and stays open: [S] renders an offline
// synopsis, [G] reports exhaustion under a deterministic bank, [P] reports no
// different-topic target, and [Q] prints the resume footer. The menu re-prompts
// after each non-exiting choice.
#[test]
fn dead_end_menu_degrades_offline_and_quits_with_footer() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-bug-136-menu-degrade-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    // Force the observer offline so [S] cannot shell out to the LLM.
    std::env::set_var("QUIZDOM_CLAUDE_COMMAND", "quizdom-no-such-binary-bug136");
    let bank = FakeBank::new([question_with_tags(
        "Q-1",
        10,
        AnswerKind::YesNo,
        ["topic:free-will", "weight:10"],
    )]);
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "yes\ns\ng\np\nq\n".as_bytes(),
        &mut output,
    )
    .unwrap();
    std::env::remove_var("QUIZDOM_CLAUDE_COMMAND");

    let output = String::from_utf8(output).unwrap();
    assert!(
        output.contains("META (synopsis, offline)"),
        "[S] should render an offline synopsis: {output}"
    );
    assert!(
        output.contains("Couldn't generate a new question"),
        "[G] should report exhaustion under deterministic: {output}"
    );
    assert!(
        output.contains("No different-topic question to punt to"),
        "[P] with a single topic should report no target: {output}"
    );
    assert!(
        output.contains("Session sess-test ended."),
        "[Q] should print the resume footer: {output}"
    );
    assert!(
        output.matches("No further questions on this path").count() >= 2,
        "the menu should re-prompt after each non-exiting choice: {output}"
    );

    let _ = fs::remove_file(path);
}

// trace:BUG-136 | ai:claude
// The dead-end menu's [P] punts to a different-topic question when one exists,
// continuing the session there.
#[test]
fn dead_end_menu_punt_continues_to_a_different_topic() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-bug-136-menu-punt-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([
        question_with_tags(
            "Q-1",
            10,
            AnswerKind::YesNo,
            ["topic:free-will", "weight:10"],
        ),
        question_with_tags("Q-2", 50, AnswerKind::YesNo, ["topic:meaning", "weight:50"]),
    ]);
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    // Q-1 dead-ends (no begets); [P] surfaces Q-2 (a different topic); end there.
    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "yes\np\n/end\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(
        output.contains("No further questions on this path"),
        "{output}"
    );
    assert!(
        output.contains("Q-2"),
        "[P] should continue into a different-topic question: {output}"
    );

    let _ = fs::remove_file(path);
}

// trace:STORY-82 | ai:claude
// A PID that no live process owns: marker files naming it are stale and so the
// session they guard is resumable. u32::MAX is never a valid Linux PID.
const DEAD_PID: u32 = u32::MAX;

// trace:STORY-82 | ai:claude
fn write_session_log(dir: &Path, session_id: &str, occurred_at: &str) {
    fs::write(
        dir.join(format!("{session_id}.jsonl")),
        format!(
            r#"{{"event_id":"evt-000001","event_type":"session_started","occurred_at":"{occurred_at}","session_id":"{session_id}","user_id":"user","branch_id":"main","seed_question_ref":"Q-1"}}"#
        ),
    )
    .unwrap();
}

// trace:STORY-82 | ai:claude
fn mark_session_active(dir: &Path, session_id: &str, pid: u32) {
    fs::write(dir.join(format!("{session_id}.active")), pid.to_string()).unwrap();
}

// trace:STORY-82 | ai:claude
// Two live sessions plus one ended: bare resume must skip both live ones (even
// though they are newer) and target the ended session.
#[test]
fn bare_resume_skips_active_sessions_and_targets_latest_ended() {
    let user = format!("story-82-bare-{}", std::process::id());
    let root = Path::new("data").join("users").join(&user);
    let dir = root.join("sessions");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&dir).unwrap();

    write_session_log(&dir, "ended", "2026-05-31T10:00:00Z");
    write_session_log(&dir, "live-older", "2026-05-31T11:00:00Z");
    write_session_log(&dir, "live-newest", "2026-05-31T12:00:00Z");
    // Both newer sessions are owned by this (live) process.
    mark_session_active(&dir, "live-older", std::process::id());
    mark_session_active(&dir, "live-newest", std::process::id());

    let config = CliConfig::parse([
        "session".to_string(),
        "resume".to_string(),
        "--user".to_string(),
        user.clone(),
    ])
    .unwrap();

    let resolved = resolve_resume_config(config).unwrap();
    assert_eq!(resolved.session_id, "ended");
    assert_eq!(resolved.log_path, dir.join("ended.jsonl"));

    let _ = fs::remove_dir_all(root);
}

// trace:STORY-82 | ai:claude
// A marker left by a dead process is stale, so its session is still the bare
// resume target — a crash must not strand the newest session.
#[test]
fn bare_resume_treats_stale_marker_as_resumable() {
    let user = format!("story-82-stale-{}", std::process::id());
    let root = Path::new("data").join("users").join(&user);
    let dir = root.join("sessions");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&dir).unwrap();

    write_session_log(&dir, "crashed-newest", "2026-05-31T12:00:00Z");
    mark_session_active(&dir, "crashed-newest", DEAD_PID);

    let config = CliConfig::parse([
        "session".to_string(),
        "resume".to_string(),
        "--user".to_string(),
        user.clone(),
    ])
    .unwrap();

    let resolved = resolve_resume_config(config).unwrap();
    assert_eq!(resolved.session_id, "crashed-newest");

    let _ = fs::remove_dir_all(root);
}

// trace:STORY-82 | ai:claude
// When every session is live, bare resume has nothing safe to attach to.
#[test]
fn bare_resume_refuses_when_all_sessions_active() {
    let user = format!("story-82-allactive-{}", std::process::id());
    let root = Path::new("data").join("users").join(&user);
    let dir = root.join("sessions");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&dir).unwrap();

    write_session_log(&dir, "live", "2026-05-31T12:00:00Z");
    mark_session_active(&dir, "live", std::process::id());

    let config = CliConfig::parse([
        "session".to_string(),
        "resume".to_string(),
        "--user".to_string(),
        user.clone(),
    ])
    .unwrap();

    let error = resolve_resume_config(config).unwrap_err();
    assert!(
        matches!(&error, QuizdomError::Usage(message) if message.contains("currently active")),
        "expected a usage error about active sessions, got: {error:?}"
    );

    let _ = fs::remove_dir_all(root);
}

// trace:STORY-82 | ai:claude
// Explicitly resuming a live session would double-attach two processes to one
// log; refuse it.
#[test]
fn explicit_resume_of_active_session_is_refused() {
    let user = format!("story-82-explicit-live-{}", std::process::id());
    let root = Path::new("data").join("users").join(&user);
    let dir = root.join("sessions");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&dir).unwrap();

    write_session_log(&dir, "sess-live", "2026-05-31T12:00:00Z");
    mark_session_active(&dir, "sess-live", std::process::id());

    let config = CliConfig::parse([
        "session".to_string(),
        "resume".to_string(),
        "sess-live".to_string(),
        "--user".to_string(),
        user.clone(),
    ])
    .unwrap();

    let error = resolve_resume_config(config).unwrap_err();
    assert!(
        matches!(&error, QuizdomError::Usage(message) if message.contains("currently active")),
        "expected refusal to resume an in-use session, got: {error:?}"
    );

    let _ = fs::remove_dir_all(root);
}

// trace:STORY-82 | ai:claude
// A stale marker (dead PID) does not block explicit resume — a crashed session
// can be picked back up by id.
#[test]
fn explicit_resume_of_stale_session_is_allowed() {
    let user = format!("story-82-explicit-stale-{}", std::process::id());
    let root = Path::new("data").join("users").join(&user);
    let dir = root.join("sessions");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&dir).unwrap();

    write_session_log(&dir, "sess-crashed", "2026-05-31T12:00:00Z");
    mark_session_active(&dir, "sess-crashed", DEAD_PID);

    let config = CliConfig::parse([
        "session".to_string(),
        "resume".to_string(),
        "sess-crashed".to_string(),
        "--user".to_string(),
        user.clone(),
    ])
    .unwrap();

    let resolved = resolve_resume_config(config).unwrap();
    assert_eq!(resolved.session_id, "sess-crashed");

    let _ = fs::remove_dir_all(root);
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

// ---- STORY-159: session goal/focus -------------------------------------

// trace:STORY-159 | ai:claude — way 1 of 3: the `--goal <text>` flag sets the
// goal at start.
#[test]
fn goal_flag_sets_the_session_goal_at_start() {
    let config = CliConfig::parse([
        "session".to_string(),
        "start".to_string(),
        "--goal".to_string(),
        "can libertarian free will be held consistently?".to_string(),
    ])
    .unwrap();
    assert_eq!(
        config.goal.as_deref(),
        Some("can libertarian free will be held consistently?")
    );
}

// trace:STORY-159 | ai:claude — a bare `--goal` with no value is a usage error
// (an empty goal would orient nothing).
#[test]
fn bare_goal_flag_without_a_value_is_a_usage_error() {
    let error = CliConfig::parse(["session".to_string(), "--goal".to_string()]).unwrap_err();
    assert!(matches!(error, QuizdomError::Usage(_)));
}

// trace:STORY-159 | ai:claude — a free-flowing session carries no goal.
#[test]
fn start_without_goal_flag_is_free_flowing() {
    let config = CliConfig::parse(["session".to_string(), "start".to_string()]).unwrap();
    assert!(config.goal.is_none());
}

// trace:STORY-159 | ai:claude — way 2 of 3: the in-session `/goal <text>`
// command. Recognized in leading `/goal` and bare `goal` keyword forms, casing
// of the goal text preserved; a mid-answer mention of "goal" is NOT a command.
#[test]
fn goal_command_parses_the_in_session_form() {
    assert_eq!(
        goal_command_text("/goal is determinism true?").as_deref(),
        Some("is determinism true?")
    );
    assert_eq!(
        goal_command_text("/GOAL  Is Free Will Real?").as_deref(),
        Some("Is Free Will Real?")
    );
    assert_eq!(
        goal_command_text("goal settle compatibilism").as_deref(),
        Some("settle compatibilism")
    );
    // Bare `/goal` (no text) carries an empty string — the session reads it as
    // "show the current goal", never as a command to clear one.
    assert_eq!(goal_command_text("/goal").as_deref(), Some(""));
    assert_eq!(goal_command_text("goal").as_deref(), Some(""));
    // A free-text answer that merely contains "goal" mid-sentence is an answer,
    // not a command.
    assert!(goal_command_text("my goal is happiness").is_none());
    assert!(goal_command_text("yes").is_none());
}

// trace:STORY-173 | ai:claude — the on-demand `/request-goal` alias is recognized
// in its slash forms (case-insensitively) and is distinct from `/goal`: an
// ordinary answer that merely mentions "request" is NOT a command.
#[test]
fn request_goal_command_parses_the_on_demand_alias() {
    assert!(is_request_goal_command("/request-goal"));
    assert!(is_request_goal_command("/request goal"));
    assert!(is_request_goal_command("request-goal"));
    assert!(is_request_goal_command("/REQUEST-GOAL"));
    // Not the alias: a bare `/goal`, a mid-sentence "request", or noise.
    assert!(!is_request_goal_command("/goal"));
    assert!(!is_request_goal_command("please request a goal"));
    assert!(!is_request_goal_command("yes"));
    // The `/request-goal` keyword is NOT swallowed by the bare-`goal` recognizer
    // (which only matches a leading `goal`/`/goal` token) — they stay distinct.
    assert!(goal_command_text("/request-goal").is_none());
}

// trace:STORY-174 | ai:claude — the `/score` gauge toggle is recognized in its
// `/score` and bare-`score` forms (case-insensitively); an ordinary answer that
// merely mentions "score" mid-sentence is NOT a command.
#[test]
fn score_command_parses_the_gauge_toggle() {
    assert!(is_score_command("/score"));
    assert!(is_score_command("score"));
    assert!(is_score_command("/SCORE"));
    assert!(is_score_command("  /score  "));
    // Not the toggle: a mid-sentence "score" or noise stays an ordinary answer.
    assert!(!is_score_command("keep score of the argument"));
    assert!(!is_score_command("my score is high"));
    assert!(!is_score_command("yes"));
}

// trace:STORY-159 | ai:claude — once a goal is set, the breadcrumb shows it so
// the user always sees the thesis they are orienting toward; free-flowing
// sessions omit the segment.
#[test]
fn breadcrumb_shows_the_goal_when_set() {
    let question = question_with_tags(
        "Q-1",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    );
    assert_eq!(
        breadcrumb_line(&question, 2, "main", Some("is free will real?")),
        "[topic: free will | depth: 2 | branch: main | goal: is free will real?]"
    );
    // No goal → no goal segment.
    assert_eq!(
        breadcrumb_line(&question, 2, "main", None),
        "[topic: free will | depth: 2 | branch: main]"
    );
    // A blank goal is treated as no goal.
    assert_eq!(
        breadcrumb_line(&question, 2, "main", Some("   ")),
        "[topic: free will | depth: 2 | branch: main]"
    );
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

// trace:BUG-71 | ai:codex
#[test]
fn session_start_records_strategy_and_llm_backend() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-bug-71-log-test-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question("Q-1", 10, AnswerKind::YesNo)]);
    let strategy = DeterministicNextQuestionStrategy;
    let mut config = test_config(&path, "Q-1");
    config.strategy = StrategyKind::Llm;
    config.llm_backend = LlmBackendKind::ClaudeCli;
    let mut output = Vec::new();

    // trace:STORY-81 | ai:claude — answer once so the session is kept (an empty
    // session is now discarded on quit); the start event still carries metadata.
    run_session(
        &config,
        &bank,
        &strategy,
        "yes\n/end\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let log = fs::read_to_string(&path).unwrap();
    assert!(log.contains(r#""event_type":"session_started""#));
    assert!(log.contains(r#""strategy":"llm""#));
    assert!(log.contains(r#""llm_backend":"claude-cli""#));
    assert!(log.contains(r#""llm_model":"#));

    let _ = fs::remove_file(path);
}

// trace:BUG-71 | ai:codex
#[test]
fn resume_restores_logged_strategy_and_backend_when_not_overridden() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-bug-71-restore-test-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    fs::write(
        &path,
        r#"{"event_type":"session_started","branch_id":"main","strategy":"llm","llm_backend":"anthropic","llm_model":"claude-test","session_id":"sess-test","user_id":"test-user","seed_question_ref":"Q-1"}"#,
    )
    .unwrap();
    let config = CliConfig::parse([
        "session".to_string(),
        "resume".to_string(),
        "--log".to_string(),
        path.to_string_lossy().to_string(),
    ])
    .unwrap();

    let resolved = resolve_resume_config(config).unwrap();

    assert_eq!(resolved.strategy, StrategyKind::Llm);
    assert_eq!(resolved.llm_backend, LlmBackendKind::Anthropic);
    assert!(!resolved.strategy_provided);

    let _ = fs::remove_file(path);
}

// trace:BUG-71 | ai:codex
#[test]
fn explicit_resume_strategy_overrides_logged_strategy() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-bug-71-override-test-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    fs::write(
        &path,
        r#"{"event_type":"session_started","branch_id":"main","strategy":"llm","llm_backend":"anthropic","session_id":"sess-test","user_id":"test-user","seed_question_ref":"Q-1"}"#,
    )
    .unwrap();
    let config = CliConfig::parse([
        "session".to_string(),
        "resume".to_string(),
        "--log".to_string(),
        path.to_string_lossy().to_string(),
        "--strategy".to_string(),
        "deterministic".to_string(),
    ])
    .unwrap();

    let resolved = resolve_resume_config(config).unwrap();

    assert_eq!(resolved.strategy, StrategyKind::Deterministic);
    assert!(resolved.strategy_provided);

    let _ = fs::remove_file(path);
}

// trace:STORY-159 | ai:claude — a resumed session restores its goal (from the
// start event, then the latest in-session `goal_set`) so it keeps orienting
// toward the same thesis without re-passing `--goal`.
#[test]
fn resume_restores_the_goal_latest_wins() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-159-resume-goal-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    fs::write(
        &path,
        concat!(
            r#"{"event_type":"session_started","branch_id":"main","strategy":"deterministic","goal":"is free will real?","session_id":"sess-test","user_id":"test-user","seed_question_ref":"Q-1"}"#,
            "\n",
            r#"{"event_type":"goal_set","branch_id":"main","turn":1,"goal":"can libertarian free will be held consistently?","source":"observer","session_id":"sess-test","user_id":"test-user"}"#,
            "\n",
        ),
    )
    .unwrap();
    let config = CliConfig::parse([
        "session".to_string(),
        "resume".to_string(),
        "--log".to_string(),
        path.to_string_lossy().to_string(),
    ])
    .unwrap();

    let resolved = resolve_resume_config(config).unwrap();
    assert_eq!(
        resolved.goal.as_deref(),
        Some("can libertarian free will be held consistently?")
    );

    let _ = fs::remove_file(path);
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
        strategy_provided: false,
        llm_backend: LlmBackendKind::ClaudeCli,
        goal: None,
        // trace:STORY-161 | ai:claude
        mode: SessionMode::Socratic,
        mode_provided: false,
        // trace:STORY-169 | ai:claude
        no_tui: false,
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

// trace:STORY-59 | ai:codex
#[test]
fn contradiction_follow_up_persists_resolution_to_graph_and_log() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-59-test-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([
        question("Q-1", 10, AnswerKind::YesNo),
        question("Q-2", 5, AnswerKind::YesNo),
    ])
    .with_edges("Q-1", ["Q-2"]);
    let edges = FakeEdges::new().with("Q-1", ["Q-2"]);
    let runner = RecordingCommandRunner::new([
        command_output(true, "", ""),
        command_output(true, "Added: DECISION-9\n", ""),
        command_output(true, "", ""),
        command_output(true, "", ""),
    ]);
    let persister = AidaCliContradictionResolutionPersister::new("aida", runner.clone());
    let strategy = DeterministicNextQuestionStrategy;
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    run_session_with_contradiction_edges_and_resolution_persister(
        &config,
        &bank,
        &strategy,
        &edges,
        &persister,
        "yes\nyes\nleft\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let calls = runner.calls();
    assert_eq!(
        calls[0],
        strings([
            "aida",
            "rel",
            "add",
            "--from",
            "Q-1",
            "--to",
            "Q-2",
            "--type",
            "contradicts"
        ])
    );
    assert_eq!(&calls[1][0..3], ["aida", "add", "--type"]);
    assert!(calls[1].contains(&"decision".to_string()));
    assert!(calls[1].contains(&"contradiction-resolution,kept:left,left:Q-1,right:Q-2".to_string()));
    assert_eq!(
        calls[2],
        strings([
            "aida",
            "rel",
            "add",
            "--from",
            "DECISION-9",
            "--to",
            "Q-1",
            "--type",
            "references"
        ])
    );
    assert_eq!(
        calls[3],
        strings([
            "aida",
            "rel",
            "add",
            "--from",
            "DECISION-9",
            "--to",
            "Q-2",
            "--type",
            "references"
        ])
    );

    let log = fs::read_to_string(&path).unwrap();
    assert!(log.contains(r#""event_type":"contradiction_resolved""#));
    assert!(log.contains(r#""left_belief_ref":"Q-1""#));
    assert!(log.contains(r#""right_belief_ref":"Q-2""#));
    assert!(log.contains(r#""kept_side":"left""#));
    assert!(log.contains(r#""graph_ref":"DECISION-9""#));

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
    // trace:STORY-128 | ai:claude — `[S] Synopsis` joins the review controls.
    // trace:STORY-176 | ai:claude — observe is `[o]`; `[?] keys` ends the set.
    assert!(output.contains(
        "[Y] Yes  [N] No  [o] Observe  [S] Synopsis  [X] eXplore  [P] Punt  [B] Back  [F] Forward  [Q] Quit  [?] keys"
    ));

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
        strategy_provided: false,
        llm_backend: LlmBackendKind::ClaudeCli,
        goal: None,
        // trace:STORY-161 | ai:claude
        mode: SessionMode::Socratic,
        mode_provided: false,
        // trace:STORY-169 | ai:claude
        no_tui: false,
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

// trace:STORY-127 | ai:claude
// '?' mid-session yields the belief-neutral exchange reading as a labeled META
// voice, then returns to the SAME question (non-destructive) — and degrades to
// the structural note when no LLM backend is reachable. The session's observer
// uses the claude-cli backend; pointing it at a nonexistent command forces the
// spawn to fail, exercising the offline degradation path deterministically.
#[test]
fn observer_key_reads_exchange_then_re_presents_same_question_offline() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-127-observer-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    std::env::set_var(
        "QUIZDOM_CLAUDE_COMMAND",
        "quizdom-no-such-observer-binary-xyz",
    );
    let bank = story_69_branching_bank();
    let strategy = DeterministicNextQuestionStrategy;
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    // trace:STORY-176 | ai:claude — the observe affordance MOVED from `?` to `o`
    // (the DECIDED change); `?` now opens the cheat-sheet. Answer Q-1 yes (-> Q-yes),
    // press 'o' at Q-yes, then end. The observer reads the Q-1 -> "yes" -> Q-yes
    // exchange and re-presents Q-yes unchanged.
    run_session(
        &config,
        &bank,
        &strategy,
        "yes\no\n/end\n".as_bytes(),
        &mut output,
    )
    .unwrap();
    std::env::remove_var("QUIZDOM_CLAUDE_COMMAND");

    let output = String::from_utf8(output).unwrap();
    // A clearly-labeled, belief-neutral META reading was surfaced.
    assert!(
        output.contains("META (observer"),
        "expected a labeled META reading, got: {output}"
    );
    assert!(
        output.contains("belief-neutral reading of this exchange"),
        "the reading must announce itself belief-neutral: {output}"
    );
    // Offline degradation: the structural note's mismatch line is present, and it
    // never supplies the user's answer back to them.
    assert!(
        output.contains("Asked:") && output.contains("Answered:"),
        "structural note should diagnose asked-vs-answered: {output}"
    );
    // Non-destructive: Q-yes is presented before AND after the reading (twice),
    // and no answer for it was recorded to the log.
    assert!(
        output.matches("Q-yes").count() >= 2,
        "the same question must be re-presented after the reading: {output}"
    );
    let log = fs::read_to_string(&path).unwrap();
    // Q-yes is presented twice (before + after the reading) but never answered:
    // the observer keypress records no answer_recorded event for it.
    assert!(
        !log.lines()
            .any(|line| line.contains(r#""event_type":"answer_recorded""#)
                && line.contains(r#""question_ref":"Q-yes""#)),
        "the observer keypress must not record an answer for Q-yes: {log}"
    );
    // The observer leaves no trace in the persisted session log.
    assert!(
        !log.contains("META (observer"),
        "the META reading must not be written to the session log"
    );

    let _ = fs::remove_file(&path);
}

// trace:STORY-176 | ai:claude
// '?' mid-session prints the keyboard CHEAT-SHEET (the headless degrade of the
// TUI overlay) generated from the keymap registry, then re-presents the SAME
// question — non-destructive, and the observe affordance is now 'o', not '?'.
#[test]
fn cheatsheet_key_prints_grouped_bindings_then_re_presents_same_question() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-176-cheatsheet-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = story_69_branching_bank();
    let strategy = DeterministicNextQuestionStrategy;
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    // Answer Q-1 yes (-> Q-yes), press '?' at Q-yes (cheat-sheet), then end.
    run_session(
        &config,
        &bank,
        &strategy,
        "yes\n?\n/end\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    // The cheat-sheet header + every group heading is printed (generated from the
    // single keymap registry, so it cannot drift from the TUI dispatcher).
    assert!(
        output.contains("Keyboard cheat-sheet"),
        "expected the cheat-sheet header: {output}"
    );
    for group in ["Answering", "Navigation", "Meta", "Editing", "Session"] {
        assert!(
            output.contains(group),
            "cheat-sheet missing {group}: {output}"
        );
    }
    // The DECIDED bindings are documented: observe is 'o', '?' is the cheat-sheet,
    // and the navigation keys are listed.
    assert!(output.contains("Observe"), "observe row present: {output}");
    assert!(
        output.contains("Ctrl-←"),
        "navigation row present: {output}"
    );
    // Non-destructive: the cheat-sheet is printed at the Q-yes prompt and the input
    // re-prompts (the `?` keypress is handled inline, then awaits the next input);
    // the question itself is never re-answered, and the cheat-sheet appears AFTER
    // Q-yes was presented.
    let q_yes_at = output.find("Q-yes").expect("Q-yes presented");
    let cheat_at = output
        .find("Keyboard cheat-sheet")
        .expect("cheat-sheet printed");
    assert!(
        cheat_at > q_yes_at,
        "the cheat-sheet must print at the Q-yes prompt: {output}"
    );
    // The cheat-sheet leaves no trace in the persisted session log.
    let log = fs::read_to_string(&path).unwrap();
    assert!(
        !log.contains("Keyboard cheat-sheet"),
        "the cheat-sheet must not be written to the session log: {log}"
    );
    assert!(
        !log.lines()
            .any(|line| line.contains(r#""event_type":"answer_recorded""#)
                && line.contains(r#""question_ref":"Q-yes""#)),
        "the cheat-sheet keypress must not record an answer for Q-yes: {log}"
    );

    let _ = fs::remove_file(&path);
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
        strategy_provided: false,
        llm_backend: LlmBackendKind::ClaudeCli,
        goal: None,
        // trace:STORY-161 | ai:claude
        mode: SessionMode::Socratic,
        mode_provided: false,
        // trace:STORY-169 | ai:claude
        no_tui: false,
    }
}

fn strategy_context(raw: &str) -> StrategyContext {
    StrategyContext {
        answer: Answer {
            raw: raw.to_string(),
            normalized: raw.to_string(),
        },
        recent_path: Vec::new(),
        goal: None,
        // trace:STORY-161 | ai:claude
        mode: SessionMode::Socratic,
        // trace:STORY-175 | ai:claude
        objection: None,
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

impl ResolutionCommandRunner for RecordingCommandRunner {
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

#[derive(Default)]
struct RecordingQuestionReweighter {
    calls: RefCell<Vec<(String, QualitySignal)>>,
}

impl QuestionReweighter for RecordingQuestionReweighter {
    fn reweight_question(&self, question: &Question, signal: QualitySignal) -> Result<Question> {
        self.calls.borrow_mut().push((question.id.clone(), signal));
        Ok(question.clone())
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

    fn all_questions(&self) -> Result<Vec<Question>> {
        Ok(self.questions.values().cloned().collect())
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

// trace:STORY-88 | ai:claude
/// Records every user-authored question the in-session quick-add control drives
/// through it (question + topic + link), so a test can assert the new question
/// was authored and linked as a `begets` follow-on from the current node. The
/// persisted question gets a stable fake id so callers can recognise it later.
#[derive(Default)]
struct RecordingUserAuthoredPersister {
    calls: RefCell<Vec<(Question, String, QuestionLink)>>,
}

impl UserAuthoredQuestionPersister for RecordingUserAuthoredPersister {
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
        persisted.id = "Q-added".to_string();
        Ok(persisted)
    }
}

// trace:STORY-88 | ai:claude
fn story_88_temp_log(slug: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-88-{slug}-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    path
}

// trace:STORY-88 | ai:claude
// Pressing the in-session quick-add control authors a new question and links it
// as a `begets` follow-on from the CURRENT node, then re-presents the current
// question so the user resumes where they paused.
#[test]
fn quick_add_authors_and_links_question_from_current_node() {
    let path = story_88_temp_log("links");
    let bank = FakeBank::new([question_with_tags(
        "Q-23",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    )]);
    let config = test_config(&path, "Q-23");
    let persister = RecordingUserAuthoredPersister::default();
    let mut output = Vec::new();

    // `a` opens quick-add -> author a yes/no question -> back at Q-23 -> answer
    // yes -> no begets successor -> session ends.
    run_session_with_user_authored_persister(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        &persister,
        "a\nDoes the self persist through change?\n1\nyes\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let calls = persister.calls.borrow();
    assert_eq!(calls.len(), 1, "exactly one question authored");
    let (question, topic, link) = &calls[0];
    assert_eq!(question.title, "Does the self persist through change?");
    assert_eq!(question.answer_kind, AnswerKind::YesNo);
    // Linked as a begets follow-on from the CURRENT node.
    assert_eq!(
        *link,
        QuestionLink::Begets {
            origin_id: "Q-23".to_string()
        }
    );
    // Topic inherited from the current node's topic tag.
    assert_eq!(topic, "free-will");

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Quick-add: authoring a new question linked from Q-23."));
    assert!(output.contains("Added Q-added"));
    // The current question is re-presented after the quick-add (seen twice:
    // once before the add, once after).
    assert_eq!(output.matches("\nQ-23\n").count(), 2);

    let _ = fs::remove_file(path);
}

// trace:STORY-88 | ai:claude
// The quick-add persists through the real AIDA-CLI persister, which issues an
// `aida rel add --type begets --from <current> --to <new>` — the edge that
// makes the question a begets successor of the current node in later sessions.
#[test]
fn quick_add_issues_begets_edge_for_later_sessions() {
    let path = story_88_temp_log("begets-edge");
    let bank = FakeBank::new([question_with_tags(
        "Q-23",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    )]);
    let config = test_config(&path, "Q-23");
    // The persister runs two aida commands: `add` (returns the new id) then
    // `rel add` (the begets edge).
    let runner = RecordingCommandRunner::new([
        command_output(true, "Added Q-77", ""),
        command_output(true, "", ""),
    ]);
    let persister =
        crate::persist::AidaCliUserAuthoredQuestionPersister::new("aida", runner.clone());
    let mut output = Vec::new();

    run_session_with_user_authored_persister(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        &persister,
        "a\nWhat would change your mind here?\n1\nyes\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let calls = runner.calls();
    assert_eq!(calls.len(), 2, "add then rel add");
    let add = &calls[0];
    assert_eq!(add[1], "add");
    assert!(add
        .iter()
        .any(|arg| arg == "source:user-authored,topic:free-will,answer:yes-no,weight:50,seed"));
    let rel = &calls[1];
    assert_eq!(rel[1], "rel");
    assert_eq!(rel[2], "add");
    let from_index = rel.iter().position(|arg| arg == "--from").unwrap();
    let to_index = rel.iter().position(|arg| arg == "--to").unwrap();
    let type_index = rel.iter().position(|arg| arg == "--type").unwrap();
    // begets is current -> new.
    assert_eq!(rel[from_index + 1], "Q-23");
    assert_eq!(rel[to_index + 1], "Q-77");
    assert_eq!(rel[type_index + 1], "begets");

    let _ = fs::remove_file(path);
}

// trace:STORY-88 | ai:claude
// Reusing a near-duplicate from the quick-add flow persists nothing new (no
// begets edge), and the session continues from the current node.
#[test]
fn quick_add_reusing_duplicate_persists_nothing() {
    let path = story_88_temp_log("dup");
    let bank = FakeBank::new([
        question_with_tags(
            "Q-23",
            70,
            AnswerKind::YesNo,
            ["topic:free-will", "answer:yes-no", "weight:70"],
        ),
        question_with_tags(
            "Q-existing",
            50,
            AnswerKind::YesNo,
            ["topic:free-will", "answer:yes-no", "weight:50"],
        ),
    ]);
    // Make Q-existing a real near-duplicate of what the user is about to type.
    let bank = {
        let mut bank = bank;
        if let Some(q) = bank.questions.get_mut("Q-existing") {
            q.title = "Is the self continuous over time?".to_string();
        }
        bank
    };
    let config = test_config(&path, "Q-23");
    let persister = RecordingUserAuthoredPersister::default();
    let mut output = Vec::new();

    // Author a near-duplicate, then decline to add it anyway (blank -> No), then
    // answer the current question to end the session.
    run_session_with_user_authored_persister(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        &persister,
        "a\nOver time, is the self continuous?\n1\n\nyes\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    assert!(persister.calls.borrow().is_empty(), "nothing persisted");
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("near-duplicate already exists (Q-existing"));
    assert!(output.contains("Reusing Q-existing"));

    let _ = fs::remove_file(path);
}

// trace:STORY-88 | ai:claude
// A quick-add on a question with no `topic:` tag (e.g. a runtime prompt) falls
// back to a stable placeholder topic rather than vanishing.
#[test]
fn quick_add_topic_falls_back_when_current_has_no_topic() {
    let path = story_88_temp_log("no-topic");
    let bank = FakeBank::new([question_with_tags(
        "Q-23",
        70,
        AnswerKind::YesNo,
        ["answer:yes-no", "weight:70"],
    )]);
    let config = test_config(&path, "Q-23");
    let persister = RecordingUserAuthoredPersister::default();
    let mut output = Vec::new();

    run_session_with_user_authored_persister(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        &persister,
        "a\nWhat anchors identity?\n1\nyes\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let calls = persister.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, "user-authored");

    let _ = fs::remove_file(path);
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
        left_id: Some("BELIEF-X".to_string()),
        left: "X".to_string(),
        right_id: Some("BELIEF-Y".to_string()),
        right: "Y".to_string(),
        explanation: "edge".to_string(),
    }];
    let semantic = vec![
        Contradiction {
            kind: ContradictionKind::Semantic,
            left_id: None,
            left: "Y".to_string(),
            right_id: None,
            right: "X".to_string(),
            explanation: "semantic".to_string(),
        },
        Contradiction {
            kind: ContradictionKind::Semantic,
            left_id: None,
            left: "P".to_string(),
            right_id: None,
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

// trace:STORY-78 | ai:claude
#[test]
fn breadcrumb_line_shows_topic_depth_and_branch() {
    let question = question_with_tags(
        "Q-1",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    );

    assert_eq!(
        breadcrumb_line(&question, 0, "main", None),
        "[topic: free will | depth: 0 | branch: main]"
    );
    assert_eq!(
        breadcrumb_line(&question, 3, "agree", None),
        "[topic: free will | depth: 3 | branch: agree]"
    );
}

// trace:STORY-78 | ai:claude
#[test]
fn breadcrumb_line_falls_back_when_topic_tag_is_missing() {
    // A runtime-minted prompt (e.g. a surfaced contradiction) carries no
    // `topic:` tag; the breadcrumb must still render a stable placeholder.
    let untagged = question_with_tags(
        "contradiction-1",
        0,
        AnswerKind::FreeText,
        ["runtime:contradiction"],
    );

    assert_eq!(
        breadcrumb_line(&untagged, 2, "main", None),
        "[topic: (general) | depth: 2 | branch: main]"
    );
}

// trace:STORY-78 | ai:claude
#[test]
fn render_breadcrumb_is_plain_text_when_styling_disabled() {
    let question = question_with_tags(
        "Q-1",
        70,
        AnswerKind::YesNo,
        ["topic:meaning-of-life", "answer:yes-no", "weight:70"],
    );
    crate::style::set_enabled(false);
    let mut output = Vec::new();

    render_breadcrumb(&question, 1, "disagree", None, &mut output).unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert_eq!(
        rendered,
        "[topic: meaning of life | depth: 1 | branch: disagree]\n"
    );
    assert!(!rendered.contains('\u{1b}'), "no SGR escapes when plain");
}

// trace:STORY-78 | ai:claude
#[test]
fn session_shows_orientation_breadcrumb_each_turn() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-78-test-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([
        question_with_tags(
            "Q-1",
            70,
            AnswerKind::YesNo,
            ["topic:free-will", "answer:yes-no", "weight:70"],
        ),
        question_with_tags(
            "Q-2",
            60,
            AnswerKind::YesNo,
            ["topic:meaning", "answer:yes-no", "weight:60"],
        ),
    ])
    .with_edges("Q-1", ["Q-2"]);
    let mut config = test_config(&path, "Q-1");
    config.branch_id = "agree".to_string();
    let mut output = Vec::new();

    // Answer the seed (depth 0), then the follow-up (depth 1), then quit.
    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "yes\nyes\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let rendered = String::from_utf8(output).unwrap();
    // First turn: seed question, nothing answered yet -> depth 0.
    assert!(
        rendered.contains("[topic: free will | depth: 0 | branch: agree]"),
        "seed turn breadcrumb missing in:\n{rendered}"
    );
    // Second turn: one answer recorded on the path -> depth 1, new topic.
    assert!(
        rendered.contains("[topic: meaning | depth: 1 | branch: agree]"),
        "follow-up turn breadcrumb missing in:\n{rendered}"
    );

    let _ = fs::remove_file(path);
}

// trace:STORY-159 | ai:claude
// End-to-end: stating the goal in-session via `/goal <text>` records a
// `goal_set` event AND re-orients the breadcrumb — the SAME question is
// re-presented (non-destructive), now carrying the goal segment. Non-TTY safe.
#[test]
fn in_session_goal_command_sets_goal_and_shows_it_in_the_breadcrumb() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-159-in-session-goal-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question_with_tags(
        "Q-1",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    )]);
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    // First the `/goal` command (re-presents the seed), then answer the seed (so
    // the session survives, STORY-81), then quit at the dead-end menu.
    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "/goal can libertarian free will be held consistently?\nyes\nq\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let rendered = String::from_utf8(output).unwrap();
    // The seed shows free-flowing first, then again WITH the goal segment after
    // the command set it.
    assert!(
        rendered.contains("[topic: free will | depth: 0 | branch: main]"),
        "pre-goal breadcrumb missing in:\n{rendered}"
    );
    assert!(
        rendered.contains(
            "[topic: free will | depth: 0 | branch: main | goal: can libertarian free will be held consistently?]"
        ),
        "post-goal breadcrumb missing in:\n{rendered}"
    );
    assert!(rendered.contains("Goal set: can libertarian free will be held consistently?"));

    // The goal_set event is persisted (source:user) so resume restores it.
    let log = fs::read_to_string(&path).unwrap();
    assert!(log.contains(r#""event_type":"goal_set""#), "log:\n{log}");
    assert!(log.contains(r#""source":"user""#));
    assert!(log.contains("can libertarian free will be held consistently?"));

    let _ = fs::remove_file(path);
}

// trace:STORY-174 | ai:claude — the persistent score gauge defaults OFF: a fresh
// session never shows the `[score: …]` gauge line until `/score` is typed, even
// when a goal is set. Then `/score` toggles it ON (shows the gauge), and a second
// `/score` toggles it OFF (emits the gauge-off marker). Offline (no LLM in tests)
// the gauge reads a belief-neutral "needs LLM" note rather than a fabricated %.
#[test]
fn score_gauge_defaults_off_and_toggles_on_then_off() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-174-score-toggle-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question_with_tags(
        "Q-1",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    )]);
    let mut config = test_config(&path, "Q-1");
    config.goal = Some("is free will real?".to_string());
    let mut output = Vec::new();

    // Turn 1 frontier: gauge OFF by default. Then `/score` ON (re-presents Q-1
    // with the gauge), `/score` OFF (re-presents again), then answer + quit.
    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "/score\n/score\nyes\nq\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let rendered = String::from_utf8(output).unwrap();
    // The FIRST frontier render (before any `/score`) carries the breadcrumb but
    // NO gauge line — default off even though a goal is set.
    let first_breadcrumb = rendered
        .find("[topic:")
        .expect("a breadcrumb should render");
    let first_score = rendered.find("[score:");
    assert!(
        first_score.map(|s| s > first_breadcrumb).unwrap_or(true),
        "the gauge must not appear before the first /score:\n{rendered}"
    );
    // After `/score` ON the gauge line appears, offline => a "needs LLM" note
    // (no fabricated %), with a live freshness marker (a gate computation).
    assert!(
        rendered.contains("[score: needs LLM to score (live)]"),
        "gauge-on line missing:\n{rendered}"
    );
    // The second `/score` turns it OFF (emits the off marker the TUI clears on).
    assert!(
        rendered.contains("[score: off]"),
        "gauge-off marker missing:\n{rendered}"
    );

    let _ = fs::remove_file(path);
}

// trace:STORY-174 | ai:claude — the cost guard: with the gauge ON, the LLM-backed
// score recomputes only at GATES (every SCORE_GATE_TURNS answered turns), NOT
// every turn. Between gates the status bar shows the LAST value with a "cached"
// freshness marker. Offline the body is the "needs LLM" note either way, but the
// live/cached MARKER still distinguishes a gate recompute from a cached render,
// which is exactly the gate logic under test.
#[test]
fn score_gauge_recomputes_only_at_gates_not_every_turn() {
    use crate::SCORE_GATE_TURNS;
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-174-score-gate-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    // A CHAIN of distinct questions (Q-1 -> Q-2 -> … ) so several turns can be
    // answered in a row, accumulating answered turns toward the gate. One more
    // than the gate so the gate boundary is actually crossed.
    let chain_len = SCORE_GATE_TURNS + 2;
    let questions: Vec<Question> = (1..=chain_len)
        .map(|n| {
            question_with_tags(
                &format!("Q-{n}"),
                70,
                AnswerKind::YesNo,
                ["topic:free-will", "answer:yes-no", "weight:70"],
            )
        })
        .collect();
    let mut bank = FakeBank::new(questions);
    for n in 1..chain_len {
        let from = format!("Q-{n}");
        let to: &'static str = Box::leak(format!("Q-{}", n + 1).into_boxed_str());
        bank = bank.with_edges(&from, [to]);
    }
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    // Turn on the gauge, then answer down the chain. The toggle is a gate (live),
    // the FIRST few post-toggle frontier renders are cached until the gate at
    // SCORE_GATE_TURNS answered turns triggers a fresh (live) recompute.
    let mut script = String::from("/score\n");
    for _ in 0..chain_len {
        script.push_str("yes\n");
    }
    script.push('q');
    script.push('\n');
    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        script.as_bytes(),
        &mut output,
    )
    .unwrap();

    let rendered = String::from_utf8(output).unwrap();
    // At least one CACHED render appears between gates — proof the gauge does NOT
    // recompute (live) every turn.
    assert!(
        rendered.contains("(cached)"),
        "expected a cached render between gates:\n{rendered}"
    );
    // And at least one LIVE render appears at the gate boundaries (the toggle +
    // the next gate after SCORE_GATE_TURNS answers).
    assert!(
        rendered.matches("(live)").count() >= 2,
        "expected at least two live (gate) recomputes:\n{rendered}"
    );

    let _ = fs::remove_file(path);
}

// trace:STORY-159 | ai:claude — the `--goal` flag is recorded on the start event
// so the goal orients from turn one and survives resume.
#[test]
fn start_records_the_goal_flag_on_the_session_started_event() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-159-start-goal-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question_with_tags(
        "Q-1",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    )]);
    let mut config = test_config(&path, "Q-1");
    config.goal = Some("is free will real?".to_string());
    let mut output = Vec::new();

    // Answer the seed (so the session survives, STORY-81), then quit at the
    // dead-end menu (Q-1 has no successor).
    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "yes\nq\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let rendered = String::from_utf8(output).unwrap();
    // The breadcrumb carries the flag goal from the very first turn.
    assert!(
        rendered.contains("| goal: is free will real?]"),
        "{rendered}"
    );
    let log = fs::read_to_string(&path).unwrap();
    assert!(log.contains(r#""event_type":"session_started""#));
    assert!(
        log.contains(r#""goal":"is free will real?""#),
        "log:\n{log}"
    );

    let _ = fs::remove_file(path);
}

// ---- STORY-160: the closing ritual -------------------------------------

// trace:STORY-160 | ai:claude — the closing-ritual command recognizers. `rest`
// (and `rest case`) opens the closing phase; `verdict` requests the assessment;
// `terminate` invokes the fairness rule. A mid-sentence mention is NOT a command.
#[test]
fn closing_ritual_command_recognizers() {
    assert!(is_rest_command("rest"));
    assert!(is_rest_command("/rest"));
    assert!(is_rest_command("rest case"));
    assert!(is_rest_command("/REST CASE"));
    assert!(!is_rest_command("i need a rest from this"));

    assert!(is_verdict_command("verdict"));
    assert!(is_verdict_command("/verdict"));
    assert!(!is_verdict_command("the verdict is in"));

    assert!(is_terminate_command("terminate"));
    assert!(is_terminate_command("/TERMINATE"));
    assert!(!is_terminate_command("terminate the contract clause"));

    // The closing controls are distinct from the session-end controls: a plain
    // quit (`q` / `/end`) does NOT trigger the closing ritual.
    assert!(!is_rest_command("q"));
    assert!(!is_terminate_command("/end"));
}

// trace:STORY-160 | ai:claude — `rest case` is a PHASE TRANSITION: the session
// stops asking questions and switches to closing STATEMENTS. A user closing
// statement is recorded and the challenger answers with an objection; the log
// shows the phase change + both closing statements. Non-TTY safe (offline the
// challenger degrades to a structural objection).
#[test]
fn rest_case_enters_the_closing_phase_with_statements_not_questions() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-160-rest-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question_with_tags(
        "Q-1",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    )]);
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    // Rest the case at the frontier, make one closing statement, then ask for the
    // verdict (which ends the session).
    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "rest case\nMy settled position is that deliberation is real.\nverdict\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let rendered = String::from_utf8(output).unwrap();
    // The phase transition is announced and the prompt is for a closing STATEMENT,
    // not another question.
    assert!(
        rendered.contains("case rested") || rendered.contains("closing ritual"),
        "closing banner missing in:\n{rendered}"
    );
    assert!(
        rendered.contains("Your closing statement"),
        "closing statement prompt missing in:\n{rendered}"
    );
    // The challenger answers with a closing objection.
    assert!(
        rendered.contains("Challenger (closing"),
        "challenger objection missing in:\n{rendered}"
    );
    // The final verdict renders the belief-neutral STRUCTURE assessment.
    assert!(
        rendered.contains("final verdict") && rendered.contains("STRUCTURE"),
        "verdict header missing in:\n{rendered}"
    );

    let log = fs::read_to_string(&path).unwrap();
    assert!(
        log.contains(r#""event_type":"phase_changed""#) && log.contains(r#""phase":"closing""#),
        "phase_changed event missing:\n{log}"
    );
    assert!(
        log.contains(r#""event_type":"closing_statement""#) && log.contains(r#""speaker":"user""#),
        "user closing statement missing:\n{log}"
    );
    assert!(
        log.contains(r#""speaker":"challenger""#),
        "challenger closing statement missing:\n{log}"
    );

    let _ = fs::remove_file(path);
}

// trace:STORY-160 | ai:claude — `verdict` renders the belief-neutral roundedness
// assessment w.r.t. the goal and ends. The goal orients the verdict header.
#[test]
fn verdict_renders_the_belief_neutral_assessment_oriented_to_the_goal() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-160-verdict-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question_with_tags(
        "Q-1",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    )]);
    let mut config = test_config(&path, "Q-1");
    config.goal = Some("is free will real?".to_string());
    let mut output = Vec::new();

    // Go straight to the verdict from the (first) frontier prompt.
    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "verdict\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(
        rendered.contains("final verdict"),
        "verdict header missing in:\n{rendered}"
    );
    // Belief-neutral: the verdict pins STRUCTURE, never which belief is true.
    assert!(
        rendered.contains("NOT whether your belief is true"),
        "belief-neutral framing missing in:\n{rendered}"
    );
    // Oriented to the goal.
    assert!(
        rendered.contains("Resolving: is free will real?"),
        "goal orientation missing in:\n{rendered}"
    );

    let log = fs::read_to_string(&path).unwrap();
    assert!(log.contains(r#""phase":"closing""#), "log:\n{log}");

    let _ = fs::remove_file(path);
}

// trace:STORY-160 | ai:claude — the FAIRNESS RULE: the party that calls
// `terminate` forfeits the last word. When the USER terminates, the CHALLENGER
// makes the FINAL closing statement (its strongest remaining objection, logged
// final_word:true) before the verdict — and the user gets NO further statement.
#[test]
fn terminator_forfeits_the_last_word() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-160-terminate-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question_with_tags(
        "Q-1",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    )]);
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    // Rest at the frontier, make a statement, then TERMINATE. The user must NOT
    // get the last word: the challenger's final objection comes first. Any input
    // after `terminate` must be ignored (the ritual has ended), so we add a stray
    // line that should never be recorded as a user closing statement.
    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "rest\nDeliberation settles it.\nterminate\nthis line must be ignored\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let rendered = String::from_utf8(output).unwrap();
    // The fairness rule is named and the challenger gets the final word.
    assert!(
        rendered.contains("forfeit the last word") || rendered.contains("forfeited the last word"),
        "fairness-rule note missing in:\n{rendered}"
    );
    assert!(
        rendered.contains("final objection"),
        "challenger final objection missing in:\n{rendered}"
    );

    let log = fs::read_to_string(&path).unwrap();
    // Exactly one user closing statement was recorded (the stray post-terminate
    // line is NOT a closing statement — the terminator forfeits further turns).
    let user_statements = log
        .lines()
        .filter(|line| {
            line.contains(r#""event_type":"closing_statement""#)
                && line.contains(r#""speaker":"user""#)
        })
        .count();
    assert_eq!(
        user_statements, 1,
        "the terminator must not get another closing statement; log:\n{log}"
    );
    assert!(
        !log.contains("this line must be ignored"),
        "post-terminate input must not be recorded; log:\n{log}"
    );
    // The challenger's final word is flagged final_word:true.
    assert!(
        log.lines().any(|line| {
            line.contains(r#""speaker":"challenger""#) && line.contains(r#""final_word":true"#)
        }),
        "challenger's final word must be flagged; log:\n{log}"
    );

    let _ = fs::remove_file(path);
}

// trace:STORY-160 | ai:claude — offline the closing ritual degrades gracefully:
// the challenger's objection and the verdict both fall back to structural notes
// (no LLM), and a non-TTY / piped run renders the verdict rather than hanging.
#[test]
fn closing_ritual_degrades_gracefully_offline() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-160-offline-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question_with_tags(
        "Q-1",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    )]);
    // The session's observer uses the claude-cli backend; pointing it at a
    // nonexistent command forces the spawn to fail, exercising the offline
    // degradation path deterministically (same approach as STORY-127's test).
    std::env::set_var(
        "QUIZDOM_CLAUDE_COMMAND",
        "quizdom-no-such-closing-binary-xyz",
    );
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    // Rest, make a statement (gets a structural objection), then verdict. EOF
    // after that is handled gracefully.
    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "rest\nMy case stands.\nverdict\n".as_bytes(),
        &mut output,
    )
    .unwrap();
    std::env::remove_var("QUIZDOM_CLAUDE_COMMAND");

    let rendered = String::from_utf8(output).unwrap();
    // The offline challenger objection is clearly marked offline and stays
    // belief-neutral (structural).
    assert!(
        rendered.contains("Challenger (closing, offline)"),
        "offline objection marker missing in:\n{rendered}"
    );
    // The verdict still renders (degraded synopsis) rather than failing.
    assert!(
        rendered.contains("final verdict"),
        "offline verdict missing in:\n{rendered}"
    );

    let _ = fs::remove_file(path);
}

// trace:STORY-160 | ai:claude — a non-TTY / EOF at the closing prompt must not
// hang: an empty/closed input stream is treated as a request for the verdict, so
// a piped run that rests then closes still renders the verdict and ends.
#[test]
fn closing_phase_eof_renders_the_verdict_instead_of_hanging() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-160-eof-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question_with_tags(
        "Q-1",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    )]);
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    // Rest at the frontier, then the input stream ENDS (no verdict/terminate).
    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "rest\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(
        rendered.contains("final verdict"),
        "EOF at the closing prompt should render the verdict; got:\n{rendered}"
    );

    let _ = fs::remove_file(path);
}

// ---- STORY-161: debate mode --------------------------------------------

// trace:STORY-161 | ai:claude — `--mode debate` sets the session mode at start;
// the default is Socratic.
#[test]
fn mode_flag_sets_debate_at_start() {
    let config = CliConfig::parse([
        "session".to_string(),
        "start".to_string(),
        "--mode".to_string(),
        "debate".to_string(),
    ])
    .unwrap();
    assert_eq!(config.mode, SessionMode::Debate);
    assert!(config.mode_provided);
}

// trace:STORY-161 | ai:claude — default mode is unchanged (Socratic) and not
// flagged as provided.
#[test]
fn default_mode_is_socratic() {
    let config = CliConfig::parse(["session".to_string(), "start".to_string()]).unwrap();
    assert_eq!(config.mode, SessionMode::Socratic);
    assert!(!config.mode_provided);
}

// trace:STORY-161 | ai:claude — an unknown `--mode` value is a usage error (a
// typo never silently falls back to the default).
#[test]
fn unknown_mode_flag_is_a_usage_error() {
    let error = CliConfig::parse([
        "session".to_string(),
        "--mode".to_string(),
        "wager".to_string(),
    ])
    .unwrap_err();
    assert!(matches!(error, QuizdomError::Usage(_)));
}

// trace:STORY-161 | ai:claude — the in-session `/mode <text>` toggle parser.
// Recognized ONLY in the leading slash form (`/mode`); a bare `mode` keyword or a
// mid-answer mention of "mode" is NOT a command (unlike `/goal`).
#[test]
fn mode_command_parses_the_in_session_form() {
    assert_eq!(mode_command_text("/mode debate").as_deref(), Some("debate"));
    assert_eq!(
        mode_command_text("/MODE  Socratic").as_deref(),
        Some("Socratic")
    );
    // Bare `/mode` (no text) carries an empty string — the session reads it as
    // "show the current mode".
    assert_eq!(mode_command_text("/mode").as_deref(), Some(""));
    // A bare `mode` keyword or a free-text answer that merely contains "mode"
    // mid-sentence is an answer, not a command.
    assert!(mode_command_text("mode debate").is_none());
    assert!(mode_command_text("my mode of thinking is careful").is_none());
    assert!(mode_command_text("yes").is_none());
}

// trace:STORY-161 | ai:claude — the `--mode` flag is recorded on the start event
// so the verdict path frames the debate and resume restores it.
#[test]
fn start_records_the_mode_on_the_session_started_event() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-161-start-mode-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question_with_tags(
        "Q-1",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    )]);
    let mut config = test_config(&path, "Q-1");
    config.mode = SessionMode::Debate;
    let mut output = Vec::new();

    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "yes\nq\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let log = fs::read_to_string(&path).unwrap();
    assert!(log.contains(r#""event_type":"session_started""#));
    assert!(log.contains(r#""mode":"debate""#), "log:\n{log}");

    let _ = fs::remove_file(path);
}

// trace:STORY-161 | ai:claude — a resumed debate session restores its mode (from
// the start event, then the latest in-session `mode_set`) so it keeps the same
// questioning style without re-passing `--mode`.
#[test]
fn resume_restores_the_mode_latest_wins() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-161-resume-mode-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    fs::write(
        &path,
        concat!(
            r#"{"event_type":"session_started","branch_id":"main","strategy":"deterministic","mode":"socratic","session_id":"sess-test","user_id":"test-user","seed_question_ref":"Q-1"}"#,
            "\n",
            r#"{"event_type":"mode_set","branch_id":"main","turn":1,"mode":"debate","session_id":"sess-test","user_id":"test-user"}"#,
            "\n",
        ),
    )
    .unwrap();
    let config = CliConfig::parse([
        "session".to_string(),
        "resume".to_string(),
        "--log".to_string(),
        path.to_string_lossy().to_string(),
    ])
    .unwrap();

    let resolved = resolve_resume_config(config).unwrap();
    assert_eq!(resolved.mode, SessionMode::Debate);

    let _ = fs::remove_file(path);
}

// trace:STORY-161 | ai:claude — an explicit `--mode` on the resume command wins
// over the logged mode (the user's override is respected).
#[test]
fn explicit_mode_flag_overrides_the_logged_mode_on_resume() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-161-resume-mode-override-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    fs::write(
        &path,
        concat!(
            r#"{"event_type":"session_started","branch_id":"main","strategy":"deterministic","mode":"debate","session_id":"sess-test","user_id":"test-user","seed_question_ref":"Q-1"}"#,
            "\n",
        ),
    )
    .unwrap();
    let config = CliConfig::parse([
        "session".to_string(),
        "resume".to_string(),
        "--mode".to_string(),
        "socratic".to_string(),
        "--log".to_string(),
        path.to_string_lossy().to_string(),
    ])
    .unwrap();

    let resolved = resolve_resume_config(config).unwrap();
    assert_eq!(resolved.mode, SessionMode::Socratic);

    let _ = fs::remove_file(path);
}

// trace:STORY-161 | ai:claude — end-to-end: toggling to debate in-session via
// `/mode debate` records a `mode_set` event and confirms the switch; the
// confirmation pins the belief-neutral-on-truth contract.
#[test]
fn in_session_mode_toggle_sets_debate_and_logs_it() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-161-toggle-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question_with_tags(
        "Q-1",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    )]);
    let config = test_config(&path, "Q-1");
    let mut output = Vec::new();

    // Toggle to debate (re-presents the seed), answer the seed (so the session
    // survives), then quit at the dead-end menu.
    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "/mode debate\nyes\nq\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("Mode set: debate"), "{rendered}");
    assert!(
        rendered.contains("never which belief is true"),
        "the toggle must pin belief-neutrality on truth:\n{rendered}"
    );
    let log = fs::read_to_string(&path).unwrap();
    assert!(log.contains(r#""event_type":"mode_set""#), "log:\n{log}");
    assert!(log.contains(r#""mode":"debate""#), "log:\n{log}");

    let _ = fs::remove_file(path);
}

// trace:STORY-161 | ai:claude — the debate-mode verdict judges which CASE was
// better-ARGUED (argument STRUCTURE), never which belief is true. The default
// Socratic verdict is unchanged (asserted by the STORY-160 verdict test).
#[test]
fn debate_mode_verdict_judges_argument_structure_not_truth() {
    let path = std::env::temp_dir().join(format!(
        "quizdom-story-161-verdict-{}.jsonl",
        std::process::id()
    ));
    let _ = fs::remove_file(&path);
    let bank = FakeBank::new([question_with_tags(
        "Q-1",
        70,
        AnswerKind::YesNo,
        ["topic:free-will", "answer:yes-no", "weight:70"],
    )]);
    let mut config = test_config(&path, "Q-1");
    config.mode = SessionMode::Debate;
    let mut output = Vec::new();

    // Go straight to the verdict from the frontier prompt.
    run_session(
        &config,
        &bank,
        &DeterministicNextQuestionStrategy,
        "verdict\n".as_bytes(),
        &mut output,
    )
    .unwrap();

    let rendered = String::from_utf8(output).unwrap();
    assert!(
        rendered.contains("final verdict"),
        "verdict header missing in:\n{rendered}"
    );
    // Debate framing: which CASE was better-argued (STRUCTURE), never truth.
    assert!(
        rendered.contains("which CASE was better-ARGUED"),
        "debate verdict framing missing in:\n{rendered}"
    );
    assert!(
        rendered.contains("NOT which belief is true"),
        "belief-neutral-on-truth framing missing in:\n{rendered}"
    );

    let _ = fs::remove_file(path);
}
