# CLAUDE.md

Guidance for Claude Code working in this repository. AIDA conventions
(trace format, commit format, daily commands, capture rules) live in
`.claude/AIDA.md` — Claude Code expands the import below automatically,
so you'll see them in context without this file having to duplicate
them.

@.claude/AIDA.md

## Project overview

**quizdom** (quiz + wisdom) is a Socratic, branching belief-exploration tool —
not trivia, no correct answers. It maps and challenges a user's beliefs about
existential questions via yes/no, multiple-choice, and free-text questions,
persisting a graph of understanding. See `OVERVIEW.md` and `aida show VIS-1`.
A hidden goal (`VIS-2`) is to dogfood and improve AIDA as a general-purpose
data substrate.

## Architecture & key decisions

- **Stack: Rust** (`ADR-32`). Cargo workspace; the app is `crates/quizdom`
  (binary). A provider-agnostic `llm` crate is coming in EPIC-7 (`ADR-34`) —
  built fresh here, not extracted from `~/ai/aida-chat`.
- **Data lives in AIDA** (`ADR-3`): no separate DB. The domain graph is AIDA
  objects — `Q-*` questions, `TERM-*` definitions (`--type term`), `BELIEF-*`
  propositions — joined by custom edges (`begets`/`probes`/`refines`/
  `contradicts`/`agrees`/`disagrees`). Canonical schema:
  `docs/architecture/graph-schema.md`. The app reads/writes by shelling out to
  the `aida` CLI.
- **Graph traversal is app-side** (`ADR-31`): `aida graph`/`query_graph` cannot
  follow custom edges (upstream `~/ai/aida` FR-282), so we walk one hop at a
  time via `aida rel list <node> --type <edge>`.
- **Interface: CLI/TUI** (`ADR-4`); web deferred. **Weighting** uses `weight:N`
  tags computed in-app (`ADR-22`).

## Development

```bash
cargo test                 # workspace tests
cargo run -p quizdom       # run the CLI session loop (reads seed data via aida)
cargo build                # build
```

Layout: `Cargo.toml` (workspace) · `crates/quizdom/{src/main.rs,src/lib.rs}`.

## Agent working discipline

Rules for driving the multi-agent fleet on this project (learned the hard way —
see the VIS-2 findings).

### Launch an implementer (one command, isolated)

```bash
aida agent new claude --role implementer --spec <SPEC>   # Claude implementer
aida agent new codex  --role implementer --spec <SPEC>   # Codex implementer
```

`--spec` creates a scoped sibling worktree + lease, spawns the agent, and
auto-prompts it to work `<SPEC>` — no `/aida-pickup`, no manual `aida session
start`, no `cd`. NEVER `git checkout -b` in the shared main worktree (causes
lease/role bleed, scope bleed, stale-branch breakage). Even inside a scoped
worktree, verify cwd before editing — an agent can still accidentally write to
the shared main checkout (a near-miss we hit). If a worktree already exists
(e.g. prepped via `aida session start`), attach with `--cwd <worktree>` instead
of `--spec` (which errors "already owned").

### Work routing (advisor posts detail; agent reads it)

- **codex** reads its detail from a **brief** the advisor posts:
  `aida brief codex <SPEC> --note ...` → the agent sees it via
  `aida brief list --for-agent codex`. (codex has no `/aida-pickup` skill.)
- **claude** gets its detail from the `--spec` auto-prompt directly.
- Keep parallel agents on **file-disjoint** specs — two implementers editing
  the same file is the real failure mode.

### Ship & reap

- Ship via branch + PR to `main` (`ADR-21`). CI (`.github/workflows/ci.yml`)
  runs fmt + clippy + test, so `aida pr ship` self-completes (no `gh` fallback).
  A spec is `Completed` only when its PR **merges**. Pure AIDA-store data (e.g.
  seed clusters) lands via `aida push --store-only`, not a code PR.
- Reap a finished worktree from the MAIN repo (not from inside it): exit the
  agent, then `aida session end <lease-id> --skip-ci -y` (`--skip-ci` avoids the
  BUG-422 hang; lease ids from `aida session leases`).

## Discipline for AIDA-using sessions

How to work effectively with AIDA on this project — the longer-form guides
live in `docs/aida/discipline/` (scaffolded by `aida init`).

- **Roles** — the advisor seat captures friction, gardens the queue, and
  hands code work to an implementer; it does not write code itself. See
  `docs/aida/discipline/advisor-role.md`.
- **Lifecycle words** — committed / pushed / merged / completed / released
  are distinct states; don't collapse them under "ship". See
  `docs/aida/discipline/lifecycle-vocabulary.md`.
- **Machinery vocabulary** — orchestrator / phase / drain / lease / role /
  scope / session / worktree / sentinel / batch / autonomy mode each have
  one precise definition. See
  `docs/aida/discipline/machinery-glossary.md`.
- **Tag conventions** — subcommand tags use the `aida:<subcommand>`
  colon-namespaced form (`aida:queue:work`, `aida:db:sync:pull`) so
  `aida list --tags 'aida:queue:*'` returns the surface; behavior /
  provenance / severity tags stay flat. See
  `docs/aida/discipline/tag-conventions.md`.
- **Workflow patterns** — `/goal` prompts use real flags only; "next steps"
  UI splits into parallel-choice tables vs sequential-step lists. See
  `docs/aida/discipline/workflow-patterns.md`.
- **Session habits** — verify before filing, pause for design input, trust
  the reviewer, check for in-flight work before rejecting. See
  `docs/aida/discipline/session-discipline.md`.
- **Ecosystem positioning** — for "where does AIDA fit / vs X?" questions
  (Claude Code `/agents` & `/ultra*` family, hosted SaaS PM, markdown-only
  patterns, neighbouring AI coding tools), consult `docs/positioning/`
  rather than improvising; capture gaps as new positioning docs.
- **Start here** — `docs/aida/discipline/README.md` indexes the pack and
  explains the companion starter memory pack (`aida init --with-memories`).

