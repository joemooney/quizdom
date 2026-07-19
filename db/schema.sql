-- trace:STORY-205 | ai:claude
-- Dolt schema for the quizdom domain graph (EPIC-202 / ADR-201).
--
-- Mirrors docs/architecture/graph-schema.md: nodes carry the typed kind and
-- the ADR-22 selection weight as a real numeric column; edges carry the six
-- custom edge kinds of the schema doc. Applied by `quizdom db-init`, and
-- idempotent by construction — every statement is CREATE TABLE IF NOT EXISTS,
-- so re-running db-init on an initialised repo is a no-op.

CREATE TABLE IF NOT EXISTS nodes (
    id         VARCHAR(64)   NOT NULL,
    kind       ENUM('question', 'term', 'belief') NOT NULL,
    title      TEXT          NOT NULL,
    body       TEXT,
    tags       VARCHAR(2048) NOT NULL DEFAULT '',
    weight     INT           NOT NULL DEFAULT 0,
    created_at TIMESTAMP     NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP     NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    CONSTRAINT chk_nodes_weight CHECK (weight BETWEEN 0 AND 100)
);

CREATE TABLE IF NOT EXISTS edges (
    from_id    VARCHAR(64) NOT NULL,
    to_id      VARCHAR(64) NOT NULL,
    kind       ENUM('begets', 'probes', 'refines', 'contradicts', 'agrees', 'disagrees') NOT NULL,
    created_at TIMESTAMP   NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (from_id, to_id, kind),
    KEY idx_edges_to (to_id),
    CONSTRAINT fk_edges_from FOREIGN KEY (from_id) REFERENCES nodes (id),
    CONSTRAINT fk_edges_to   FOREIGN KEY (to_id)   REFERENCES nodes (id)
);
