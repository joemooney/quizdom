// trace:TASK-56 | ai:codex
mod bank;
mod contradiction;
mod error;
mod honing;
mod input;
mod model;
mod persist;
mod session;
mod strategy;

pub use bank::{
    parse_begets_rel_list, parse_probes_rel_list, parse_question_show, parse_term_show,
    rewrite_weight_and_quality_tags, AidaCliQuestionBank, QuestionBank,
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
    Answer, AnswerKind, Question, QuestionRef, TermDefinition, TermMappingProposal, TermRef,
};
pub use persist::{
    GeneratedQuestionPersister, NoopGeneratedQuestionPersister, NoopQuestionReweighter,
    QuestionReweighter,
};
pub use session::run_cli;
pub use strategy::{reweight, AnsweredQuestion, QualitySignal, StrategyContext};
pub use strategy::{
    DeterministicNextQuestionStrategy, LlmNextQuestionStrategy, NextQuestionStrategy,
    WeightSampler, WeightedNextQuestionStrategy, XorShiftWeightSampler,
};

#[cfg(test)]
mod tests;
