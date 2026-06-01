# quizdom — Overview

> **quiz + wisdom.** A Socratic, branching belief-exploration tool. Not trivia —
> there are no correct answers. The aim is to help a person map, examine, and
> challenge their own beliefs about existential / philosophical questions, and
> along the way teach them semantic nuances they hadn't noticed.

Captured requirements: see AIDA (`aida show VIS-1`, `aida show VIS-2`). This file
is the human-readable companion, not the source of truth.

## Vision

People hold beliefs ("I believe in free will") without having interrogated what
they mean by the terms, whether those beliefs are internally consistent, or how
their position relates to established academic / public definitions. General
trivia apps test recall; nothing helps you explore and stress-test what you
*believe*. quizdom does.

## Target users

- **v1: a single user (Joe).** No accounts or multi-tenant concerns yet.
- The architecture should anticipate multiple users later — questions asked
  across users, and cross-user weighting of question quality.

## How it works (the experience)

1. A session starts from a **seed question** (e.g. "Do you believe in free
   will?") posed as **yes/no or multiple choice**.
2. Each answer **begets more questions**, branching down a tree / graph — a
   persisted **graph of understanding**.
3. When a **loaded term** appears, the system drills into what the user means via
   **free-text input** (Claude-Code-style) and tries to **steer toward a formal,
   academically / publicly agreed definition** before falling back to a bespoke
   user-specific one (which is allowed but aimed against).
4. An **LLM analyzes each answer** and either generates a new question or selects
   a fitting one from the evolving question bank.
5. The system **surfaces contradictions** across the user's answers, and lets the
   user **explore both sides** of a proposition (agree *and* disagree) to see
   what lies down each branch. Challenging existing beliefs is the point.

## Evolving knowledge base

- Questions live in a **git-backed knowledge base** that evolves over time.
- Questions are **tagged insightful vs unhelpful**; tags govern whether a
  question is asked of future users and how heavily it's weighted when chosen
  again. Good questions surface; weak ones fade.

## Architecture (early thinking — not yet decided)

Decided early:

- **Data substrate → AIDA (ADR-3).** quizdom's domain data — questions as typed
  objects, belief relations as typed edges, tags as quality signals — lives in
  the AIDA git-canonical store, dogfooding it as a general substrate (VIS-2).
  Where AIDA can't express what quizdom needs, file an AIDA finding rather than
  working around it.
- **Interface → CLI/TUI first (ADR-4).** Terminal, Claude-Code-style prompt.
  Web deferred to the multi-user era; the session/graph core stays
  interface-agnostic.

Still open:

- **LLM integration.** Which model / API drives answer analysis and question
  generation (Claude API is the natural default on this machine).
- **Graph model specifics.** Exact node and edge types (question, belief,
  definition; begets, contradicts, refines, agrees/disagrees) — owned by EPIC-5.

## Non-goals (v1)

- Not trivia; no scoring of "correctness."
- Not multi-user / social / accounts yet.
- Not steering the user to a *predetermined belief* — the steering is toward
  *shared definitions*, not a particular conclusion.

## Status

A working Rust session engine. Decisions live in ADRs (`aida list --type
decision`); progress in the EPIC tree (`aida list --type epic`).

- **EPIC-5 (domain graph model) — complete.** Schema (`docs/architecture/
  graph-schema.md`) + the "free will" seed cluster (`Q-23`, `TERM-24/25`,
  `BELIEF-28/29`) live as AIDA objects with custom edges.
- **EPIC-6 (session engine) — complete.** `crates/quizdom`: branching Q&A loop,
  pluggable `NextQuestionStrategy` (deterministic), both-sides agree/disagree
  forking, and start/resume/end persistence over a JSONL log. 9 tests green.
- **EPIC-7 (LLM integration) — complete.** Provider-agnostic `llm` crate with
  two backends: `ClaudeCliClient` (default — runs on the Max plan via `claude
  -p`, no API charges, ADR-39) and `AnthropicClient` (opt-in, API key). The
  `LlmNextQuestionStrategy` selects bank questions or mints new ones that
  persist back to the bank. Live `claude -p` smoke verified.
- **EPIC-8 (semantic honing) — complete.** Surface competing definitions →
  capture the user's meaning → LLM-map to a formal definition → steer to adopt
  it → record & reuse the settled meaning.
- **EPIC-9 (contradiction detection) — complete.** Detect (graph + LLM) →
  surface in-session → resolve (confirm `contradicts` edge + decision record).
  `quizdom contradictions` lists them standalone.
- **EPIC-10 (bank evolution) — complete.** Answer-conditioned follow-ons,
  re-weighting engine, weighted-probabilistic selection, log-derived quality
  signals, and `quizdom curate` to run the loop.
- **EPIC-50 (interaction model) — complete.** Single-key Y/N/X/P/B/F/Q,
  eXplore-then-honing, Punt-to-new-topic, B/F review+revise, resume
  discoverability + strategy restoration.
- **EPIC-11 (CLI/TUI polish)** — draft, low priority.

**The vision is feature-complete** (~108 tests, CI green, runs on the Max plan
by default). Remaining work is EPIC-11 polish and whatever real use surfaces.

Substrate gaps surfaced by dogfooding (VIS-2) are filed as findings or upstream
`~/ai/aida` issues (FR-282 custom-edge traversal, BUG-415/417).
