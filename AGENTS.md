# AGENTS.md

Guidance for Codex and MCP-compatible coding agents working in this
repository. Read this as instructions-to-self: when you implement work,
coordinate through AIDA, keep the git/aida-store state coherent, and
leave durable traces for the next agent.

The block delimited by HTML comment markers below is auto-generated from
`.claude/AIDA.md` on each `aida scaffold apply`. Leave the markers in
place. Content outside the marked block is project-owned guidance.

## Project Orientation

quizdom

Use `OVERVIEW.md` for product/architecture context and
`docs/agents/cross-agent-onboarding.md` for the shared MCP operating
model. Use `docs/agents/codex-mcp-setup.md` when configuring Codex
against AIDA's MCP server. Use `docs/agents/session-communication.md`
for agent pause/abort/defer semantics.

<!-- AIDA-AUTOGEN-BEGIN -->
# AIDA Conventions

This file is the single source of truth for AIDA's coding conventions in
this project. CLAUDE.md imports it via `@.claude/AIDA.md`; AGENTS.md
inlines a copy inside auto-generated delimiters. Edit this file to change
the conventions for both.

## Requirements management

This project tracks requirements with [AIDA](https://github.com/joemooney/aida).
**Do not maintain a separate `REQUIREMENTS.md`** — the requirements DB is
canonical.

Requirements database: distributed git-canonical store at `.aida-store/` (orphan branch `aida-store`, plus a rebuildable SQLite cache at `.aida/cache.db`).

### Daily commands

```bash
aida list                              # list all requirements (cache-backed)
aida list --status draft               # filter by status
aida show <ID>                         # show details (e.g. `aida show FR-0042`)
aida search "<query>"                  # full-text search
aida add --title "..." --type <type> --status draft
aida edit <ID> --status in-progress
aida edit <ID> --status completed
aida comment add <ID> "implementation note..."
aida rel add --from <ID> --to <ID> --type <Parent|Verifies|References>
aida history                           # what was touched recently (digest)
aida statusline                        # one-line: project · role · queue · cache
```

### Requirement-first development

1. **Before coding:** check whether the work has a SPEC-ID. If not, create one
   (`aida add --type <task|story|bug|...> --status approved --title "..."`).
2. **During coding:** add inline trace comments referencing the SPEC-ID.
3. **Before committing:** mark the requirement `completed` (or `in-progress`
   if work continues), and ensure the commit message references it.

## Inline trace comments

Add a comment near the code that implements (or fixes, or verifies) a
requirement:

```rust
// trace:FR-0042 | ai:claude
fn implement_feature() { /* ... */ }
```

Format: `// trace:<SPEC-ID> | ai:<tool>[:<confidence>]`

- `<SPEC-ID>` — e.g. `FR-0042`, `BUG-1-017`, `TASK-0344`
- `<tool>` — `claude`, `codex`, `copilot`, `human`, `aider`, …
- `<confidence>` — optional: `high` (implied), `med` (40-80% AI), `low` (<40% AI)

## Commit message format

```
[AI:tool] type(scope): description (REQ-ID)
```

Examples:

```
[AI:claude] feat(auth): add login validation (FR-0042)
[AI:claude:med] fix(api): handle null response (BUG-0023)
[AI:antigravity+claude] test(hooks): accept mixed authorship (TASK-509)
chore(deps): update dependencies        # no REQ-ID needed
docs: update README                     # no REQ-ID needed
```

Rules:

- `[AI:tool]` required when commit includes AI-assisted code (any file with a
   `// trace:... | ai:tool` comment changed). Use `[AI:tool1+tool2]` for
   mixed-agent authorship, with optional confidence on the whole commit
   (`[AI:tool1+tool2:med]`).
- `type` required: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`,
   `build`, `ci`, `chore`, `revert`.
- `(scope)` optional — component or area affected.
- `(REQ-ID)` required for `feat`/`fix`; optional for `chore`/`docs`.

Set `AIDA_COMMIT_STRICT=true` (or commit through the `/aida-commit` skill) to
enforce; otherwise the commit-msg hook just warns on non-conforming messages.

## Capture proactively, not reactively

The requirements DB is only valuable when it stays in sync with reality.
Treat `/aida-capture` as a habit, not a safety net:

1. **Spec-first when introducing a new theme.** New command, new field on a
   core model, new skill, new architectural pattern — pause and `aida add`
   *before* the implementation commits. ~2 min cost; saves backfill later.
2. **Don't reuse one EPIC as a catchall.** When the work has drifted from
   what the EPIC was originally about, that's a signal to create a new EPIC,
   not stretch the existing one.
3. **Run `/aida-capture` at natural pauses.** End of focused work, before
   compaction, when stepping away. Five-minute pass that catches missed reqs.
4. **Yellow flag at >5 untracked commits.** Five+ feat/fix commits without a
   matching requirement → offer to capture before continuing.
5. **Trace comments must match reality.** A `// trace:EPIC-N` on code that
   has nothing to do with EPIC-N is misinformation that compounds. If you're
   unsure which spec a piece of work belongs to, that's the signal it needs
   its own.

## Glance at the statusbar

`.claude/settings.json` wires `aida statusline` into Claude Code's status
bar. It shows project · active role · queue depth · cache freshness. If the
role you expect isn't there, you forgot to `aida role enter <name>` before
starting the session.

## When `aida pull` refuses (divergent branches)

`aida pull` is two operations in one: a `git pull` of your code branch
and a `git pull --rebase` of the orphan `aida-store` branch. The two
legs are deliberately asymmetric:

- **Code leg**: `git pull --ff-only` — refuses if the branch has
   diverged from origin. Won't surprise your working tree with an
   auto-rebase.
- **Store leg**: `git pull --rebase` — store conflicts are rare and
   the worktree is AIDA-managed.

When the code leg refuses (or raw `git pull` complains about divergent
branches), the recovery recipe:

```bash
git fetch origin "$(git rev-parse --abbrev-ref HEAD)"
git log --oneline @{u}..HEAD     # what you have that origin doesn't
git log --oneline HEAD..@{u}     # what origin has that you don't
git log --name-only @{u}..HEAD --pretty= | sort -u   # files you touched
git log --name-only HEAD..@{u} --pretty= | sort -u   # files they touched
# No overlap → safe: git pull --rebase
# Overlap   → inspect; rebase + resolve, or git rebase --abort
```

To make raw `git pull` Just Work without per-incident decisions (one-time,
machine-global):

```bash
git config --global pull.rebase true
git config --global rebase.autoStash true
git config --global advice.diverging false
```

Trade-off: silent auto-rebase for fewer manual decisions. `autoStash`
preserves uncommitted changes across the rebase. If you'd rather see the
prompt each time, leave these unset and the recipe above is your fallback.

## Review workflow

`aida review prompt --pr N` (or `--specs FR-1,STORY-2,…`) generates a
markdown review prompt that lifts each linked requirement's `## Acceptance`
section verbatim — paste it into a fresh Claude Code review session, or
write it to a file with `--write`.

- **Install `gh` or `glab` for `--pr` mode.** AIDA shells out to
   [`gh pr view`](https://cli.github.com) / [`glab mr view`](https://gitlab.com/gitlab-org/cli)
   to resolve the PR's base + head refs. Without them, AIDA falls back to
   `base=main` and a local review branch named `pr-N` / `mr-N` — that path
   works when the PR was started via `aida session start --owns PR-N`
   (STORY-61), surprising otherwise.
- **Acceptance sections are the contract.** Write a `## Acceptance`,
   `## Verify`, `## Tests`, `## Test cases`, or `## Verification` section
   in every STORY / BUG description so the review prompt has something
   concrete to lift. `aida doctor convention-check` lints for the gap.
<!-- AIDA-AUTOGEN-END -->


## Codex Operating Discipline

### Storage Model

AIDA's source of truth is the git-canonical spec store, not an ad hoc
notes file. Use MCP tools for spec graph and coordination operations
when available; use shell commands for build, test, git inspection, and
cross-surface verification.

### Requirements Management

Before implementing, make sure a requirement exists and read it with
`show_requirement` or `aida show <ID>`. If you file new requirements via
MCP, pass a valid lowercase `type`; AIDA derives the canonical ID prefix
from that type. Do not invent `SPEC-N` IDs.

### Daily-Use Commands

```bash
codex mcp add aida -- aida mcp-serve
aida show <SPEC-ID>
aida list --status approved
aida queue work <SPEC-ID>
aida pr ship
aida brief list --for-agent codex
aida brief ack .aida/agent-briefs/codex/<brief>.md
tests/test_mcp_stdio.sh --skip-agent-contract
tests/test_mcp_doc_consistency.sh
```

### MCP Coordination

Use AIDA MCP for substrate operations: `show_requirement`,
`list_active_leases`, `claim_task`, `release_task`, `file_finding`,
`post_punt`, `list_briefs`, `read_brief`, `ack_brief`, `add_comment`,
and directive tools. Trust MCP `tools/list` for argument names. Current
responses are text envelopes; parse defensively until structuredContent
ships.

For cross-agent communication semantics, especially Claude Code
`PreToolUse` / `PostToolUse`, `continue: false`, `ask`, and `defer`, use
`docs/agents/session-communication.md`. Do not assume a later hook can ask
whether to continue after an earlier hook has halted the run.

### Worktree And Session Discipline

Do implementation work in a sibling worktree. Link `.aida-store` into
that worktree when the local CLI needs direct store access. Do not edit
another agent's dirty main worktree. If a branch, lease, or worktree
state looks inconsistent, stop and surface it instead of forcing git.

### Code Traceability

When code implements a spec, add a trace comment in the touched code:

```rust
// trace:TASK-123 | ai:codex
```

Keep spec IDs in developer artifacts: commits, PR titles, trace
comments, and plans. Do not leak internal IDs into user-facing CLI text
unless that output is explicitly developer/operator-facing.

### Commit And PR Format

Use the Codex prefix and put every shipped spec in trailing parens:

```text
[AI:codex] fix(scope): concise description (TASK-123)
[AI:codex] docs(agents): Codex setup integration (STORY-417 TASK-485 TASK-484)
```

The auto-bump scanner reads the trailing parens. If one PR closes
multiple specs, include every spec ID in that group.

### Sketch-First Protocol

Before opening a PR for architecture-class changes, post a sketch on the
owning spec and wait for master sign-off. Architecture-class means file
formats, MCP tool contracts, orchestrator semantics, lease model,
cross-cutting lifecycle vocabulary, or discipline/memory changes.
Bounded tests, docs refreshes, and acceptance-criteria implementation do
not need a sketch unless they introduce a reusable harness or new
project convention.

### Known Codex Pitfalls

- PR-201 missed the trailing spec trailer in the squash subject; that
  incident is why trailing-parens discipline is non-optional.
- Read the `aida pr ship` arc before relying on the wrapper in a new
  environment: SPEC-410, BUG-339, BUG-344, and BUG-345 document subject
  repair, parser alignment, CI startup waiting, and stale-main-worktree
  handling.
- `aida mcp-serve` self-respawns after handled requests when the on-disk
  `aida --version` reports a newer package version or different build
  SHA. If MCP still appears stale, kill that agent's server process and
  let the client respawn it.
- If an instruction from another session sounds inconsistent with the
  branch contents, verify the PR contents and flag the mismatch.
