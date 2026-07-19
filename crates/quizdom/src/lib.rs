// trace:BUG-200 | ai:claude — the pinned-format choke point for aida spawns.
mod aida_cmd;
// trace:TASK-56 | ai:codex
mod bank;
mod contradiction;
// trace:STORY-205 | ai:claude — Dolt repo bootstrap for the domain graph (EPIC-202).
mod db_init;
// trace:STORY-206 | ai:claude — AIDA-store → Dolt exporter with parity check (EPIC-202).
mod db_migrate;
// trace:STORY-207 | ai:claude — the Dolt-backed DomainStore + backend selection (EPIC-202).
mod dolt_store;
// trace:STORY-180 | ai:claude — the capable TUI free-text editor (tui-textarea).
mod editor;
mod error;
// trace:STORY-168 | ai:claude
mod frontend;
mod honing;
mod input;
// trace:STORY-176 | ai:claude
mod keymap;
mod model;
// trace:STORY-127 | ai:claude
mod observer;
// trace:STORY-163 | ai:claude
mod palette;
mod persist;
// trace:STORY-87 | ai:claude
mod question_add;
mod session;
// trace:STORY-194 | ai:claude — the runtime settings surface (/settings, /editor)
// + the small persisted config file.
mod settings;
// trace:STORY-68 | ai:claude
mod signals;
// trace:STORY-204 | ai:claude — the DomainStore storage abstraction (EPIC-202).
mod store;
// trace:STORY-83 | ai:claude
mod spinner;
mod strategy;
// trace:STORY-76 | ai:claude
mod style;
// trace:STORY-179 | ai:claude — TUI markdown renderer (inline+block) with quote-yellow (BUG-178).
mod markdown;
// trace:STORY-169 | ai:claude
mod tui;
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
    NoopContradictionResolutionPersister,
};
// trace:STORY-205 | ai:claude
pub use db_init::{run_db_init, DEFAULT_DOLT_DB_PATH, DOLT_SCHEMA_SQL};
pub use db_migrate::{run_db_migrate, DEFAULT_SPOT_CHECK_ROOT};
// trace:STORY-207 | ai:claude
pub use dolt_store::{domain_store_from_config, DoltDomainStore, SelectedDomainStore};
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
// trace:STORY-204 | ai:claude
pub use session::run_cli;
pub use store::{AidaDomainStore, DomainStore, EdgeKind, NewNode, NodeKind, NodeRecord};
// trace:STORY-68 | ai:claude
pub use signals::{
    analyze_session_log, apply_log_signals, run_curate, signals_from_log, QuestionSignalStats,
    ReweightOutcome, DEEP_BRANCH_DEPTH, PUNT_RATE_THRESHOLD,
};
pub use strategy::{
    assist_user_question, reweight, AnsweredQuestion, QualitySignal, SessionMode, StrategyContext,
    TurnEnvelope, UserQuestionAssist,
};
pub use strategy::{
    DeterministicNextQuestionStrategy, LlmNextQuestionStrategy, NextQuestionStrategy,
    WeightSampler, WeightedNextQuestionStrategy, XorShiftWeightSampler,
};
// trace:STORY-128 | ai:claude
// trace:STORY-174 | ai:claude — the persistent `/score` gauge (ScoreGauge) +
// its gate cadence (SCORE_GATE_TURNS) join the synopsis surface.
pub use synopsis::{
    arc_from_session_log, parse_synopsis, render_synopsis, run_session_synopsis,
    structural_synopsis, synopsize, ScoreGauge, SessionArc, SessionSynopsis, SessionTurn,
    SCORE_GATE_TURNS,
};
// trace:STORY-77 | ai:claude
pub use transcript::{render_transcript, run_session_show};

/// Top-level usage text listing every subcommand the binary dispatches.
///
/// `quizdom --help` prints this and exits 0 (each subcommand keeps its own
/// `-h`). Keep the command list in sync with the dispatch in `main.rs`.
// trace:TASK-199 | ai:claude
pub fn top_level_usage() -> String {
    [
        "usage: quizdom [command] [options]",
        "",
        "Running with no command starts a new session (same as `session start`).",
        "",
        "Commands:",
        "  session start            Start a new session (interactive TUI on a TTY)",
        "  session resume [id]      Resume a session; omit id to resume latest",
        "  session list             List saved sessions for a user",
        "  session fork             Fork a proposition into agree/disagree branches",
        "  session show <id>        Pretty-print a saved session's full transcript",
        "  session synopsis <id>    Summarize a saved session's arc",
        "  contradictions           Detect contradictions among adopted beliefs",
        "  curate                   Re-weight the question bank from session signals",
        "  db-init                  Create the Dolt domain-graph repo and apply its schema",
        "  db-migrate               Migrate the domain graph from the AIDA store into Dolt",
        "  question add             Author a new question into the bank",
        "  -h, --help               Show this help",
        "",
        "Run `quizdom <command> --help` for command-specific options.",
    ]
    .join("\n")
}

#[cfg(test)]
mod tests;
