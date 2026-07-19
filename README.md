# quizdom

**quiz + wisdom** — a Socratic, branching belief-exploration tool. Not trivia:
there are no correct answers. quizdom interviews you about existential and
philosophical questions (yes/no, multiple-choice, free text), maps the beliefs
you express into a persistent graph, and then probes that graph for tensions
and contradictions worth examining. See [OVERVIEW.md](OVERVIEW.md) for the
vision and architecture in depth.

## Prerequisites

- **Rust toolchain** (stable) — the app is a Cargo workspace; the binary crate
  is `crates/quizdom`.
- **[AIDA](https://github.com/joemooney/aida) CLI (`aida`)** on your `PATH`.
  quizdom has no database of its own: questions, terms, beliefs, and the edges
  between them are AIDA objects, read and written by shelling out to `aida`.
  The store lives on the `aida-store` orphan branch of this repo (cloning the
  repo brings it along; `aida` materializes it under `.aida-store/`).
- **[Claude Code](https://claude.com/claude-code) CLI (`claude`)** — the
  default LLM backend (per ADR-39) drives follow-up questioning through the
  `claude` CLI. Alternatively, use the raw Anthropic API by setting
  `QUIZDOM_BACKEND=anthropic` and `ANTHROPIC_API_KEY` (see below).

## Quickstart

There is deliberately no wrapper script — `cargo run` is one line and the
flags are the interface.

```bash
# Start a new session (interactive TUI on a TTY); same as `session start`
cargo run -p quizdom

# Start from a specific seed question in Socratic mode
cargo run -p quizdom -- session start --seed Q-23 --mode socratic

# Resume the most recent session (or pass a session id)
cargo run -p quizdom -- session resume

# Detect contradictions among your adopted beliefs
cargo run -p quizdom -- contradictions

# Re-weight the question bank from session signals
cargo run -p quizdom -- curate

# Author a new question into the bank
cargo run -p quizdom -- question add --seed Q-23
```

Run `cargo run -p quizdom -- --help` for the full command list, and
`quizdom <command> --help` for per-command options (e.g. `--mode
socratic|debate`, `--strategy deterministic|weighted|llm`, `--no-tui` for the
headless line UI).

## Environment variables

| Variable | Effect |
| --- | --- |
| `QUIZDOM_BACKEND` | LLM backend: `claude-cli` (default) or `anthropic`. |
| `QUIZDOM_MODEL` | Model override. With the `anthropic` backend the default is `claude-sonnet-4-6`; with `claude-cli` the CLI's own default model is used unless this is set. |
| `QUIZDOM_CLAUDE_COMMAND` | Command to invoke for the `claude-cli` backend (default: `claude`). |
| `QUIZDOM_STRATEGY` | Follow-up selection strategy: `deterministic` (default), `weighted`, or `llm`. |
| `ANTHROPIC_API_KEY` | Required when `QUIZDOM_BACKEND=anthropic`. |
| `NO_COLOR` | Set (to anything) to disable colored output. |

## Learn more

- [OVERVIEW.md](OVERVIEW.md) — vision, target users, architecture.
- [docs/architecture/graph-schema.md](docs/architecture/graph-schema.md) —
  the canonical belief-graph schema (`Q-*` questions, `TERM-*` definitions,
  `BELIEF-*` propositions and their edges).
- [CLAUDE.md](CLAUDE.md) — agent-facing development guidance (build/test
  commands, working discipline, AIDA conventions).
