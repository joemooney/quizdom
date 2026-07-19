-- trace:STORY-205 | ai:claude
-- Recursive-CTE walk of the `begets` chain from Q-1 — the traversal shape
-- that retires the ADR-31 one-hop BFS once the Dolt backend lands
-- (STORY-207). Against db/fixtures/traversal_fixture.sql it must return:
--   Q-1 (depth 0), Q-2 (depth 1), Q-3 (depth 2)
WITH RECURSIVE reachable (id, depth) AS (
    SELECT CAST('Q-1' AS CHAR(64)), 0
    UNION ALL
    SELECT e.to_id, r.depth + 1
    FROM edges e
    JOIN reachable r ON e.from_id = r.id
    WHERE e.kind = 'begets' AND r.depth < 10
)
SELECT id, depth FROM reachable ORDER BY depth, id;
