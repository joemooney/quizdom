# quizdom Graph Schema

<!-- trace:STORY-14 | ai:codex -->
<!-- trace:STORY-209 | ai:claude -->

This document is the canonical schema for quizdom's domain graph. Domain
objects live in a local [Dolt](https://www.dolthub.com/) repo (ADR-201,
EPIC-202; see the *Dolt Schema* section below) — the AIDA store remains
canonical for project intent only. *Historical note (ADR-3, superseded):
v1 kept the domain graph in the AIDA store to dogfood AIDA as a shared
knowledge substrate; STORY-208 cut the runtime over to Dolt and STORY-209
removed the store-side domain objects.* User-specific exploration logs stay
separate until a belief is intentionally promoted.

## Object Model

Each graph object is a row in the Dolt `nodes` table with a stable ID,
title, body, tags, and typed edges. quizdom uses the prefixes below to make
object roles clear in titles, tags, and traversals.

| Prefix | Node type | Purpose | Required fields |
|---|---|---|---|
| `Q` | Question | A yes/no, multiple-choice, or free-text prompt that can be asked in a session. | Title as the prompt text; description with answer mode and intended use; tags for topic and quality. |
| `TERM` | Term definition | A formal, public, academic, or user-specific definition of a loaded term. | Title as the term plus definition label; description with source, definition text, and scope notes. |
| `BELIEF` | Belief proposition | A claim a user or shared corpus may hold, test, refine, agree with, or reject. | Title as a concise proposition; description with provenance and interpretation notes. |

### Question Nodes

Question nodes are reusable prompts. Their description must identify the answer
shape:

- `answer: yes-no` for binary prompts.
- `answer: choice[...]` for bounded multiple choice.
- `answer: free-text` for definition, nuance, or explanation capture.

Question nodes should not encode a correct answer. They exist to branch,
clarify, or stress-test belief structure.

### Term Nodes

Term nodes describe competing meanings of loaded words such as "free will",
"responsibility", or "consciousness". Prefer public or academic definitions
before creating a user-specific definition. When a bespoke definition is
needed, tag it `definition:user-specific` and include the session-log reference
that produced it.

### Belief Nodes

Belief nodes capture propositions, not raw answers. A raw answer remains in the
per-user session log until it is worth promoting. Promotion should preserve the
source session, original wording, normalized proposition, and any definition
nodes needed to make the proposition intelligible.

## Edge Vocabulary

Relationships are typed rows in the Dolt `edges` table. The source and
target order matters.

| Edge | Source -> target | Meaning |
|---|---|---|
| `begets` | `Q -> Q` or `BELIEF -> Q` | An answer or proposition naturally leads to the next question. |
| `probes` | `Q -> TERM` or `Q -> BELIEF` | A question tests understanding of a term or pressure-tests a belief. |
| `refines` | `TERM -> TERM`, `BELIEF -> BELIEF`, or `Q -> Q` | The source narrows, clarifies, or improves the target. |
| `contradicts` | `BELIEF -> BELIEF` | Two propositions cannot both be held under the same definitions. |
| `agrees` | `BELIEF -> BELIEF` | The source supports or is compatible with the target. |
| `disagrees` | `BELIEF -> BELIEF` | The source rejects or stands against the target without strict logical contradiction. |

Edge reads are ordered, and the order is a contract (TASK-221): `neighbors`
and `neighbors_many` return a source's targets oldest-first by `created_at`,
ties broken by `to_id` in lexical order. `edges.created_at` is a 1-second
`TIMESTAMP`, so edges written in the same second — the common case for one
batch of writes — are ordered entirely by the tie-break, which puts `Q-10`
ahead of `Q-2`. Callers wanting a numeric or semantic order sort for
themselves.

These six kinds are the only values the `edges.kind` column admits. Links
that are project intent rather than domain structure — e.g. a
contradiction-resolution decision node pointing at project specs — stay in
AIDA as its normal `parent`, `child`, `references`, `blocked-by`, or
`verifies` edges.

## Tag Conventions

Tags describe topic, answer shape, quality, definition status, and selection
weight. Keep them lowercase and hyphenated unless a namespace below requires a
colon.

| Tag pattern | Applies to | Meaning |
|---|---|---|
| `topic:<name>` | all nodes | Major topic, such as `topic:free-will`. |
| `answer:<shape>` | `Q` | Answer shape, such as `answer:yes-no` or `answer:free-text`. |
| `definition:<kind>` | `TERM` | Definition source class: `formal`, `academic`, `public`, or `user-specific`. |
| `quality:<state>` | `Q` | Reuse signal: `insightful`, `neutral`, or `unhelpful`. |
| `from-answer:<value>` | `Q` | Records the normalized answer that triggered this follow-on, so different answers to the origin can branch to different follow-ups. |
| `seed` | all nodes | Hand-authored seed data used to bootstrap a cluster. |

### Answer-Conditioned Follow-ons

<!-- trace:STORY-48 | ai:claude -->

A `begets` edge is `Q -> Q` and records *that* one question leads to another,
but not *which* answer triggered it. To branch different answers to different
follow-ups, the generated follow-on question carries a `from-answer:<value>`
tag whose value is the normalized triggering answer (e.g. `from-answer:yes`,
`from-answer:no`, or a choice option). Only bounded answers (yes/no, choice)
condition a follow-on; open-ended free-text answers leave the follow-on
unconditional.

The `NextQuestionStrategy` (STORY-18/37) reads this tag when picking the next
question:

- A successor whose `from-answer` matches the current answer is **preferred**.
- A successor with **no** `from-answer` tag is an **unconditional** follow-on,
  always eligible as a fallback.
- A successor whose `from-answer` names a **different** answer is **excluded**
  from automatic selection for the current answer.

**Substrate note (historical, VIS-2).** The triggering answer lives on the
target node as a tag rather than on the `begets` edge itself — originally
because `aida rel add` had no edge-attribute support. The Dolt `edges` table
could now carry an `on-answer` column (which would also let a single shared
follow-on be reached from more than one answer); until a need materializes,
the node-tag encoding stands.

### Weight Encoding

Selection weight is the numeric `weight` column on `nodes` — an integer
from `0` to `100`. *(Historical: ADR-22, now retired, encoded this as a
`weight:N` tag while the graph lived in the AIDA store; `quizdom
db-migrate` converted the tags into the column.)*

- `0` means never select automatically, but keep for history.
- `1` through `39` means low-priority reuse.
- `40` through `69` means normal reuse.
- `70` through `100` means high-priority reuse.

When `quality:*` and `weight` disagree, treat `weight` as the current
selection signal and `quality:*` as human-readable history. Update the
weight when repeated sessions show that a question is more or less useful.

## Worked Example

The first seed cluster for free will should look like this shape:

```text
Q: Do you believe in free will?
  tags: topic:free-will, answer:yes-no, quality:neutral, seed   weight: 70

TERM: free will / libertarian
  tags: topic:free-will, definition:academic, seed              weight: 60

TERM: free will / compatibilist
  tags: topic:free-will, definition:academic, seed              weight: 60

Q: Do you mean the ability to have chosen otherwise in exactly the same conditions?
  tags: topic:free-will, answer:yes-no, quality:neutral, seed   weight: 65

Q: Can a choice be free if it is fully caused by prior events?
  tags: topic:free-will, answer:yes-no, quality:neutral, seed   weight: 65

BELIEF: Free will requires genuine alternative possibilities
  tags: topic:free-will, seed                                   weight: 50

BELIEF: Free will is compatible with causal determinism
  tags: topic:free-will, seed                                   weight: 50

Edges:
  Q "Do you believe in free will?"
    probes -> TERM "free will / libertarian"
    probes -> TERM "free will / compatibilist"
    begets -> Q "Do you mean the ability to have chosen otherwise..."
    begets -> Q "Can a choice be free if it is fully caused..."

  TERM "free will / libertarian"
    refines -> TERM "free will / compatibilist"

  BELIEF "Free will requires genuine alternative possibilities"
    disagrees -> BELIEF "Free will is compatible with causal determinism"
```

This example intentionally keeps the seed question neutral. Later session logs
may promote user-specific beliefs into `BELIEF` nodes and connect them to the
same term definitions with `agrees`, `disagrees`, or `contradicts` edges.

## Related Requirements

- `EPIC-5`: Domain graph model on AIDA.
- `STORY-14`: Schema doc for node types, edges, tags, weights, and example
  subgraph.
- `STORY-15`: Per-user session log and promotion path, which depends on this
  schema.
- `STORY-16`: Free-will seed cluster, which should instantiate this schema.

## Dolt Schema (ADR-201 / EPIC-202)

<!-- trace:STORY-205 | ai:claude -->

ADR-201 (superseding ADR-3) moves the domain graph out of the AIDA store
into [Dolt](https://www.dolthub.com/), a versioned MySQL-compatible
database; AIDA remains canonical for project intent. The SQL schema below mirrors the
object model above and lives in `db/schema.sql`, applied by
`quizdom db-init` (idempotent — `CREATE TABLE IF NOT EXISTS` only, safe to
re-run). The default repo location is `data/dolt` (`--path` overrides).

### Tables

- **`nodes`** — one row per graph object: `id` (the `Q-*` / `TERM-*` /
  `BELIEF-*` identifier), `kind` (`question` | `term` | `belief`), `title`,
  `body` (the descriptive text that held answer-mode / definition / scope
  notes in AIDA), `tags` (comma-joined, same vocabulary as the tag table
  above, minus `weight:N`), `weight` (integer `0`–`100` — the ADR-22
  `weight:N` tag becomes a real column), and created/updated timestamps.
- **`edges`** — one row per typed edge: `from_id`, `to_id`, and `kind`
  constrained to the six custom edges of the vocabulary table
  (`begets`, `probes`, `refines`, `contradicts`, `agrees`, `disagrees`).
  The primary key `(from_id, to_id, kind)` makes duplicate edges
  unrepresentable; both endpoints are foreign keys into `nodes`.

### Mapping from the AIDA representation

| AIDA construct | Dolt equivalent |
|---|---|
| `Q-*` / `TERM-*` / `BELIEF-*` object | `nodes` row with the matching `kind` |
| Object description | `nodes.body` |
| Tags (`topic:*`, `answer:*`, `quality:*`, …) | `nodes.tags` (comma-joined) |
| `weight:N` tag (ADR-22) | `nodes.weight` integer column |
| Custom relationship (`aida rel add --type begets` …) | `edges` row |
| One-hop walk via `aida rel list` (ADR-31) | recursive CTE over `edges` |

### Migration

<!-- trace:STORY-206 | ai:claude -->

`quizdom db-migrate` is the one-shot exporter that loads the AIDA-side
domain graph into the tables above (after `quizdom db-init`). It reads the
inventory through the `aida` CLI (`Q-*` and `BELIEF-*` are `functional`
objects, `TERM-*` are `term` objects), converts each `weight:N` tag into
the numeric `weight` column, keeps only the six custom edge kinds whose
endpoints are both domain nodes, and verifies parity at the end: node and
edge counts per kind (aida-side vs Dolt-side) plus a recursive-CTE walk of
a `begets` lineage (default root `Q-23`, `--spot-check <id>|none`
overrides) compared against an app-side BFS over the same edges. Re-running
is safe — nodes upsert and edges insert-ignore. Node timestamps are
load-time defaults, not the AIDA `Opened`/`Modified` times.

<!-- trace:STORY-209 | ai:claude -->
After the STORY-208 cutover, parity was verified one final time and the
store-side domain objects (`Q-*`/`TERM-*`/`BELIEF-*` and their custom
edges) were deleted from the AIDA store (STORY-209) — Dolt's history is
the domain graph's version control now. `db-migrate` remains available for
importing a legacy store, but this repo's store no longer holds domain
data.

### Traversal

ADR-31's one-hop-at-a-time BFS existed because `aida graph` cannot follow
custom edges. SQL removes that constraint: `db/fixtures/traversal_check.sql`
is the canonical recursive-CTE walk (the `begets` chain from a seed
question), verified against the hand-inserted fixture in
`db/fixtures/traversal_fixture.sql`. The Dolt backend (STORY-207) adopts
this traversal; the cutover story (STORY-208) retires the BFS.
