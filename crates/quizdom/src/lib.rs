// trace:TASK-56 | ai:codex
mod bank;
mod error;
mod honing;
mod input;
mod model;
mod persist;
mod session;
mod strategy;

pub use bank::{
    parse_begets_rel_list, parse_probes_rel_list, parse_question_show, parse_term_show,
    AidaCliQuestionBank, QuestionBank,
};
pub use error::{QuizdomError, Result};
pub use model::{
    Answer, AnswerKind, Question, QuestionRef, TermDefinition, TermMappingProposal, TermRef,
};
pub use persist::{GeneratedQuestionPersister, NoopGeneratedQuestionPersister};
pub use session::run_cli;
pub use strategy::{AnsweredQuestion, StrategyContext};
pub use strategy::{
    DeterministicNextQuestionStrategy, LlmNextQuestionStrategy, NextQuestionStrategy,
};

#[cfg(test)]
mod tests;
