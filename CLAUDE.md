# CLAUDE.md

Guidance for Claude Code working in this repository. AIDA conventions
(trace format, commit format, daily commands, capture rules) live in
`.claude/AIDA.md` — Claude Code expands the import below automatically,
so you'll see them in context without this file having to duplicate
them.

@.claude/AIDA.md

## Project overview

quizdom

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

