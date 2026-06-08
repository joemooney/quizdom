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
