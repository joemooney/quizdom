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

## 2026-07-22 — BUG-277: a backup directory belongs to exactly one lineage

**Request.** `/aida-pickup BUG-277`, under a headless single-spec drain (`aida
queue work BUG-277 --auto-complete --no-human=both`).

**The bug, as found.** Advisor verification of STORY-261 on main hit `error:
dolt push backup main failed: unknown push error; no common ancestor`. Root
cause was not in the push: `~/.local/share/quizdom/dolt-backup` already held a
*different* repo — a two-commit throwaway, where the live `data/dolt` has 195
commits. A verification run during the STORY-261 drive had executed `db-backup`
with no `--to`, so it resolved the DEFAULT backup path and claimed the user's
real backup location. Two unrelated roots, so dolt correctly refused every
later push. The round-trip test stayed green throughout — it pins its own
fixture and therefore never observes a pre-existing foreign remote. The advisor
preserved the foreign copy as `dolt-backup.foreign-lineage-20260722` (moved,
not deleted) and verified the full recovery path by hand before filing.

**Defect 1 — tests could reach the real paths.** `db_backup` / `db_restore`
now call a `#[cfg(test)]` `guard_test_paths` tripwire that panics unless BOTH
`--path` and the backup directory sit under the system temp directory. A
whitelist, not a blacklist of known-real paths: a test that forgets to pin
cannot reach `data/dolt`, the platform data dir, or anywhere else by any
route. Every test in the crate is an in-crate `#[cfg(test)]` unit test (there
is no `tests/` directory compiling the lib without it), so one guard covers
the whole suite, and it compiles out entirely in a real build — the CLI is
*supposed* to write to the real paths.

**Defect 2 — the failure mode was an opaque engine string.** The push moved
out of the generic `run_dolt` into `push_to_backup`, which classifies the
failure. On an unrelated-history refusal it raises a quizdom-level message that
names the backup directory and the repo, explains that these are two lineages
rather than a diverged branch, states that nothing reached the backup, and
lists three ways out: back up elsewhere (`--to`), clone the foreign copy out to
inspect it, or move it aside under a `.foreign-lineage` suffix — a move, never
a delete, matching what the advisor did by hand. Dolt's own text is kept as a
labelled parenthetical. Every other push failure keeps `run_dolt`'s plain
shape; this is a targeted translation, not a blanket rewrite.

**Detection is grounded, not guessed.** Before matching on anything, the
scenario was reproduced against dolt 2.2.1 with two `dolt init` repos sharing
one file remote: exit 1, stderr exactly `unknown push error; no common
ancestor`. That probe also turned up a second defect — dolt pads stderr with
backspace runs to erase its `- Uploading...` spinner, and `\x08` is not
whitespace, so `str::trim` left a trail of control characters mid-message. A
`clean_dolt_message` helper strips them; it is applied to the other two dolt
error surfaces in the file too.

**Tests.** Three new unit tests (the translation, the plain-shape control case,
and the tripwire itself via `#[should_panic]`) plus
`real_dolt_backup_refuses_a_foreign_lineage_backup`, which replays the exact
BUG-277 sequence against the real engine: bootstrap two repos, let a throwaway
claim the backup directory, then prove the genuine graph's backup is refused
with the actionable message. The mock test pins the translation, the real one
pins the trigger — without it a dolt rewording would silently stop the
translation firing with every unit test green.

**Verified.** 607 quizdom + 7 llm tests green; all 5 `real_dolt` tests pass
against dolt 2.2.1 (~120s); fmt clean; no new clippy warnings (the 6
pre-existing lints in `input.rs` / `session.rs` remain TASK-240's batch). The
end-to-end CLI output was eyeballed against a real pair of repos, which is what
caught both the control-character trail and an inaccurate "exactly as they
were" claim in the draft message (`db-backup` does take a local snapshot commit
before pushing).

**Docs.** A module-level *A backup directory belongs to exactly one lineage*
section in `db_backup.rs`, and a matching note in `OVERVIEW.md` § Durability
and recovery covering the failure, the `--to`-into-scratch rule for manual
verification runs, and the tripwire that enforces it in the suite.

---

## Session: 2026-07-22 — STORY-260 post-cutover hygiene

**Request.** `/aida-pickup STORY-260` under a headless `aida queue work
STORY-260 --auto-complete --no-human=both` drain. The story bundles three
approved follow-ups from EPIC-249: TASK-226 (rename the `AidaCli*` types),
TASK-229 (drop decorative `weight:N` test fixtures), TASK-233 (clear the
clippy backlog).

**TASK-226 — the rename.** Seven types carried an `AidaCli` prefix naming a
backend they no longer use: `AidaCliQuestionBank`, `AidaCliQuestionReweighter`,
`AidaCliGeneratedQuestionPersister`, `AidaCliUserAuthoredQuestionPersister`,
`AidaCliUserSpecificTermPersister`, `AidaCliContradictsEdges`,
`AidaCliContradictionResolutionPersister`. Every one is generic over
`DomainStore` with `DoltDomainStore` as the default, so they were renamed to a
backend-neutral `Store*` prefix (83 references). `AidaIntentStore` keeps its
name — it genuinely shells out to `aida`, and
`StoreContradictionResolutionPersister` still holds one (the decision node and
its `references` edges stay AIDA-canonical intent per ADR-201).

Doc comments were the misleading half of the problem and were corrected
alongside: `aida rel add` / `aida add --prefix Q` / "the AIDA bank" / "without
shelling out to `aida`" all described the pre-STORY-208 write path.

**TASK-229 — decorative fixtures.** Test fixtures passed the weight twice: once
as the real numeric `Question::weight` field and again as a `weight:N` tag no
code has parsed since STORY-208 moved weight to a column. All such tag entries
were stripped, and the two helpers that fabricated them (`question`,
`titled_question`) now build empty / tag-only lists. The `weight:N` occurrences
that *are* load-bearing were deliberately kept: `parse_node_show`'s legacy
exporter tests and `db_migrate`'s fixtures, where the tag is exactly the legacy
input being converted. Two stale doc comments in `persist.rs` (claiming a
"neutral `weight:50`" tag) and two in `strategy.rs` (`weight:0` successors) were
reworded to describe the column.

**TASK-233 — clippy.** Six lints, all pre-existing. Four were mechanical:
three `io::Error::new(io::ErrorKind::Other, e)` → `io::Error::other(e)` and one
`write!(.., "{c}\n")` → `writeln!`. The two `too_many_arguments` hits were on
`SessionLogger::contradiction_resolved` and `::next_question_selected`, whose
parameter lists mirror their JSON event schema field-for-field — and
`session_started`, two methods above them, already carried
`#[allow(clippy::too_many_arguments)]` for the same reason. Matching that
precedent kept all ten writers reading alike; bundling a subset of two of them
into payload structs would have bought nothing at the call sites. The allows
carry a comment saying so.

With the backlog at zero the CI clippy step was switched from informational to
gating (`-D warnings`), which was TASK-233's stated follow-on.

**Verified.** `cargo fmt --all --check` clean; `cargo clippy --workspace
--all-targets -- -D warnings` exits 0; 607 quizdom + 7 llm tests green; all 5
`real_dolt` acceptance tests pass against the local dolt (~60s). Behaviour is
unchanged by construction — the whole story is renames, dead fixture data, and
lint fixes.

## 2026-07-22 — STORY-291: the Dolt commit lifecycle (db-init and db-migrate commit their own writes)

**Request.** `/aida-pickup STORY-291` under a headless single-spec drain
(`aida queue work STORY-291 --auto-complete --no-human=both`). The story bundles
TASK-272 (the substantive one), TASK-270 and TASK-271.

**TASK-272 — the writes now commit.** `db-init` applied the schema DDL and
`db-migrate` bulk-imported the graph with plain `dolt sql -q`, neither followed
by `dolt add -A` + `dolt commit`; only `DoltDomainStore` committed. So a freshly
bootstrapped-and-migrated `data/dolt` had exactly one commit ("Initialize data
repository") with the whole graph untracked in the working set, and the first
`db-backup` pushed an EMPTY history and reported success. STORY-261 papered over
it by snapshotting the working set before pushing; this fixes the root.

Both commands now end with a commit tail, and the three copies of that tail
collapsed into one: `db_init::commit_all` (stage everything, commit, return
whether a commit was actually created) plus `db_init::is_nothing_to_commit` (the
clean-tree predicate). `DoltDomainStore::commit` delegates to it, and
`db_backup::snapshot_working_set` shares the predicate while keeping its own
`clean_dolt_message` error path. Messages: `quizdom db-init: apply domain-graph
schema` and `quizdom db-migrate: import N nodes / M edges`. Idempotency is
preserved by construction — a second run changes nothing, dolt refuses the
commit as "nothing to commit", and that refusal is the no-op path, so no empty
commits accumulate. A commit that fails for a real reason (e.g. unknown author
identity) still surfaces.

`snapshot_working_set` stays, with its rationale rewritten: it is now a backstop
for changes made *outside* quizdom (a hand-run `dolt sql -q` in the repo), not
the thing that rescues a migration.

**TASK-270 — reachable's depth-limit advice.** The error told the user to raise
`cte_max_recursion_depth`, which ADR-203's per-spawn model makes impossible —
a `SET SESSION` in the user's own shell never reaches quizdom's next `dolt sql`
spawn. The message now says the limit is the engine's *default* (quizdom sets
nothing, which also resolves the "1000 hops … aborted after 2001 iterations"
self-contradiction), explains why a shell-set variable cannot reach it, and
names two remedies that work: traverse from a node further down the chain, or
raise the ceiling inside quizdom by sending the `SET` in the same statement
batch as the CTE.

**TASK-271 — the same-second tie-break fixture.** The seven fan nodes were
minted by `create_node`, so the lexical-vs-insertion straddle the test depends
on was an accident of how many questions the test happened to create first
(Q-5..Q-11 straddles; a decade-aligned Q-20..Q-26 would not), and the
`assert_ne!` guard could fire on unrelated fixture drift looking like an
ordering regression. The targets are now written out (`Q-fan-3`, `Q-fan-1`,
`Q-fan-7`, `Q-fan-5`, `Q-fan-2`) and inserted in one statement — the straddle is
structural, and the `Q-fan-*` suffix does not parse as a number so
`create_node`'s max-suffix mint ignores these rows entirely.

**Tests.** New unit tests pin the commit tail on both commands, the
nothing-to-commit no-op, and that a genuine commit failure still errors; the
depth-limit test now also asserts the advice is reachable. Two `real_dolt`
tests carry the acceptance: `real_dolt_migrate_is_idempotent_and_passes_parity`
gained clean-working-set and commit-count assertions across its two runs, and a
new `real_dolt_migrate_commits_its_import_so_the_backup_carries_history` runs
bootstrap → migrate → backup → restore and asserts the CLONE holds every node,
every edge, and the whole history — the regression the acceptance asked for.

**Verified.** `cargo fmt --all --check` clean; `cargo clippy --workspace
--all-targets -- -D warnings` exits 0; 610 quizdom + 7 llm tests green; all 6
`real_dolt` acceptance tests pass against local dolt 2.2.1.

**Docs.** `CLAUDE.md` and `OVERVIEW.md` § *Durability and recovery* no longer
claim db-init / db-migrate leave their writes uncommitted.

## 2026-07-22 — STORY-299: durability ergonomics + an observability seam for the TUI

**Request.** `/aida-pickup STORY-299` under a headless `--auto-complete
--no-human=both` drain. The story takes on the two EPIC-289 items that were
parked because they needed a human design call — TASK-273 (should backups be
automatic after session writes, or stay explicit?) and TASK-257 (where should a
TUI app log, given the alternate screen makes stderr invisible?) — with the
decisions recorded in the spec so they are reviewable rather than implicit.

**TASK-273 — backups stay explicit; the ergonomics close instead.** An implicit
push at the end of every writing session spends seconds of dolt spawns on a
directory that may be an unmounted removable disk, and can fail in ways that
muddy the end of a session, so `quizdom db-backup` remains the primitive. What
that leaves is the real gap — "explicit" quietly coming to mean "forgotten" —
and three things now close it:

* **A reminder that fires on the session that caused the drift.** Both halves
  must hold: this process committed a graph write, *and* the working copy is
  ahead of its backup. A read-only session says nothing even when the graph has
  been unbacked-up for a week; a session whose writes are already backed up says
  nothing either. The write half is a process-wide flag set at
  `DoltDomainStore::commit`, which is the choke point every write already passes
  through — set only when a commit was actually created, so an idempotent
  `ensure_edge` that changed nothing does not trigger it.
* **The ahead-of-backup probe is local and read-only.** One `dolt sql` comparing
  `dolt_branches`' `main` hash against the `remotes/backup/main` tracking ref,
  plus `dolt_status` for a hand-edited working set that `snapshot_working_set`
  would carry. It never touches the backup directory, so a session end cannot
  block on an unmounted disk. A missing tracking ref reads as *ahead* ("you have
  never backed this up" is the case the reminder exists for); a probe that
  cannot answer at all reads as *unknown* and stays silent, because nagging on a
  broken probe trains users to ignore the line.
* **`auto_backup` (off by default, `$QUIZDOM_AUTO_BACKUP` > settings.toml).** On,
  it pushes instead of reminding. A failed auto-backup never takes the session
  down — it degrades to the reminder, with dolt's complaint in the diagnostic
  log. The cron recipe is documented next to the setting.

**TASK-257 — a file-backed diagnostic seam.** `crates/quizdom/src/diagnostics.rs`
is one append-only file, one line per event, no levels and no dependency. It
takes no writer and returns nothing, so no caller can aim it at the terminal the
TUI owns, and a write failure is dropped rather than reported. Path resolution
matches every other quizdom path (`$QUIZDOM_LOG_PATH` > `log_path` >
`~/.local/share/quizdom/quizdom.log`), including TASK-263's anchoring. Under
`cfg(test)` the sink is a thread-local buffer (the TASK-266 pattern) so the
in-crate tests never append to the developer's real log and parallel tests
cannot see each other's entries.

`load_probed_terms`' two `unwrap_or_default()`s now route through it. The
degrade itself was always right — the session must not die because one read
failed — but it rendered identically to a question that genuinely probes no
terms, so a store failure looked like an empty graph. The log now says which
read, on which question, failed how.

**Tests.** 18 new: the position probe's five cases (up to date / a commit / a
dirty tree at an unchanged hash / never pushed / unanswerable) and that the
remote name is SQL-quoted; the footer decision under every combination of
position and `auto_backup`, including the failed-push degrade; `auto_backup`'s
tier resolution, where an unparseable value falls through rather than reading as
`false`; the log path's default and its anchoring; the seam's
never-touches-the-terminal invariant, asserted against its own source so it
covers paths no test exercises; and a forced store failure in a probes read
leaving a breadcrumb. A new `real_dolt` acceptance test pins the two dolt system
tables the whole reminder rests on, so a release that renames a column cannot
turn the reminder into permanent silence with the unit tests green.

**Verified.** `cargo fmt --all --check` clean; `cargo clippy --workspace
--all-targets -- -D warnings` exits 0; 664 quizdom + 7 llm tests green; all 8
`real_dolt` acceptance tests pass against local dolt 2.2.1. Also smoke-run
against the release binary in a sandboxed `XDG_*`: a writing session prints the
reminder naming the exact command and destination; a read-only session while
ahead prints nothing; a backed-up graph prints nothing; `auto_backup = true`
pushes and reports it in one line with the dolt narration in the log; a broken
backup destination degrades to the reminder with the session still exiting 0;
and renaming the `edges` table out from under a session put the real cause in
the log while the terminal stayed uncorrupted.

**Docs.** `OVERVIEW.md` gains § *Explicit backups, and the three ways not to
forget one* and § *The diagnostic log*, and its settings table lists the two new
keys; `CLAUDE.md` summarizes both.

## 2026-07-22 — STORY-326: finishing the durability + logging surface

**Request.** `/aida-pickup STORY-326` under a headless `aida queue work
STORY-326 --auto-complete --no-human=both` drain. Third-generation follow-ups on
what STORY-299 shipped — five TASKs, all in `db_backup.rs`, the logging seam and
`settings.rs`. Two of them (TASK-325, TASK-322) were filed *precisely* because
the earlier implementer wanted the trade-off visible rather than assumed, so the
work here is mostly deciding, then pinning the decision with a test.

**TASK-325 — a blind probe no longer cancels an opted-in push (the decision).**
`durability_footer` matched `UpToDate | Unknown => None` *before* consulting
`auto_backup`, so a probe that could not answer silently skipped a push the user
explicitly asked for: no push, no reminder, no log line — the only path through
the function that left no trace, in the module built to leave traces. The two
halves of the rule now differ because their costs differ. For the **reminder**,
silence stays right: a failed probe is not evidence of drift, and nagging on a
broken probe costs trust in every later reminder. For the **push**, silence was
wrong: a redundant push costs seconds against a directory the user already
nominated, a skipped one costs the graph, and setting `auto_backup = true` is
someone saying which they would rather have. So `Unknown` + auto-backup on now
pushes and says why (*"Could not tell whether the domain graph was backed up, so
pushed it to … anyway."*), degrading to the usual reminder if the push fails;
`Unknown` + auto-backup off stays silent but records the blind probe.

**TASK-324 — the probe and the push now name the same remote.** The end-of-
session probe hardcoded `BACKUP_REMOTE_NAME` while `db-backup` accepted
`--remote <name>`, so an operator working under another name never populated
`remotes/backup/main`; a missing tracking ref deliberately reads as *"you have
never backed this graph up"*, and the reminder then fired after every writing
session — including seconds after a successful `db-backup --remote archive`. A
reminder that is always wrong is exactly what the Unknown-stays-silent rule was
written to avoid. Added `settings::resolve_backup_remote` in the shape every
other quizdom value already has (`$QUIZDOM_BACKUP_REMOTE` > `backup_remote` >
`backup`), made it the parsed default so `--remote` still sits on top, and
resolved it **once** in `session_end_durability` for both the probe and the push.
Chose the resolution chain over "read the repo's remote list and find whichever
points at the backup directory" because one rule for every quizdom value beats a
second, cleverer rule for this one.

**TASK-321 — the diagnostic log is bounded.** A healthy install writes nothing at
all, but a persistently broken store (a graph missing its `edges` table) writes a
line per probed read per turn, unbounded, in the situation the user is least
likely to be watching. At 1 MiB `append_entry` renames the file to
`quizdom.log.1` and opens a fresh one with a line saying so. Rotation rather than
truncate-on-open, which TASK-321 also offered: truncating discards the run of
entries that *explains* the breakage that filled the file, which is the only
reason the file exists. One generation, one `rename`, no rotation state that can
be wrong; worst case is ~2 MiB.

**TASK-322 — terminal safety is now proven behaviourally, not just lexically.**
The source scan stays (it covers every path through the module, including
branches no test exercises — a print on a rare error branch is what would corrupt
a frame in the field). Beside it, a real capture: a child process re-runs the
test binary for one `#[ignore]`d test with `--test-threads=1 --nocapture`, enters
the alternate screen, drives `record` / `degraded_read` / a successful file write
/ a write that **fails**, and the parent asserts the bytes between crossterm's
enter and leave sequences are zero. The child is what makes the assertion
possible: the `dup2`-this-process approach TASK-322 proposed was implemented
first and failed, because the capture then also contains libtest's `test … ok`
lines from every concurrent test. `--nocapture` is load-bearing — it is what lets
a stray `println!` reach the pipe instead of being swallowed. Verified by
mutation: adding a `println!` to `emit` fails the test. No new dependency (the
`libc` dev-dependency the fd approach needed was dropped with it); `diagnostics.rs`
joins the BUG-200 allowlist, which already models "this file spawns something
that is not `aida`".

**TASK-320 — `auto_backup` and the log path are visible.** STORY-299 added both
keys and gave neither a surface. Generalised TASK-262's single `dolt_path_row`
into `settings::ReadOnlyRows` (resolve once, render everywhere), so the TUI panel
and the headless value list draw the same three rows from one computation and
cannot drift. `Auto-backup:` reads On/Off — a durability control is the worst
category to hide, since believing it is on and believing it is off look identical
until the disk dies — and `Diagnostics:` shows the resolved log path, which
doubles as the answer to "something degraded, where do I look?".

**Tests.** 12 new: the two `Unknown` branches (pushes and reports vs stays silent
and records) plus the failed blind push degrading to the reminder; the probe
reading a non-default remote's tracking ref and the flag-over-chain precedence;
the remote name's tier resolution including blank-value fall-through; rotation at
the bound, the previous generation being kept, a second rotation replacing rather
than accumulating it, and a small log left untouched; the alternate-screen
capture and its child; and the three read-only rows on both surfaces, including
an `Off` auto-backup still getting a row.

**Verified.** `cargo fmt --all` applied; `cargo clippy --workspace --all-targets
-- -D warnings` exits 0; 675 quizdom + 7 llm tests green; all 8 `real_dolt`
acceptance tests pass against local dolt 2.2.1.

**Docs.** `OVERVIEW.md` gains the read-only-rows table in § *Settings*, the
configured-remote and blind-probe rules in § *Durability and recovery*, and the
log bound + the two-test rationale in § *The diagnostic log*; `CLAUDE.md`
summarizes all five.

## 2026-07-22 — STORY-327: contract-message accuracy, an enforced store contract, the last session.rs `too_many_arguments`

**Request.** `/aida-pickup STORY-327` under a headless `aida queue work
STORY-327 --auto-complete --no-human=both` drain. Third-generation follow-ups on
STORY-293, across `signals.rs` / `store.rs` / `session.rs`.

**TASK-319 — a message that named the wrong kind of seam.** The three
batch-contract errors in `apply_log_signals` shared a tail inherited from
`take_by_id`: *"a batch load must return one entry per requested id"*. All three
are raised against **both** seams, and `reweight_questions` is a batch write, so
half the diagnostics described the wrong operation. TASK-319 proposed making the
tail seam-neutral; instead the seam became a value (`BatchSeam { name, verb }`,
with `LOAD_QUESTIONS` / `REWEIGHT_QUESTIONS` constants), so the message names the
seam *and* its direction and cannot disagree with the call that produced it. A
new test asserts all three violations from both seams, including that the write
seam's message never contains "load".

**TASK-318 — a doc-comment promoted to an enforced contract.** STORY-293 made
`fetch_nodes_present` skip only `NotFound` and propagate everything else, which
silently moved an obligation onto backends: report a missing row as `Dolt(...)`
and every absence propagates as a hard failure — the lenient read becomes the
strict one, with no compile error and no failing test. Two things now hold it:
`store::missing_node(id, backend)`, the shared constructor that fixes the variant
by construction (the Dolt backend supplies only the wording), and
`store::check_absence_contract` / `assert_absence_contract`, the conformance
check pinning all three halves at once. The check is non-panicking underneath so
the check *itself* is under test: a backend reporting absence as `Dolt(...)`
fails it, and the report names the variant and why it matters. The Dolt backend
runs it against scripted rows and inside `real_dolt_full_trait_surface` against
a real dolt.

**TASK-315 — the last seven allows, retired with payload structs.** Applied
STORY-293's own `EventScope` move one layer up. Three structs:

- `TurnJournal<'a>` — the `(config, logger, turn)` triple every session-loop
  helper took. Applied to all **eleven** helpers carrying it, not only the five
  over the lint threshold: a payload struct only earns its keep as a rule, and a
  convention half the siblings follow is worse than none.
- `SessionWiring<'a>` — the seven collaborators threaded one-per-parameter
  through `run_session_from_current` (thirteen arguments) and re-listed at six
  call sites. Destructured at the top of the body, so the loop reads exactly as
  before; the bundle is about the call sites. `resume_session_with_term_persister`
  now builds one wiring instead of constructing the same `Store*` defaults twice.
- `SynopsisSource<'a>` — the `(observer, log_path, branch)` triple
  `render_session_synopsis` reads from. Not invented to dodge the lint:
  `ReviewContext` had already bundled exactly these three, and now embeds it.

Zero `#[allow(clippy::too_many_arguments)]` remain in `session.rs`. The two in
`frontend.rs` / `tui.rs` are out of scope per TASK-315's own file set.

**Verified.** `cargo fmt --all` applied; `cargo clippy --workspace --all-targets
-- -D warnings` exits 0; 680 quizdom + 7 llm tests green (5 new); all 8
`real_dolt` acceptance tests pass against local dolt.

**Docs.** `docs/architecture/graph-schema.md` gains § *The absent-node
invariant*; `CLAUDE.md`'s storage-seam bullet names it and points there.

## 2026-07-22 — STORY-342: a log reader, safe rotation, the honest reminder, the last two allows

**Request.** `/aida-pickup STORY-342` — the final consolidation round after the
review generations converged. Four substantive items: TASK-331 (the diagnostic
log has no reader), TASK-333 (rotation is `stat`-then-`rename`, so two processes
can clobber `quizdom.log.1`), TASK-328 (a blind backup probe still says nothing
when `auto_backup` is off), TASK-336 (the last two `too_many_arguments` allows).

**TASK-331 — `quizdom logs [--tail N] [--path <file>]`.** Logging that cannot be
read will not be used, and the path resolves through three tiers, so "just cat
it" needs you to already know which tier won. The reader therefore names the
resolved file above whatever it prints; `--tail N` says `last N of M entries` so
a truncated view cannot be mistaken for the whole log; a missing or empty log is
a plain message and exit 0, because a healthy install genuinely has no file.
When a rotated generation sits beside the log it is pointed at (never inlined) —
a `--tail` never reaches the live log's `-- rotated at … --` first line.

It lives in a new `logs.rs`, not in `diagnostics.rs`, and the split is the
point: *diagnostics writes and never prints; logs prints and never writes*. The
seam's promise is pinned by a scan of its own source for print macros, and
putting the one legitimate terminal write inside it would have made that scan a
statement about a module that does print.

**TASK-333 — rotation safe under concurrency.** `stat`-then-`rename` is a
TOCTOU race across processes: both see an over-sized log, the first renames it
to `quizdom.log.1`, the second renames the near-empty file that replaced it over
that rotated generation, and the megabyte explaining the breakage is gone — in
exactly the pathological case rotation exists to survive. Two changes, one
mechanism:

- Every append takes an exclusive advisory lock on the log (`File::lock`, std
  since 1.89 — no new dependency), so the size check, the rotation and the write
  are one critical section rather than three racing syscalls.
- Rotation **copies then truncates in place** instead of renaming, so the live
  log keeps its identity: a writer holding it open across a rotation keeps
  writing to the live file instead of silently into the dead generation, and the
  lock always guards the same object. The copy is staged and renamed into place
  *before* the truncate, so `quizdom.log.1` is never observable half-written and
  a failed rotation leaves the log whole rather than empty.

A lock that cannot be taken degrades to the unlocked write — the module's
standing call that a breadcrumb which cannot be written is worse than one
written imperfectly.

Three tests pin it, and all three were **verified to fail against the
pre-TASK-333 code** before being kept: the critical section (an append cannot
land while another writer holds the log), the mechanism (a handle opened before
a rotation still writes to the live log, which a `rename` cannot satisfy), and
the loss itself under eight contending writers — with a watcher sampling
`quizdom.log.1` throughout, because checking only at the end misses a clobber
that the next good rotation overwrites. The old code was observed leaving a
321-byte "generation" behind a 4 KiB limit.

**TASK-328 — both halves honest about not knowing.** STORY-326 made a blind
probe push when `auto_backup` is on; the reminder half stayed silent on the
reasoning that a failed probe is not evidence of drift. The first clause is
right and silence was the wrong conclusion from it: *nothing* is what a
backed-up graph looks like, so the default configuration (`auto_backup` off)
learnt nothing at all from a check that had failed. It now says exactly what it
knows — "Could not tell whether the domain graph is backed up … and `quizdom
logs` for why the check failed" — a weaker claim than the reminder's assertion
of drift, so it cannot be wrong in the way that would spend the reminder's
credibility, and it only fires when *this* session wrote to the graph. A new
table-driven test walks all six cells (three positions × two settings); the hole
TASK-328 named was one silent cell indistinguishable from `UpToDate`. Two probe
tests were renamed to describe what they assert (`…leaves_the_position_unknown`)
rather than a silence that is no longer the behaviour.

**TASK-336 — the last two allows.** Both were vestigial: `author_question` takes
six arguments in `frontend.rs` and `tui.rs`, under clippy's threshold of seven,
so removing the attributes needed no refactor. Zero
`#[allow(clippy::too_many_arguments)]` remain in the workspace.

**Verified.** `cargo fmt --all` applied; `cargo clippy --workspace --all-targets
-- -D warnings` exits 0; 692 quizdom + 7 llm tests green (13 new); all 8
`real_dolt` acceptance tests pass against local dolt 2.2.1. `quizdom logs`
smoke-tested end to end for the missing, full, `--tail`, rotated-generation and
bad-argument paths.

**Docs.** `OVERVIEW.md` § *The diagnostic log* gains the reader and the
concurrency rule, and § *Durability* gains the blind-probe notice; `CLAUDE.md`
names `quizdom logs` in the command list and in the durability paragraph; both
`/settings` surfaces (headless list and TUI panel) now name the command beside
the log path they already showed.
