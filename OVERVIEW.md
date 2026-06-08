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
- **Interface → CLI/TUI first (ADR-4).** A full-screen **ratatui + crossterm**
  TUI is now the default for interactive sessions (EPIC-167); a headless line
  front-end behind the same engine seam serves non-TTY / scripted / piped paths
  and the standalone commands. Web deferred to the multi-user era; the
  session/graph core stays interface-agnostic.
- **One turn-envelope LLM call per turn (ADR-187).** The interrogator's
  next-question call returns a structured envelope — `{ next_question,
  objection?, goal_offer? }` — so the objection / goal-offer decisions are a
  near-free byproduct of a call we already make, instead of separate
  full-history probes every turn (a cost fix).

Still open:

- **LLM integration.** Settled in EPIC-7: a provider-agnostic `llm` crate with a
  `ClaudeCliClient` default (runs on the Max plan via `claude -p`, ADR-39) and an
  opt-in `AnthropicClient`. Tracked-separately: the deeper O(turns²) full-history
  re-send growth (a finding noted in ADR-187).
- **Graph model specifics.** Settled in EPIC-5: question / belief / definition
  nodes joined by `begets` / `contradicts` / `refines` / `agrees` / `disagrees`
  custom edges (`docs/architecture/graph-schema.md`).

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
- **EPIC-11 (CLI/TUI polish) — complete.** Styled output, thinking spinner,
  orientation breadcrumb, session-end resume hints, empty-session discard,
  concurrent-session-safe resume.
- **EPIC-84 (user-authored questions) — complete.** Author questions into the
  bank standalone (`quizdom question add`) or mid-session (the `A` key), with
  LLM dedup + refinement.
- **EPIC-126 (the Observer) — complete.** A belief-neutral meta layer: `?`
  reads the current exchange (what's asked, where you went off-track, what a
  precise answer must address); `S` / `quizdom session synopsis` summarizes
  the arc + your engagement. Clarifies and coaches, never advocates a belief.
- **EPIC-154 (convergence) — complete.** A belief-neutral *roundedness* score in
  the synopsis (consistency/clarity/completeness/coherence + the limiting gap) and
  an offer-to-conclude when you cross 'well-rounded'.
- **EPIC-158 (session framing) — complete.** A `--goal`/`/goal` that orients the
  questioning + scoring; a closing ritual (`/rest` -> closing statements ->
  `verdict`, terminator forfeits the last word); and a `--mode debate` toggle
  where the questioner steelmans the opposing side.
- **EPIC-162 (TUI overlays) — complete.** A `/` slash-command palette (menu +
  descriptions + `?`-help, crossterm), plus `/help` (how the tool works) and
  `/tutor` (helps you articulate your point + the nuance you're missing).
- **EPIC-167 (full ratatui TUI front-end) — landed.** The interactive front-end
  is now a real full-screen TUI, built on a front-end seam that keeps the session
  engine front-end-agnostic (STORY-168): a headless line front-end preserves
  every existing test / non-TTY / scripted path, while a ratatui front-end is the
  default for an interactive TTY. What landed across STORY-168..194:
  - **Shell + palette** (STORY-169): alternate screen, layout, event loop, and a
    live `/` palette that opens on the keystroke and redraws in place (replacing
    the EPIC-162 Enter-to-open overlay) — with filter modes (leading-`/` =
    command-name prefix vs. substring search, STORY-177) and context-aware greying
    of inapplicable commands (STORY-190).
  - **Theme** (STORY-171, BUG-172): colored borders, gold cursor, per-role colors,
    symmetric quote-attribution.
  - **Keyboard + discoverability** (STORY-176): navigation keys and a cheat-sheet
    driven from one keymap registry; F1 help alias (STORY-194).
  - **Markdown rendering** (STORY-179, BUG-178): inline + block markdown in the
    transcript with an always-on quote-yellow color.
  - **Free-text editor** (STORY-180, BUG-183/184): a capable answer editor —
    readline/Emacs + optional Vim via `tui-textarea`, open-in-`$EDITOR` escape,
    soft-wrap + dynamic vertical grow, and a post-submit "thinking" state.
  - **Focus + transcript** (STORY-193, STORY-191): Tab focus model, scrollbar,
    mouse support, and a full styled scrollable transcript that hydrates prior
    history on resume.
  - **Settings** (STORY-194): a `/settings` panel + runtime `/editor` toggle
    (vim/emacs/auto), persisted to config.
  - Session-mechanic stories also landed in this batch: request-a-goal when none
    is set (STORY-173), the `/score` distance-to-goal gauge (STORY-174), and the
    `/objection` court mechanic — pin a contested point, asymmetric `/resolved`
    (objector) vs. `/judge` (other party → Observer ruling) exits (STORY-175).
  - **Cost fix** (ADR-187, STORY-188): consolidated the per-turn goal / objection
    probes into one structured turn-envelope on the next-question call — one LLM
    call per turn instead of 2-3 full-history spawns (BUG-181 also de-flaked the
    score-gauge gate test off a live LLM).

**Every epic is complete** (~521 quizdom + 7 llm tests, CI green, runs on the Max
plan by default). The product is the full vision plus the use-driven extensions;
further work is driven entirely by real use.

Substrate gaps surfaced by dogfooding (VIS-2) are filed as findings or upstream
`~/ai/aida` issues (FR-282 custom-edge traversal, BUG-415/417). Cost / scaling
gaps surfaced by real use — the O(turns²) full-history re-send growth flagged in
ADR-187 — are likewise tracked as findings.
