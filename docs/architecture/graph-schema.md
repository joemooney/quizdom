# quizdom Graph Schema

<!-- trace:STORY-14 | ai:codex -->

This document is the canonical v1 schema for quizdom's domain graph. Domain
objects live in AIDA so the product can dogfood AIDA as its shared knowledge
substrate while keeping user-specific exploration logs separate until a belief
is intentionally promoted.

## Object Model

Each graph object is represented as an AIDA requirement-like object with a
stable ID, title, description, tags, and typed relationships. quizdom uses the
prefixes below to make object roles clear in titles, tags, and relationship
traversals even before AIDA has first-class domain object types.

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

Relationships are typed AIDA edges. The source and target order matters.

| Edge | Source -> target | Meaning |
|---|---|---|
| `begets` | `Q -> Q` or `BELIEF -> Q` | An answer or proposition naturally leads to the next question. |
| `probes` | `Q -> TERM` or `Q -> BELIEF` | A question tests understanding of a term or pressure-tests a belief. |
| `refines` | `TERM -> TERM`, `BELIEF -> BELIEF`, or `Q -> Q` | The source narrows, clarifies, or improves the target. |
| `contradicts` | `BELIEF -> BELIEF` | Two propositions cannot both be held under the same definitions. |
| `agrees` | `BELIEF -> BELIEF` | The source supports or is compatible with the target. |
| `disagrees` | `BELIEF -> BELIEF` | The source rejects or stands against the target without strict logical contradiction. |

Use these quizdom-specific edge names as custom AIDA relationship types. If a
relationship is merely implementation dependency or ownership, use AIDA's
normal `parent`, `child`, `references`, `blocked-by`, or `verifies` edges
instead.

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
| `weight:N` | reusable nodes | Selection weight from `0` through `100`. |
| `seed` | all nodes | Hand-authored seed data used to bootstrap a cluster. |

### Weight Encoding

`weight:N` is the only weight encoding. `N` is an integer from `0` to `100`.

- `weight:0` means never select automatically, but keep for history.
- `weight:1` through `weight:39` means low-priority reuse.
- `weight:40` through `weight:69` means normal reuse.
- `weight:70` through `weight:100` means high-priority reuse.

When `quality:*` and `weight:N` disagree, treat `weight:N` as the current
selection signal and `quality:*` as human-readable history. Update the weight
when repeated sessions show that a question is more or less useful.

## Worked Example

The first seed cluster for free will should look like this shape:

```text
Q: Do you believe in free will?
  tags: topic:free-will, answer:yes-no, quality:neutral, weight:70, seed

TERM: free will / libertarian
  tags: topic:free-will, definition:academic, weight:60, seed

TERM: free will / compatibilist
  tags: topic:free-will, definition:academic, weight:60, seed

Q: Do you mean the ability to have chosen otherwise in exactly the same conditions?
  tags: topic:free-will, answer:yes-no, quality:neutral, weight:65, seed

Q: Can a choice be free if it is fully caused by prior events?
  tags: topic:free-will, answer:yes-no, quality:neutral, weight:65, seed

BELIEF: Free will requires genuine alternative possibilities
  tags: topic:free-will, weight:50, seed

BELIEF: Free will is compatible with causal determinism
  tags: topic:free-will, weight:50, seed

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
