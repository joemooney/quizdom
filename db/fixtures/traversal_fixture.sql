-- trace:STORY-205 | ai:claude
-- Hand-inserted fixture for verifying recursive-CTE traversal over the edges
-- table (the STORY-205 acceptance check). Apply after db/schema.sql:
--   dolt sql < db/fixtures/traversal_fixture.sql
INSERT INTO nodes (id, kind, title, tags, weight) VALUES
    ('Q-1', 'question', 'Does free will require an uncaused cause?', 'topic:free-will,answer:yes-no', 70),
    ('Q-2', 'question', 'What do you mean by cause?', 'topic:free-will,answer:free-text', 50),
    ('Q-3', 'question', 'Is moral responsibility possible without free will?', 'topic:free-will,answer:yes-no', 60),
    ('TERM-1', 'term', 'free will / libertarian', 'topic:free-will,definition:academic', 40);

INSERT INTO edges (from_id, to_id, kind) VALUES
    ('Q-1', 'Q-2', 'begets'),
    ('Q-2', 'Q-3', 'begets'),
    ('Q-1', 'TERM-1', 'probes');
