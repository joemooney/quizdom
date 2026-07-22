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
- **Domain data lives in Dolt** (`ADR-201`, EPIC-202, supersedes `ADR-3`;
  cutover: `STORY-208`; store-side domain objects deleted post-cutover:
  `STORY-209`): the domain graph — `Q-*` questions, `TERM-*`
  definitions, `BELIEF-*` propositions, joined by custom edges
  (`begets`/`probes`/`refines`/`contradicts`/`agrees`/`disagrees`) — lives
  in a local Dolt repo (`quizdom db-init` bootstraps it, `quizdom
  db-migrate` re-imports from a legacy AIDA store; default path
  `data/dolt`, override with `QUIZDOM_DOLT_PATH` or `dolt_path = ...` in
  `~/.config/quizdom/settings.toml` — a **relative** `dolt_path` anchors to
  that settings file's directory, not the cwd, so one config line names one
  graph from every worktree; the env var, the `--path` flag, and the compiled
  `data/dolt` default stay cwd-relative. A leading `~` expands to `$HOME`, and a
  double-quoted value is a TOML *basic* string (escapes processed) while a
  single-quoted one is *literal* (`STORY-350`). Rule and rationale:
  `OVERVIEW.md` § *Settings, and how a relative path resolves* (`STORY-290`)).
  AIDA remains canonical for project
  intent, including contradiction-resolution decision nodes and
  `references` edges (`AidaIntentStore` — the only runtime aida writes).
  Canonical schema: `docs/architecture/graph-schema.md` + `db/schema.sql`.
  **Whose commit is it** (`STORY-351`): `db-backup`'s snapshot is the only
  commit that stages the whole working set (`dolt add -A`) — breadth is its
  job, and "snapshot working set" claims nothing about authorship. Every other
  writer stages `nodes`/`edges` **by name** (`db_init::commit_tables`), because
  their messages do make a claim. Staging is table-granular, so every writer
  also asks **before** it writes (`db_init::begin_write`) and refuses when a
  table it is about to stage already holds a hand-run edit — quizdom cannot
  author a message for a change it did not make, and refusing beats mislabelling
  it or dropping it. **Whose changes are they** (`BUG-366`): the answer is the
  `dolt_status.staged` flag, not a memory. Every quizdom write stages itself in
  the same `dolt sql` call (`db_init::staging_write` appends `CALL
  DOLT_ADD('nodes','edges')`), so staged-but-uncommitted rows are an unfinished
  quizdom run — **resumed**, with a line saying so — and unstaged rows are
  refused. Before this, a `db-migrate` that failed parity was blocked on its own
  leftovers by a message blaming a hand edit that never happened. The three
  writers share one seam: `begin_write` hands back a `WriteClaim` and
  `commit_tables` takes that claim rather than a path, so a writer cannot reach
  the commit tail without passing the pre-flight — the gap that let the store,
  the writer a session runs on every answer, silently miss it (`TASK-357`).
  `db-migrate` commits **last**, after parity + the BUG-231 cross-check + the
  spot-check agree: its message asserts the counts it carried, and a failed run
  used to leave permanent history asserting what that same run had just
  disproved. A missing `dolt_path` directory now reports the missing directory
  rather than blaming `PATH` (`db_init::spawn_failure`) — one `NotFound`, two
  unrelated causes. `OVERVIEW.md` §§ *Whose commit is it?* / *`db-migrate`
  verifies before it commits*.
- **One storage seam** (`STORY-204`/`STORY-207`/`STORY-208`): all domain
  reads/writes go through the `DomainStore` trait; the Dolt backend — the
  only backend since the STORY-208 cutover — spawns `dolt sql -r json` per
  query (`ADR-203`, no daemon). Multi-hop traversal is a single recursive
  CTE (`DomainStore::reachable`; retired ADR-31's app-side per-hop walk),
  and the selection weight is the numeric `weight` column (retired ADR-22's
  in-app `weight:N` tags). A second backend inherits one non-obvious
  obligation — the **absent-node invariant** (`STORY-327`): `fetch_node` must
  report "no such node" as `QuizdomError::NotFound`, because the lenient
  `fetch_nodes_present` keys its skip off exactly that variant. Build the error
  with `store::missing_node` and call `store::assert_absence_contract` from the
  backend's tests; the rule and its rationale are in
  `docs/architecture/graph-schema.md` § *The absent-node invariant*.
- **Interface: CLI/TUI** (`ADR-4`); web deferred.

## Development

```bash
cargo test                 # workspace tests
cargo run -p quizdom       # run the CLI session loop (reads the Dolt domain graph)
cargo build                # build
cargo run -p quizdom -- db-init     # bootstrap the Dolt repo (data/dolt)
cargo run -p quizdom -- db-migrate  # import a legacy AIDA-store domain graph
cargo run -p quizdom -- db-backup   # snapshot + push data/dolt to its file remote
cargo run -p quizdom -- db-restore  # clone it back after a disk loss
cargo run -p quizdom -- logs        # read the diagnostic log (--tail N)
```

CI installs a pinned dolt and runs the `real_dolt` acceptance tests
(`cargo test --workspace real_dolt -- --ignored`), so the storage layer is
verified in the pipeline, not only on a developer's machine (STORY-261).
The domain graph's durability path — a file-based Dolt remote defaulting to
`~/.local/share/quizdom/dolt-backup`, with the recovery steps spelled out — is
documented in `OVERVIEW.md` § *Durability and recovery*. Backups stay
**explicit** (`STORY-299`): a session that wrote to the graph and sits ahead of
its backup ends with a one-line reminder naming the command, `auto_backup =
true` in `settings.toml` opts into the push instead (off by default, degrades
to the reminder on failure), and cron covers the machine. `STORY-326` finished
that surface: a probe that cannot answer no
longer cancels an opted-in *push* (it pushes and says why — a redundant push is
cheaper than a skipped one), and the probe reads the **configured** remote
(`$QUIZDOM_BACKUP_REMOTE` > `backup_remote` > `backup`) so it and `db-backup
--remote` cannot disagree. `STORY-342` closed the other half: a blind probe with
`auto_backup` **off** no longer stays silent — silence is what a backed-up graph
looks like, so the default configuration learnt nothing from a failed check. It
now says it could not tell, a weaker claim than the reminder's assertion of
drift. Survivable failures —
a degraded store read, a failed auto-backup — go to the append-only diagnostic
log (`$QUIZDOM_LOG_PATH` > `log_path` > `~/.local/share/quizdom/quizdom.log`),
never to the terminal: `crates/quizdom/src/diagnostics.rs` is the one seam, and
the TUI owns the alternate screen. **`quizdom logs [--tail N]`** is the reader
(`STORY-342`); it names the resolved path above what it prints, and lives in
`logs.rs` rather than the seam — *diagnostics writes and never prints, logs
prints and never writes*. `STORY-352` made the reader **honest about why it has
nothing to show**: only `ErrorKind::NotFound` is absence (a message, exit 0), and
a permissions problem, a directory in the way, or a non-UTF-8 file now exit
non-zero naming the OS's cause instead of all claiming "no such file"; a
`--path` that found nothing also names the resolved log, since a typo's "no such
file" says nothing about which of two candidate paths was wrong. Same story
recorded text: `diagnostics::one_line` collapses each event to one line of
**printable** text on the way in — escape sequences dropped whole, other control
characters to spaces — because entries quote subprocess output, and a bare `\r`
lets one entry hide the one before it in the very command run to diagnose the
problem; `logs.rs` re-applies it on the way out, since `--path` reads files this
crate never wrote. The log is bounded (1 MiB, then one kept generation in
`quizdom.log.1` — `diagnostics::ROTATED_SUFFIX` is the one definition, shared
with the reader that points at it, `STORY-352`), and rotation is **safe under
concurrency** (`STORY-342`):
every append takes an exclusive `File::lock` and rotation copies-then-truncates
in place instead of renaming, so two quizdom processes cannot clobber the
rotated history between them. `/settings` shows `auto_backup` and the resolved
log path as read-only rows beside `dolt_path`. **`settings.toml` changes only
when the user asks it to** (`STORY-367`): a session can run a mode the file does
not name (`--mode debate` over a saved `mode = "socratic"`), so each front-end
keeps a *display* copy the engine mirrors its live `score`/`mode` into
(`FrontEnd::mirror_live`, never writes) beside the *persisted* copy that a save
writes, and an explicit change crosses between them one key at a time
(`Settings::adopt`). Before this, `/settings` pushed the live mode across as a
persisting call, a bare `/mode` wrote back the answer to its own question, and
any explicit change saved the mirrored struct whole — three roads to the same
clobber `TASK-266`/`TASK-300` fixed at the seed. The mode precedence (`--mode` >
resumed log > `settings.toml` > Socratic) is also resolved **once**, into
`config.mode`, so the resume auto-continue and the loop cannot frame two
different modes. `OVERVIEW.md` § *Settings, and how a relative path resolves*. Exercising `db-backup`
by hand: a `--path` away from the resolved default now REQUIRES `--to`, so a
scratch run cannot claim the real backup directory (`STORY-292`), and
`db-backup --force` is the executable way past a backup directory already held
by another lineage — it moves the foreign copy to `<backup>.foreign-lineage`,
never deletes it.

Clippy **gates** CI (`-D warnings`) as of STORY-260 — the lint backlog is at
zero, so a new warning is a new regression, not a known debt. Run
`cargo clippy --workspace --all-targets -- -D warnings` before pushing.

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
  A spec is `Completed` only when its PR **merges**. Domain seed data (e.g.
  seed clusters) lands in the local Dolt repo (`data/dolt`, gitignored) via
  `quizdom question add` / `quizdom db-migrate` — the Dolt *store* commits
  every write it makes into Dolt's own history (STORY-208; `aida push
  --store-only` no longer carries domain data), as do `db-init`'s DDL and
  `db-migrate`'s bulk import (STORY-291) — so `db-backup`'s pre-push snapshot
  is now a backstop for hand-run `dolt sql`, not the thing that rescues a
  migration. AIDA-store pushes remain for project intent only.
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

