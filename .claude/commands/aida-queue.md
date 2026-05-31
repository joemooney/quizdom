<!-- AIDA Generated: v2.0.0 | checksum:6be21b89 | DO NOT EDIT DIRECTLY -->
<!-- To customize: copy this file and modify the copy -->

# View Queue Contents

Show what's queued for the active role — read-only counterpart to `/aida-pickup`.

## Instructions

Follow the workflow in `.claude/skills/aida-queue.md`:

1. Show active role + `aida queue list` (role-routed + scope-filtered by default)
2. If filtered queue is empty, offer `--no-scope` and `--all` as broader views
3. Suggest next actions: `/aida-pickup` to start, `aida queue move/remove`, `aida show <id>`
4. Do NOT auto-pick anything up — that's `/aida-pickup`'s job

Pairs with `/aida-pickup` on the action side.