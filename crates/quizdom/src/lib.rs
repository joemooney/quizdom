// trace:TASK-56 | ai:codex
mod bank;
mod contradiction;
mod error;
mod honing;
mod input;
mod model;
// trace:STORY-127 | ai:claude
mod observer;
mod persist;
// trace:STORY-87 | ai:claude
mod question_add;
mod session;
// trace:STORY-68 | ai:claude
mod signals;
// trace:STORY-83 | ai:claude
mod spinner;
mod strategy;
// trace:STORY-76 | ai:claude
mod style;
// trace:STORY-128 | ai:claude
mod synopsis;
// trace:STORY-77 | ai:claude
mod transcript;

pub use bank::{
    find_near_duplicate, parse_begets_rel_list, parse_probes_rel_list, parse_question_show,
    parse_term_show, rewrite_weight_and_quality_tags, AidaCliQuestionBank, NearDuplicate,
    QuestionBank, DEDUP_SIMILARITY_THRESHOLD,
};
pub use contradiction::{
    beliefs_from_session_log, detect_graph_contradictions, detect_semantic_contradictions,
    merge_contradictions, parse_contradicts_rel_list, run_contradictions, AdoptedBelief,
    AidaCliContradictionResolutionPersister, AidaCliContradictsEdges, Contradiction,
    ContradictionKind, ContradictionResolution, ContradictionResolutionPersister, ContradictsEdges,
    NoopContradictionResolutionPersister, ResolutionCommandRunner,
};
pub use error::{QuizdomError, Result};
pub use model::{
    Answer, AnswerKind, Question, QuestionRef, RefinementProposal, TermDefinition,
    TermMappingProposal, TermRef,
};
// trace:STORY-127 | ai:claude
pub use observer::{parse_reading, read_exchange, structural_reading, Exchange, ExchangeReading};
pub use persist::{
    GeneratedQuestionPersister, NoopGeneratedQuestionPersister, NoopQuestionReweighter,
    NoopUserAuthoredQuestionPersister, QuestionLink, QuestionReweighter,
    UserAuthoredQuestionPersister,
};
// trace:STORY-87 | ai:claude
pub use question_add::run_question_add;
pub use session::run_cli;
// trace:STORY-68 | ai:claude
pub use signals::{
    analyze_session_log, apply_log_signals, run_curate, signals_from_log, QuestionSignalStats,
    ReweightOutcome, DEEP_BRANCH_DEPTH, PUNT_RATE_THRESHOLD,
};
pub use strategy::{
    assist_user_question, reweight, AnsweredQuestion, QualitySignal, SessionMode, StrategyContext,
    UserQuestionAssist,
};
pub use strategy::{
    DeterministicNextQuestionStrategy, LlmNextQuestionStrategy, NextQuestionStrategy,
    WeightSampler, WeightedNextQuestionStrategy, XorShiftWeightSampler,
};
// trace:STORY-128 | ai:claude
pub use synopsis::{
    arc_from_session_log, parse_synopsis, render_synopsis, run_session_synopsis,
    structural_synopsis, synopsize, SessionArc, SessionSynopsis, SessionTurn,
};
// trace:STORY-77 | ai:claude
pub use transcript::{render_transcript, run_session_show};

#[cfg(test)]
mod tests;
