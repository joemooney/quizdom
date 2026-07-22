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

- **Data substrate → Dolt (ADR-201, superseding ADR-3).** quizdom's domain
  data — questions, term definitions, and belief propositions joined by typed
  edges — lives in a local [Dolt](https://www.dolthub.com/) repo
  (`data/dolt`; `quizdom db-init` bootstraps it), with multi-hop traversal as
  a recursive CTE and selection weight as a numeric column. The AIDA store
  remains canonical for *project intent* (specs, decisions, `references`
  edges). *Historical: ADR-3 originally put the domain graph in the AIDA
  store to dogfood it as a general substrate (VIS-2); EPIC-202 migrated it
  out after the dogfooding surfaced real substrate gaps — which was itself
  the point.*
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

## Settings, and how a relative path resolves

<!-- trace:STORY-290 | ai:claude -->

Runtime preferences live in `~/.config/quizdom/settings.toml` (or
`$XDG_CONFIG_HOME/quizdom/`) — a small flat `key = value` table quizdom
hand-parses. Four keys are the `/settings` surface (`editor`, `mouse`, `score`,
`mode`) and quizdom rewrites those in place; every other line — comments, blanks,
and foreign keys like `dolt_path`, `dolt_backup_path`, `backup_remote`,
`auto_backup` and `log_path` — is preserved verbatim on save. Unknown keys are
ignored on load, so an older quizdom never chokes on a newer file.

<!-- trace:TASK-320 | ai:claude -->

`/settings` also *shows* three resolved values as read-only rows, because a
value with no surface has no discoverability and each of these is consequential:

| Row | Key | Why it is shown |
|-----|-----|-----------------|
| `Domain graph:` | `dolt_path` | decides which graph the session reads |
| `Auto-backup:` | `auto_backup` | a durability control — believing it is on and believing it is off look identical until the disk dies |
| `Diagnostics:` | `log_path` | the answer to "something degraded, where do I look?" |

None is togglable there (no cursor stop, no cycle) — the settings file and the
environment variables stay the way to change them, and the panel says so.

<!-- trace:TASK-300 | ai:claude -->

Two of the four `/settings` keys name state the **engine** owns rather than the
front-end — `score` (the distance-to-goal gauge) and `mode` (Socratic / Debate)
— and the engine **seeds** both from the loaded file at session start
(`FrontEnd::persisted_settings`). It has to: a value the engine does not seed is
a value the engine's own default overwrites the first time `/settings` pushes
the live state back across the seam and saves it, so the setting is ignored on
load *and* destroyed on the next write. The mode's full precedence, highest
first: `--mode` > the mode a resumed session logged > `settings.toml` > Socratic.
A resumed debate stays a debate even for a user whose saved default is Socratic.

Values parse the way TOML would: double **and** single quotes come off, and an
inline `# comment` ends the value. `dolt_path = "/mnt/data/dolt"  # the big disk`
means `/mnt/data/dolt`.

<!-- trace:TASK-307 | ai:claude -->

Two more TOML rules, because the file is one a human hand-edits (STORY-350). A
**double**-quoted value is a TOML *basic* string, so its backslash escapes are
processed (`\t`, `\n`, `\\`, `\"`, `\uXXXX`) and the closing quote is the first
*unescaped* one; a **single**-quoted value is a *literal* string taken verbatim,
which is the spelling for a path full of backslashes (`dolt_path =
'C:\graphs\main'`). An escape quizdom does not recognise is kept as written
rather than dropped. And **a leading `~` expands to `$HOME`**, in every written
tier — `dolt_path = "~/graphs/main"` is the value a user actually reaches for,
and it used to name a *literal* `~` directory anchored under
`~/.config/quizdom/`. Only a leading `~` alone or before a `/` expands;
`~alice/x` needs a password database quizdom does not read, so it stays
recognisable rather than half-translated.

**The anchoring rule: a relative path in `settings.toml` resolves against the
directory `settings.toml` lives in** — not against the process's current
directory. `dolt_path = "graphs/main"` is
`~/.config/quizdom/graphs/main` from every shell and every checkout.

That asymmetry is deliberate. The settings file is *per-user and global*: one
file shared by every worktree. Resolving its relative paths against the cwd
meant one config line selected a different domain graph from each sibling
checkout, silently. Anchoring to "the project root" would have reproduced the
bug, because each worktree has its own. The settings file's own directory is the
only base as global as the file.

The other two tiers stay cwd-relative on purpose, because both are named
per-invocation by someone who can see their own cwd:

| Tier | Example | Relative to |
|------|---------|-------------|
| CLI flag / env var | `--path`, `$QUIZDOM_DOLT_PATH`, `$QUIZDOM_DOLT_BACKUP_PATH`, `$QUIZDOM_LOG_PATH` | the process cwd |
| `settings.toml` key | `dolt_path`, `dolt_backup_path`, `log_path` | the settings file's directory |
| Compiled default | `data/dolt` | the process cwd (deliberately per-checkout — it is the gitignored local graph each worktree gets for free) |

The two non-path keys take the same env > file > default shape:
`auto_backup` (`$QUIZDOM_AUTO_BACKUP`, default **off**) opts a writing session
into pushing to the backup remote on its way out, and `backup_remote`
(`$QUIZDOM_BACKUP_REMOTE`, default `backup`) names the Dolt remote pointed at
the backup directory — see *Durability and recovery* below.

## Durability and recovery

<!-- trace:STORY-261 | ai:claude -->

The domain graph lives only in the local Dolt repo (`data/dolt`, gitignored),
so it needs its own backup — the AIDA store stopped carrying domain data at
STORY-209. The mechanism is a **file-based Dolt remote**: no hosted account, no
credentials, no network. Point it at a removable disk or a synced folder and
the same command covers off-machine.

```bash
cargo run -p quizdom -- db-backup      # snapshot + push to the backup remote
```

`db-backup` commits anything sitting in the working set first (a push carries
committed data only), points the `backup` remote at the backup directory, and
pushes `main`. Every quizdom writer commits its own writes — the store per write
(STORY-208), `db-init` its schema and `db-migrate` its import (STORY-291) — so
that first step normally finds nothing to do; what it catches is a change made
by hand with `dolt sql` in the repo.

The backup directory resolves as `$QUIZDOM_DOLT_BACKUP_PATH` > `dolt_backup_path`
in `~/.config/quizdom/settings.toml` > `~/.local/share/quizdom/dolt-backup` —
deliberately outside the project tree, so `rm -rf data/` cannot take the backup
with it.

<!-- trace:STORY-299 | ai:claude -->

### Explicit backups, and the three ways not to forget one

**A backup is an explicit act, and that is the decision** (STORY-299, closing
TASK-273). Pushing implicitly at the end of every writing session would spend
seconds of `dolt` spawns on a directory that may be an unmounted removable disk
or a synced folder, and would fail in ways that muddy the end of a session.
`quizdom db-backup` stays the primitive.

What "explicit" must not come to mean is "forgotten" — a graph drifting further
from its backup every session with nothing anywhere saying so. Three ways to
close that, in increasing order of automation:

1. **The reminder (always on).** A session that *wrote to the graph* and leaves
   the working copy ahead of its backup ends with one extra line naming the
   exact command:

   ```
   Domain graph has changes not in its backup — run `quizdom db-backup` to push them to /home/you/.local/share/quizdom/dolt-backup.
   ```

   Both halves have to hold, so the line stays feedback on what you just did
   rather than ambient nagging: a session that only read says nothing, and a
   session whose writes are already backed up says nothing. The check is local
   and read-only — it compares `main` against the backup remote-tracking ref
   (plus `dolt_status` for a hand-edited working set), so it never blocks on a
   backup directory that is not mounted, and a probe that cannot answer stays
   silent rather than guessing.

   <!-- trace:TASK-324 | ai:claude -->

   The probe reads the tracking ref for the **configured** remote — the same
   `$QUIZDOM_BACKUP_REMOTE` > `backup_remote` > `backup` chain `db-backup`'s
   `--remote` sits on top of. Probe and push naming different remotes is how the
   reminder came to fire seconds after a successful `db-backup --remote archive`:
   `remotes/backup/main` never existed, a missing tracking ref reads as "you have
   never backed this graph up", and a reminder that is always wrong trains you to
   ignore the one that isn't.

2. **`auto_backup` (opt-in, off by default).** One line in
   `~/.config/quizdom/settings.toml` performs the push instead of printing the
   reminder:

   ```toml
   auto_backup = true    # push to the backup remote when a writing session ends
   ```

   `QUIZDOM_AUTO_BACKUP=1` does the same for one shell. A failed auto-backup
   never takes the session down — it degrades to the reminder, so you still
   learn the graph is unbacked-up and still get the command, with dolt's
   complaint in the diagnostic log.

   <!-- trace:TASK-325 | ai:claude -->

   **A probe that cannot answer does not cancel the push.** Reading a failed
   probe as "nothing to do" turned an `auto_backup` you explicitly opted into
   into a no-op — the exact failure `auto_backup` exists to prevent. A redundant
   push costs seconds; a skipped one costs the graph. So an opted-in session
   pushes anyway and says why:

   ```
   Could not tell whether the domain graph was backed up, so pushed it to /home/you/.local/share/quizdom/dolt-backup anyway.
   ```

   <!-- trace:TASK-328 | ai:claude -->

   **…and it does not cancel the notice either.** The other half of the
   blind-probe rule stayed silent for a while, on the reasoning that a failed
   probe is not evidence of drift and nagging on one costs your trust in every
   later reminder. The first clause is right; silence was the wrong conclusion
   from it. *Nothing* is what a backed-up graph looks like, so the default
   configuration — `auto_backup` off — learnt nothing at all from a probe that
   had failed. It now says exactly what it knows:

   ```
   Could not tell whether the domain graph is backed up — run `quizdom db-backup` to be sure it reaches /home/you/.local/share/quizdom/dolt-backup, and `quizdom logs` for why the check failed.
   ```

   That is a weaker claim than the reminder's assertion of drift, so it cannot
   be wrong in the way that would spend the reminder's credibility — and the
   line only appears at all when *this* session wrote to the graph, so it is
   feedback on what you just did rather than ambient nagging. Both branches also
   record the blind probe in the diagnostic log, so neither is a path through the
   end-of-session decision that leaves no trace.

3. **Cron / a systemd timer**, which covers the machine rather than the session
   — including the hand-run `dolt sql` no session end will ever see:

   ```cron
   0 * * * * cd /path/to/quizdom && ./target/release/quizdom db-backup
   ```

### The diagnostic log

<!-- trace:STORY-299 | ai:claude -->

The TUI owns the terminal (crossterm's alternate screen + raw mode), so a
diagnostic printed to stdout lands inside a frame ratatui is about to redraw and
one printed to stderr is invisible at best. Failures that are *survivable* —
a store read that degraded to "no definitions", an auto-backup that could not
push — therefore go to an append-only file instead:

```
$QUIZDOM_LOG_PATH > log_path in ~/.config/quizdom/settings.toml > ~/.local/share/quizdom/quizdom.log
```

Same resolution chain as every other quizdom path, including the anchoring rule
in *Settings* above; `/settings` shows the resolved path as its `Diagnostics:`
row. It is a breadcrumb trail, not a logging framework: no levels, no filters,
one line per event. Nothing in the seam ever writes to the terminal, and a log
that cannot be written is dropped silently — a breadcrumb that takes down the
session it was meant to explain is worse than none.

<!-- trace:TASK-331 | ai:claude -->

**Reading it: `quizdom logs`.** A three-tier path is a three-tier guessing game
for anyone trying to `cat` the file, so the reader names the resolved path above
whatever it prints, and `--tail N` cuts it to the last N entries — the shape you
want right after a session said something went wrong. A missing log is the
healthy case and reads as a plain message with an exit code of 0, not an error.
The reader lives outside the write seam on purpose (`logs.rs`, not
`diagnostics.rs`): *diagnostics writes and never prints; logs prints and never
writes*, so the seam's never-touch-the-terminal scan below stays a true
statement about every path through it.

<!-- trace:TASK-321 | ai:claude -->

**Bounded at 1 MiB.** Append-only is not the same as unbounded. A healthy
install writes nothing at all, but a persistently broken store writes a line per
probed read per turn — unbounded growth in exactly the situation you are least
likely to be watching. At the limit the contents move to `quizdom.log.1` and the
live file restarts with a line saying so, capping the pair at ~2 MiB. One
generation: the previous entries are kept rather than truncated, because the run
of entries explaining the breakage is the reason the file exists. `quizdom logs`
points at the kept generation when there is one, so a `--tail` cannot be mistaken
for the whole story.

<!-- trace:TASK-333 | ai:claude -->

**Safe when two quizdoms are running.** A TUI session in one terminal and a
`db-backup` from cron in another share the log, so rotation is concurrent — and
`stat`-then-`rename` is a time-of-check/time-of-use race. Both processes see an
over-sized file; the first renames it to `quizdom.log.1`; the second renames the
near-empty file that replaced it *over* that rotated generation, and the
megabyte of history explaining the breakage is gone, in exactly the pathological
case rotation exists to survive. Two changes close it, and they are one
mechanism: every append takes an **exclusive advisory lock** on the log
(`File::lock`, std — no dependency), making the size check, the rotation, and
the write one critical section across processes; and rotation **copies then
truncates in place** rather than renaming, so the live log keeps its identity
and a writer holding it open across a rotation keeps writing to the live file
instead of silently into the dead generation. The copy is staged and renamed
into place before the truncate, so `quizdom.log.1` is never observable
half-written and a rotation that fails leaves the log whole. A lock that cannot
be taken (a filesystem without locking) degrades to the unlocked write rather
than dropping the breadcrumb.

<!-- trace:TASK-322 | ai:claude -->

Two tests pin the never-touch-the-terminal invariant, and they catch different
things. One scans the seam's own source for print macros: lexical, but it covers
every path through the module including ones no test exercises — a print on a
rare error branch is precisely what would corrupt a frame in the field. The
other re-runs the test binary for a single test with `--nocapture`, enters the
alternate screen for real, drives the seam (including a write that fails), and
asserts the bytes between crossterm's enter and leave sequences are zero. The
child process is what makes that assertable: capturing this process's own
descriptors would also catch libtest's progress lines from every concurrent
test.

**Recovery — from a deleted `data/dolt`:**

```bash
# 1. Confirm the backup is there (a Dolt remote directory, not a repo).
ls ~/.local/share/quizdom/dolt-backup

# 2. Restore. --path defaults to the same chain the app reads
#    ($QUIZDOM_DOLT_PATH > dolt_path > data/dolt), so a bare run puts the
#    graph back exactly where the session loop looks for it.
cargo run -p quizdom -- db-restore

# 3. Verify the graph came back whole.
cd data/dolt && dolt sql -q 'SELECT COUNT(*) FROM nodes; SELECT COUNT(*) FROM edges'
# -> 75 nodes / 75 edges for the current seed + session-grown graph
```

`db-restore` refuses to touch an existing repo — recovery must never be the
command that destroys the copy you still had. To restore beside a live repo,
pass `--path /tmp/graph-check` and compare.

The round trip (seed → backup → delete the repo → restore → count rows) is
`real_dolt_backup_restore_round_trip`, which runs in CI alongside the other
`real_dolt` acceptance tests now that the pipeline installs dolt.

<!-- trace:BUG-277 | ai:claude -->
<!-- trace:STORY-292 | ai:claude -->

**A backup directory belongs to exactly one graph.** A file remote is just a
directory, and it cannot tell which repo is entitled to it. Push two repos with
unrelated roots at the same directory and dolt refuses the second — there is no
common ancestor to reconcile them against. `db-backup` detects that refusal and
names the ways out, rather than forwarding dolt's `unknown push error; no common
ancestor`:

```bash
cargo run -p quizdom -- db-backup --to <fresh-empty-directory>   # back up elsewhere
cargo run -p quizdom -- db-restore --path /tmp/check --from <backup>  # look first
cargo run -p quizdom -- db-backup --force                        # take the directory
```

`--force` retires the lineage already in the backup directory to
`<backup>.foreign-lineage` and pushes. It **moves; it never deletes** — the
displaced copy stays recoverable, and a second `--force` lands on
`.foreign-lineage.2` rather than on top of the first. Nothing on this path can
lose a backup.

The way a directory gets claimed by the wrong graph in practice is a
**verification run with no `--to`**: it resolves the default backup path and
hands your real backup directory to a throwaway fixture. Three guards, in the
order they fire:

- `db-backup` **refuses** a `--path` the settings chain did not choose unless
  `--to` is given too. That mismatch — a scratch repo aimed at the real backup
  directory — is the vector exactly, and it is caught before the push that would
  claim the directory. Backing up the resolved default still needs no flags:
  `quizdom db-backup`.
- Inside the test suite the pinning is enforced rather than advised: a
  `#[cfg(test)]` tripwire panics if any test aims `db-init`, `db-migrate`,
  `db-backup`, `db-restore` or the domain store at a path outside the system
  temp directory, and the store's config-resolved constructor — the one with no
  `--path` to pin — is redirected there outright. No test can reach the real
  graph or its backup by any route.
- A guard test fails the build if `crates/quizdom/tests/`, `benches/` or
  `examples/` ever appears, because those targets link the lib *without*
  `cfg(test)` and would compile the tripwire down to a no-op.

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
  `BELIEF-28/29`) with custom edges — originally AIDA objects, migrated to
  Dolt in EPIC-202.
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

- **EPIC-202 (Dolt migration) — complete.** The domain graph moved from the
  AIDA store to a local Dolt repo (ADR-201): schema + `db-init`
  (STORY-205), the `db-migrate` exporter with parity checks (STORY-206),
  a Dolt-backed `DomainStore` with recursive-CTE traversal (STORY-207),
  runtime cutover retiring the ADR-31 BFS and ADR-22 weight tags
  (STORY-208), and post-cutover removal of the store-side domain objects
  (STORY-209).

**Every epic is complete** (~521 quizdom + 7 llm tests, CI green, runs on the Max
plan by default). The product is the full vision plus the use-driven extensions;
further work is driven entirely by real use.

Substrate gaps surfaced by dogfooding (VIS-2) are filed as findings or upstream
`~/ai/aida` issues (FR-282 custom-edge traversal, BUG-415/417). Cost / scaling
gaps surfaced by real use — the O(turns²) full-history re-send growth flagged in
ADR-187 — are likewise tracked as findings.
