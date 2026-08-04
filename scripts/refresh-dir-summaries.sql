-- Re-generate every directory summary by enqueueing dir_summary_sync outbox
-- events. Run after a DIR_L0/DIR_L1 prompt change; the worker recomputes from
-- child L0s and upserts in place (idempotent, no delete window). First used
-- 2026-08-04 for the 0.1.25 "short introduction" prompt.
--
-- Cost model: each directory = 2 LLM calls + 1 embedding. Dry-run first and
-- sanity-check `2 x dirs_to_refresh` against upstream quota.
--
-- Throttle: available_at is staggered so only @per_min directories become
-- claimable per minute. This is the ONLY throttle protecting online traffic —
-- the LLM path has no concurrency gate (embedding does), and the outbox is a
-- global id-ordered FIFO, so un-staggered bulk rows would head-of-line block
-- fresh online events. Ranking is GLOBAL on purpose: the 2026-08-04 run used
-- PARTITION BY workspace_id ("fairness"), which turned 20/min into
-- 20/min/workspace — 25 workspaces made 55 of 78 rows claimable instantly.
-- Harmless then (worker batch_size=10 was the real bottleneck), a burst
-- hazard on many-workspace databases.
--
-- Rules that came from reading the worker, do not "simplify" them away:
--   * payload.parent_path holds the directory's OWN path (misleading name,
--     confirmed against worker.rs handler bindings).
--   * Deepest-first ordering => bottom-up processing (claim is ORDER BY id
--     ASC), so parents aggregate already-refreshed children in one pass.
--   * NOT EXISTS guard replicates enqueue_dedup: veda_outbox has no unique
--     index; raw SQL bypasses the Rust-side dedup check.
--   * The SET time_zone below is what makes the staircase real, not a
--     nicety. available_at is a TIMESTAMP column: MySQL interprets written
--     values in the SESSION time zone. Both company MySQL instances default
--     to Asia/Shanghai, so writing UTC_TIMESTAMP() from a default session
--     stores UTC-8h — and the worker (UTC session, `available_at <=
--     UTC_TIMESTAMP()`) sees every row as already due. Discovered
--     2026-08-04: every earlier "throttled" run was actually a full burst
--     (22 rows at once survived; a 69-dir burst drew 467×429 + 22 dead).
--   * UTC_TIMESTAMP() everywhere — NOW() misfires in a non-UTC session.
--     The inverse trap: veda_summaries `updated_at` is LOCAL time, so don't
--     filter it with UTC_TIMESTAMP() when sampling refreshed rows (bit an
--     operator on 2026-08-04).

SET time_zone = '+00:00';
--
-- Scope to one workspace by adding `AND d.workspace_id = '<id>'` to the two
-- WHERE clauses below.

-- ── 1. dry-run: how many directories, how many LLM calls ──────────────
SELECT COUNT(*) AS dirs_to_refresh
FROM veda_dentries d
WHERE d.is_dir = 1
  AND EXISTS (SELECT 1 FROM veda_summaries s
              WHERE s.dentry_id = d.id AND s.status = 'ready');

-- ── 2. enqueue with global staircase ──────────────────────────────────
-- 8/min, NOT higher: with summary_disable_thinking the worker sustains
-- ~29 dirs/min and the ceiling moved from LLM latency to the upstream TPM
-- quota — a 2026-08-04 burst of 69 dirs drew 467 HTTP 429
-- ("insufficient_quota", token-limit) and 22 dead letters in minutes,
-- while a 22-dir burst survived. Honest history: the timezone bug above
-- meant no staircase run before 2026-08-04 was actually rate-limited, so
-- 8/min is chosen as comfortably below the worker's own ~29/min ceiling
-- rather than as a measured safe rate. Raise only with quota headroom
-- evidence.
SET @per_min := 8;

INSERT INTO veda_outbox
    (workspace_id, event_type, payload, status, retry_count, max_retries,
     available_at, lease_until, created_at)
SELECT
    workspace_id,
    'dir_summary_sync',
    JSON_OBJECT('dentry_id', id, 'parent_path', path),
    'pending', 0, 3,
    DATE_ADD(UTC_TIMESTAMP(), INTERVAL ((rn - 1) DIV @per_min) MINUTE),
    NULL, UTC_TIMESTAMP()
FROM (
    SELECT d.id, d.workspace_id, d.path,
           ROW_NUMBER() OVER (
               ORDER BY (LENGTH(d.path) - LENGTH(REPLACE(d.path, '/', ''))) DESC, d.path
           ) AS rn
    FROM veda_dentries d
    WHERE d.is_dir = 1
      AND EXISTS (SELECT 1 FROM veda_summaries s
                  WHERE s.dentry_id = d.id AND s.status = 'ready')
      AND NOT EXISTS (
          SELECT 1 FROM veda_outbox o
          WHERE o.workspace_id = d.workspace_id
            AND o.event_type = 'dir_summary_sync'
            AND o.status IN ('pending', 'processing')
            AND JSON_UNQUOTE(JSON_EXTRACT(o.payload, '$.dentry_id')) = d.id
      )
) t
ORDER BY rn;

-- ── 3. progress / verification ────────────────────────────────────────
-- Status counts; pending+processing reaching 0 = converged. Observed
-- effective worker ceilings 2026-08-04: 9-13 dirs/min with thinking on,
-- ~29 dirs/min with summary_disable_thinking — which is why the staircase
-- rate above is now the binding limit and must stay under the TPM quota.
SELECT status, COUNT(*) FROM veda_outbox
WHERE event_type = 'dir_summary_sync' GROUP BY status;

-- Claimable now vs still gated by the staircase.
SELECT SUM(available_at <= UTC_TIMESTAMP()) AS claimable,
       SUM(available_at >  UTC_TIMESTAMP()) AS scheduled
FROM veda_outbox
WHERE event_type = 'dir_summary_sync' AND status = 'pending';

-- Dead letters (expect 0) and the empty-abstract sentinel (expect 0;
-- the 2026-07 incident shape — investigate before continuing if it rises).
SELECT COUNT(*) AS dead FROM veda_outbox
WHERE event_type = 'dir_summary_sync' AND status = 'dead';
SELECT COUNT(*) AS empty_l0 FROM veda_summaries
WHERE dentry_id IS NOT NULL AND l0_abstract = '';

-- ── 4. requeue dead letters (after a 429 storm) ───────────────────────
-- Re-staircase them instead of releasing all at once — releasing 22 rows
-- immediately is exactly the burst that killed them the first time.
SET @per_min := 8;
UPDATE veda_outbox o
JOIN (
    SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS rn
    FROM veda_outbox
    WHERE event_type = 'dir_summary_sync' AND status = 'dead'
) t ON t.id = o.id
SET o.status = 'pending', o.retry_count = 0, o.lease_until = NULL,
    o.available_at = DATE_ADD(UTC_TIMESTAMP(), INTERVAL ((t.rn - 1) DIV @per_min) MINUTE);
