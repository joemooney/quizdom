# PROMPT_HISTORY.md — quizdom session log

Chronological log of working sessions on quizdom. Each entry captures the user's
request, what was built and why, and the git / AIDA operations. Newest entries
at the bottom. For the source-of-truth on requirements see AIDA (`aida list`);
for the human-readable product summary see `OVERVIEW.md`.

---

## 2026-06 — The EPIC-167 TUI build-out (session-mechanics → full ratatui front-end)

**User's drive.** An iterative, idea-at-a-time build-out: the user kept playing
with live sessions, hit friction or saw an opportunity, and turned each one into
an AIDA spec rather than a loose TODO. Two intertwined threads ran through the
batch — (1) richer *session mechanics* for the Socratic debate (goals,
objections, scoring, convergence framing) and (2) a *real front-end* to house
them, since the EPIC-162 crossterm-overlay palette couldn't keep up (it only
opened on Enter and line-stacked). The decision: rebuild the interactive
front-end as a full-screen TUI on a clean engine/front-end seam.

**The architecture (EPIC-167, STORY-168).** Extract a small front-end interface
so the session ENGINE (loop control, strategy, observer/help/tutor,
synopsis/roundedness, goal/mode/closing logic) stays front-end-agnostic. Two
front-ends sit behind it:

- a **headless line front-end** that preserves today's behavior for the existing
  piped/byte tests, non-TTY / scripted use, `--no-tui`, and the standalone
  commands (`contradictions`, `curate`, `synopsis`, `question add`, `session
  list/show`) — nothing in those paths changes; and
- a **ratatui + crossterm TUI** that becomes the default for an interactive TTY
  (`session start/resume/fork`).

This is *why* the split mattered: it let the TUI own the event loop (so `/` pops
a live palette on the keystroke and everything redraws in place) without
disturbing the ~500-test headless contract.

**What landed (STORY-168..194).**

- **Shell + palette** — STORY-169 (alternate screen, layout, event loop, live
  `/` palette), STORY-177 (palette filter modes: leading-`/` = command-name
  prefix vs. substring search across name+description), STORY-190 (context-aware
  greying of inapplicable commands, e.g. `/judge`/`/resolved` only when an
  objection is open).
- **Theme** — STORY-171 (colored borders, gold cursor, per-role colors,
  quote-attribution), BUG-172 (symmetric quote-attribution: interrogator quoting
  the user renders in the user's color).
- **Keyboard + discoverability** — STORY-176 (navigation keys + a cheat-sheet
  generated from one keymap registry), with the F1 help alias folded in via
  STORY-194.
- **Markdown rendering** — STORY-179 + BUG-178 (inline + block markdown in the
  transcript, composed with role color and an always-on quote-yellow).
- **Free-text editor** — STORY-180 (a capable answer editor: readline/Emacs +
  optional Vim via `tui-textarea`, plus an open-in-`$EDITOR` escape), with
  BUG-183 (soft-wrap + dynamic vertical grow so long answers don't overflow a
  fixed 1-row box) and BUG-184 (post-submit "thinking" state + redraw so the TUI
  doesn't freeze on the answer box).
- **Focus + transcript** — STORY-193 (Tab focus model, transcript scrollbar,
  mouse support), STORY-191 (full styled scrollable transcript; hydrate prior
  history on resume as the styled transcript rather than a debug replay, and
  render the visible window efficiently).
- **Settings** — STORY-194 (`/settings` panel + runtime `/editor` toggle
  vim/emacs/auto, persisted to config; unifies the mouse/score/mode toggles).

**Session mechanics that rode along in the same batch.**

- STORY-173 — request a goal when none is set (either party: user-requested
  proposal + interrogator offer).
- STORY-174 — `/score`: toggle a persistent distance-to-goal roundedness gauge in
  the status bar.
- STORY-175 — `/objection`: the court mechanic. Pin the exchange on a contested
  point until `/resolved` (objector) or `/judge` (the other party, which calls
  the Observer to rule) — asymmetric exits, judge ruling.

**The cost fix (ADR-187, STORY-188).** Real use exposed a scaling problem: the
loop spawned a full-history `claude -p` subprocess PER TURN for each of
`propose_goal` and `interrogator_objection` *in addition to* the next-question
call. The one-shot guards gated the OUTCOME, not the CADENCE, so the probes ran
on essentially every turn, each re-sending the whole growing conversation — cost
scaling ~O(turns²), visible in `ps` as a recurring objection probe re-sending
~150 Q&A turns. The decision (user): CONSOLIDATE. The next-question call — which
already sends full history and already reasons about the whole dialogue — now
returns a structured envelope `{ next_question, objection?, goal_offer? }`, so
the objection / goal-offer decisions are a near-free byproduct of one call.
Belief-neutrality is preserved (the envelope fields are prompted as a structural
tension / the question to resolve, never a belief); the one-shot guards now gate
whether we SURFACE the objection/goal-offer, not whether we pay for a probe.
Note: this does NOT by itself fix the deeper O(turns²) full-history re-send
growth — that's tracked separately as a finding. BUG-181 also de-flaked the
score-gauge gate test (~60s) by mocking it off a live LLM.

**How it shipped.** Every story / bug went out the project's standard way:
isolated sibling worktrees + leases per spec (never `git checkout -b` in the
shared main checkout), a branch + PR to `main` per item, CI (fmt + clippy +
test) gating, and CI auto-merge on green. PR numbers #57..#75 map to
STORY-168..194 / the bugs / ADR-187. A spec is `Completed` only once its PR
merges.

**Status at end of batch.** Test suite ~521 quizdom lib + 7 llm, CI green, runs
on the Max plan by default. EPIC-167 landed; further work is driven by real use.

**This entry's own change (docs refresh).** Refreshed `OVERVIEW.md` to current
state — the last update (commit f03e50f) only covered through EPIC-154/158/162 —
adding the EPIC-167 status block, the TUI/turn-envelope architecture notes, the
real test count, and resolving the "still open" LLM-integration item. Created
this `PROMPT_HISTORY.md`. Docs only, no code touched. Shipped via the same
isolated-worktree + PR-to-`main` + CI-auto-merge workflow.

## 2026-07-21 — STORY-244: batching the DomainStore (the ADR-203 spawn ceiling)

**The request.** `/aida-pickup STORY-244` — the implementer seat picking up the
queued story off the branch `story-244`.

**The problem, as measured.** `quizdom curate` against the real 75-node bank
took 2m09s wall for ~12s of CPU, spawning 264 `dolt` processes. Process
startup (~0.37s each), not query time, was the whole cost. STORY-207's
recursive-CTE win on traversal was being swamped by per-query spawns
everywhere else. Root cause was N+1 at the trait seam, not in the backend:
every hot path looped a per-item `DomainStore` method — `all_questions()` did
`list_node_ids` + a `fetch_node` per id, the `begets`/`probes` fan-outs loaded
their successors one at a time, `detect_graph_contradictions` read edges one
belief at a time, and the curate re-weight loop paid an UPDATE + `dolt add` +
`dolt commit` per question (4 spawns × 66 questions = the 264).

**What was done.** Four set-based methods added to `DomainStore` —
`fetch_nodes`, `list_nodes`, `neighbors_many`, `update_weights` — each with a
**default implementation that loops the per-item method it batches**, so every
impl and every test double stayed correct without being touched. The Dolt
backend overrides all four with single-query forms: `IN (...)` selects, one
grouped edge select (`ORDER BY from_id, created_at, to_id`, preserving the
per-source ordering TASK-221 documents), and a multi-row
`UPDATE ... SET tags = CASE id WHEN ... END, weight = CASE id WHEN ... END`
followed by one `add` + one `commit`. All three chunk at `MAX_BATCH_IDS = 500`
so a bank far larger than the current one costs a spawn per chunk, not per row
— and can't blow the argv limit. Node-row decoding was pulled into one
`node_from_row` so the per-item and set-based reads can't drift.

Contract parity was the design constraint, since the point of the defaults is
that the two paths are interchangeable: `fetch_nodes` returns records in
requested-id order and raises the *same* not-found error looping `fetch_node`
raises; `neighbors_many` is total over its inputs; `update_weights` is
last-write-wins per repeated id. The two bank-level batch reads deliberately
differ in strictness, each matching the call site it replaces —
`load_questions` is strict (the `begets` and curate fan-outs were), while
`load_terms` skips a node with no `definition:` line rather than failing the
batch (the `probes` fan-out's long-standing `filter_map(.ok())`).

**Result, measured the same way (PATH shim logging every `dolt` invocation,
against a copy of the real 71-question bank plus a log that re-weights all
71).** Before: 207 spawns and still running when killed at 120s. After: **4
spawns, 1.94s wall.** The batched write was checked for correctness too, not
just speed — all 71 rows moved −12 with `quality:unhelpful` appended, in a
single Dolt commit.

**Tests.** 578 lib tests green (8 new: batch-vs-loop parity for each method,
the spawn counts, empty-batch no-op, repeated-id last-write-wins, and an
end-to-end curate run over a scripted Dolt runner asserting a *fixed* 4 spawns
regardless of question count). The 3 ignored `real_dolt` tests green against a
real `dolt` binary, now asserting batch/per-item agreement on real data. No new
clippy warnings.

**Not done, deliberately.** No `dolt sql-server` — ADR-203 keeps CLI spawns;
this story was about spawn *count*, not the spawn *mechanism*.

## 2026-07-21 — STORY-258: one path-resolution chain, and a settings save that stops eating keys

**Request.** Headless `--auto-complete` drain of STORY-258 (bundling TASK-228,
TASK-218, TASK-222 — the settings/path-resolution lane of EPIC-249).

**The three defects were one defect wearing three hats:** `settings.toml` had
two readers and one writer, and none of them agreed about it.

- `db-init` / `db-migrate` resolved the Dolt repo from `--path` or the compiled
  default only, while the runtime store honored `QUIZDOM_DOLT_PATH` and
  `dolt_path`. `QUIZDOM_DOLT_PATH=/tmp/x quizdom db-init` bootstrapped
  `data/dolt`; the next session read `/tmp/x` and found nothing (TASK-228).
- `settings::save` serialized only its four modelled keys, so the first
  `/settings` toggle of a session silently deleted a hand-added `dolt_path`
  line — STORY-194 promised unknown keys are ignored on *load*, and the save
  path never honored the other half of that bargain (TASK-218).
- `dolt_store::config_value` compared keys case-sensitively and stripped quotes
  with `trim_matches('"')`, where `Settings::from_toml` lowercased keys and
  required a matched pair. `Dolt_Path = ...` was visible to one reader and
  invisible to the other (TASK-222).

**Fix.** `settings.rs` now owns the whole file. One line-parser (`config_entry`
— lowercase key, matched-pair unquote) backs both `from_toml` and
`config_value`, so the two readers cannot diverge on case, quotes, or
repeated-key precedence (both are last-wins). `Settings::to_toml_merged`
rewrites the modelled keys *in place* over the existing text and keeps every
other line verbatim — comments, blanks, foreign keys — so `save` merges instead
of round-tripping. `resolve_dolt_path()` is the single env > settings > default
chain; `domain_store_from_config`, `db-init`, and `db-migrate` all call it, with
`--path` layered on top by each subcommand's arg parser (the parsers take the
resolved default as a *parameter*, which keeps them pure and their tests free of
ambient env). `dolt_store.rs` lost its private copy of both helpers.

**Verified end-to-end, not just in unit tests.** Against the real binary and a
real `dolt`: `QUIZDOM_DOLT_PATH=<p> quizdom db-init` created `<p>`; `--path`
beat the env var; a `dolt_path` in a temp `XDG_CONFIG_HOME` settings file was
honored when neither was set; `db-migrate` named the resolved repo in its
"no Dolt repo at ..." error under both tiers. Then a real headless session
(`/settings set mouse off` against a scratch copy of the domain graph) left the
hand-written comment, `dolt_path`, and `store` lines intact while flipping
`mouse` in place — the exact round-trip TASK-218 filed.

**Tests.** 587 lib tests green (4 new in `settings.rs`: the foreign-key-preserving
save plus its fixed-point second write, the empty-file degrade, reader agreement
on case/quotes/repeats, and the full resolution chain including blank-tier
fall-through; the two subcommand parse tests now pin `--path` over a resolved
default). No new clippy warnings.

## 2026-07-21 — STORY-259: Dolt store hardening (the four dolt_store review findings)

**Request.** `/aida-pickup STORY-259` — the file-disjoint `dolt_store.rs` bundle
from EPIC-249: TASK-248, TASK-221, TASK-223, TASK-224.

**The one with teeth (TASK-248).** `MAX_BATCH_IDS = 500` said in its doc comment
that it "bounds the SQL text handed to a single `dolt` spawn", but it bounds the
id *count* — and for `update_weights` the ids are not the dominant term. Each
`CASE` arm carries a whole `tags` column, and `nodes.tags` is `VARCHAR(2048)`, so
a worst-case 500-row chunk is ~1 MB of SQL handed to `dolt sql -q` as **one argv
element**. Linux caps a single argv element at `MAX_ARG_STRLEN` = 128 KiB — a
separate and far lower limit than the ~2 MB `ARG_MAX` for the whole command line
— and blowing it fails `execve` with `E2BIG`: an opaque spawn error, not a SQL
error. The real 61-question bank is already ~45 KB per statement, so the headroom
was ~3x on payload, not the ~8x the id count implied.

**Fix.** `chunk_by_sql_bytes` splits on accumulated SQL bytes *and* id count,
against `db_migrate`'s existing `SQL_BATCH_BUDGET` (64 KiB) — promoted to
`pub(crate)` so the importer and the runtime store share one budget with one
rationale rather than growing two. `update_weights` now builds each row's `CASE`
arms *before* chunking, so the chunker measures the SQL it will actually spawn
instead of guessing from the id count. An item wider than the whole budget still
ships alone, matching the importer's chunker rather than looping forever.

**The three parity findings.** TASK-221 was a guarantee to document, not
reconcile, now that Dolt is the only backend: `neighbors` returns oldest-first by
`created_at` with ties broken by `to_id` *lexically*, and since `created_at` is a
1-second `TIMESTAMP`, a batch of edges written together is ordered entirely by the
tie-break — `Q-10` before `Q-2`. That is now on the trait and in
`graph-schema.md`. TASK-223's `weight:0` asymmetry died with ADR-22's tag
encoding, so the work was a test that pins the symmetry and fails if tag-encoded
weight ever comes back. TASK-224: `reachable` inherits Dolt's
`cte_max_recursion_depth`, so a >1000-hop chain aborted with a raw engine string;
it now maps to a quizdom error naming the limit and how to raise it, with the
engine text as a trailing detail, while unrelated dolt failures pass through
untouched.

**Tests.** 600 workspace tests green. New: the chunker's two caps (count, bytes,
oversized-alone, empty); a 100-row wide-tags batch proving every spawn stays under
`MAX_ARG_STRLEN` while every row lands exactly once under a *single* add + commit
(chunking is a spawn detail, not extra Dolt history); the same-second tie-break;
the weight round-trip through both read paths; both recursion-depth error paths.
`real_dolt_full_trait_surface` gained same-second-insert coverage — the edges go
in as one statement so they genuinely share a `created_at`, which a loop of
`create_edge` (a dolt commit each) could never stage reliably — plus the weight
symmetry check. All 3 ignored `real_dolt` tests pass locally against dolt 2.2.1.
No new clippy warnings.

**Git.** Committed on `story-259`, pushed, PR #94 open against `main`. The four
tasks and the story stay `in-progress` until it merges.

## 2026-07-22 — STORY-261: durability + CI (dolt in the pipeline, a backup for data/dolt, the stale promotion doc)

**Request.** `/aida-pickup STORY-261` — the EPIC-249 infra/docs bundle:
TASK-243 (a remote or backup for the domain graph, which after STORY-209 exists
only in the local gitignored `data/dolt`), TASK-219 (install dolt in CI so the
`real_dolt` acceptance tests actually run), TASK-241 (rewrite
`session-log-promotion.md` in Dolt terms).

**The durability shape.** A file-based Dolt remote, per TASK-243's own stated
preference — no hosted account, no credentials, no network. New
`crates/quizdom/src/db_backup.rs` adds `quizdom db-backup` (point the `backup`
remote at the backup directory, adding or re-pointing as needed, then push
`main`) and `quizdom db-restore` (clone it back). The backup directory rides the
same env > settings > default chain as `dolt_path`
(`QUIZDOM_DOLT_BACKUP_PATH` > `dolt_backup_path` > `~/.local/share/quizdom/
dolt-backup`), refactored in `settings.rs` as a shared `tiered_path` helper so
the two chains cannot drift. The default sits outside the project tree on
purpose: a backup under `data/` is no backup against `rm -rf data/`.

**Two things the real graph taught us.** First, `dolt clone` enumerates its
working directory as a set of databases before doing anything, so running it
from the target's parent fails outright when a sibling directory disappears
mid-scan — it did, in `/tmp`, against another test's temp dir. The restore now
clones from an empty scratch directory it creates and removes. Second, and more
serious: **the live `data/dolt` had never been committed**. `db-init`'s DDL and
`db-migrate`'s bulk import land in the working set untracked (only the *store*
commits its writes, STORY-208), and a push carries committed data only — so the
first backup of the real graph uploaded an empty history and reported success.
`db-backup` now snapshots the working set (`add -A` + commit, treating "no
changes added to commit" as the ordinary clean-tree case) *before* pushing, and
a failed snapshot never pushes a half-backup.

**Acceptance, verified against the real graph.** `db-backup` on the live repo,
then `db-restore --path /tmp/quizdom-restore-check`: 75 nodes / 75 edges back.
The same round trip is `real_dolt_backup_restore_round_trip`, which seeds rows
*without* committing them, deletes the repo, restores, and counts — so the
uncommitted-working-set trap stays caught.

**CI.** `.github/workflows/ci.yml` installs a pinned dolt (2.2.1, from the
release tarball rather than `latest`, so a dolt release cannot turn CI red on
its own schedule), configures the author identity dolt requires before it will
init or commit, and runs `cargo test --workspace real_dolt -- --ignored` as its
own step. The four acceptance tests take ~60s. The three pre-existing
`real_dolt` doc comments claiming "ignored in CI (no dolt there)" were updated —
they were about to become the stale claim.

**Docs.** `session-log-promotion.md` rewritten in Dolt terms (TASK-241): the
STORY-209 redirect banner is gone, promotion targets are `nodes` / `edges` rows,
`promotion_weight` maps to the numeric `weight` column instead of ADR-22's
retired `weight:N` tag, the promotion rules name the `DomainStore` calls that
perform them, and the worked example is the SQL the store writes rather than an
AIDA YAML object. `OVERVIEW.md` gained a *Durability and recovery* section with
the explicit recovery commands; `CLAUDE.md` gained the two new commands, the CI
note, and a correction to its "commits every write" claim.

**Tests.** 604 quizdom + 7 llm tests green, all 4 `real_dolt` tests pass against
dolt 2.2.1, fmt clean, no new clippy warnings (the 6 pre-existing lints in
`input.rs` / `session.rs` are TASK-240's batch).
