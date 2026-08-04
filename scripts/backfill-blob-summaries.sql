-- Backfill L0/L1 summaries for PDF/Word files uploaded before the
-- extract → summary handoff existed. Those files got a text layer and
-- vectors but never a SummarySync, so `veda abstract` answered 202
-- "pending" forever. Run AFTER deploying the fix — on an old binary these
-- rows are claimed and silently skipped by the blob guard, burning the
-- queue slot for nothing.
--
--   mysql -h <host> -u <user> -p <db> < scripts/backfill-blob-summaries.sql
--
-- Cost model: each file = 2 LLM calls (L0 + L1) + 1 embedding. But that is
-- NOT the whole draw — finishing a file's SummarySync enqueues a
-- DirSummarySync on its parent (2 more LLM), and that handler recurses to
-- the root, so a file at /a/b/c.pdf can touch 3 ancestors. enqueue_dedup
-- and the 30s burst debounce coalesce much of it, not all. Budget ~4 LLM
-- per file (the estimate in docs/plans/pdf-word-summary-gap.md §6) and
-- check `4 x files_to_summarize` against upstream quota before opening the
-- gate.
--
-- Throttle: available_at is staggered so only @per_min files become
-- claimable per minute — the ONLY throttle protecting online traffic. The
-- LLM path has no concurrency gate (embedding does), and the outbox is a
-- global id-ordered FIFO, so un-staggered bulk rows head-of-line block
-- fresh online events.
--
--   * The cascaded DirSummarySync rows are enqueued BY THE WORKER at
--     UTC_TIMESTAMP() (or +30s when debounced) — they are NOT on this
--     staircase and become claimable immediately. That is precisely why
--     @per_min carries the amplification factor instead of just
--     budget / 2.
--   * Derivation: refresh-dir-summaries.sql sustains 8 dirs/min = 16 LLM
--     calls/min, measured 2026-08-04 as sitting under the upstream TPM
--     wall (an effectively unthrottled probe drew 467 HTTP 429s and 22
--     dead letters within minutes). Same 16 LLM/min budget here, at ~4 LLM
--     per file => 4 files/min. Conservative on purpose: unlike the dir
--     refresh, part of this load arrives off-staircase.
--   * Raise only with quota headroom evidence, and watch the dead-letter
--     count in section 3 while it drains.
--
-- Rules that came from reading the worker, do not "simplify" them away:
--   * Ranking is GLOBAL. Do NOT add PARTITION BY workspace_id: the
--     2026-08-04 dir run did exactly that for "fairness" and turned 20/min
--     into 20/min/workspace — 25 workspaces made 55 of 78 rows claimable
--     at once. Ordering BY workspace_id (below) is fine and is not the
--     same thing.
--   * Files are ordered by their directory so same-directory files land in
--     the same window and their parent's DirSummarySync coalesces into one
--     run instead of one per file. This is the main lever on the 4x
--     amplification above. MIN() subquery rather than a JOIN on
--     veda_dentries: a JOIN would fan out (and duplicate outbox rows) if a
--     file ever has more than one dentry.
--   * NOT EXISTS guard replicates enqueue_dedup: veda_outbox has no unique
--     index; raw SQL bypasses the Rust-side dedup check.
--   * Freshness (fe.source_sha256 = f.checksum_sha256) is not optional —
--     it is the same predicate the worker uses. A stale extract is the
--     previous revision's text; summarizing it publishes an L0/L1 that
--     describes content the file no longer has. Files with a stale extract
--     are deliberately left out: their pending ExtractSync will refresh the
--     text and enqueue the SummarySync itself.
--   * SummarySync payload is JSON_OBJECT('file_id', f.id), dedup key
--     `file_id` (see make_outbox in veda-core/src/service/fs.rs).
--   * UTC_TIMESTAMP() everywhere — the claim predicate compares against
--     it; NOW() misfires in a non-UTC session. The inverse trap:
--     veda_summaries `updated_at` is LOCAL time, so don't filter it with
--     UTC_TIMESTAMP() when sampling refreshed rows.
--
-- Scope to one workspace by adding `AND f.workspace_id = '<id>'` to the two
-- WHERE clauses below.

-- ── 1. dry-run: how many files, how many LLM calls ────────────────────
-- Multiply by ~4, not 2, for the parent-directory cascade.
SELECT COUNT(*) AS files_to_summarize
FROM veda_files f
JOIN veda_file_extracts fe
  ON fe.file_id = f.id AND fe.source_sha256 = f.checksum_sha256
LEFT JOIN veda_summaries s ON s.file_id = f.id
WHERE f.source_type IN ('pdf', 'word')
  AND f.storage_type = 'blob'
  AND s.file_id IS NULL;

-- ── 2. enqueue with global staircase ──────────────────────────────────
SET @per_min := 4;

INSERT INTO veda_outbox
    (workspace_id, event_type, payload, status, retry_count, max_retries,
     available_at, lease_until, created_at)
SELECT
    workspace_id,
    'summary_sync',
    JSON_OBJECT('file_id', id),
    'pending', 0, 3,
    DATE_ADD(UTC_TIMESTAMP(), INTERVAL ((rn - 1) DIV @per_min) MINUTE),
    NULL, UTC_TIMESTAMP()
FROM (
    SELECT f.id, f.workspace_id,
           ROW_NUMBER() OVER (
               ORDER BY f.workspace_id,
                        (SELECT MIN(d.path) FROM veda_dentries d
                          WHERE d.file_id = f.id),
                        f.id
           ) AS rn
    FROM veda_files f
    JOIN veda_file_extracts fe
      ON fe.file_id = f.id AND fe.source_sha256 = f.checksum_sha256
    LEFT JOIN veda_summaries s ON s.file_id = f.id
    WHERE f.source_type IN ('pdf', 'word')
      AND f.storage_type = 'blob'
      AND s.file_id IS NULL
      AND NOT EXISTS (
          SELECT 1 FROM veda_outbox o
          WHERE o.workspace_id = f.workspace_id
            AND o.event_type = 'summary_sync'
            AND o.status IN ('pending', 'processing')
            AND JSON_UNQUOTE(JSON_EXTRACT(o.payload, '$.file_id')) = f.id
      )
) t
ORDER BY rn;

-- ── 3. progress / verification ────────────────────────────────────────
-- Status counts; pending+processing reaching 0 = converged. Watch
-- dir_summary_sync too — it is the cascade this backfill sets off, and it
-- shares the same LLM quota without sharing the staircase.
SELECT event_type, status, COUNT(*) FROM veda_outbox
WHERE event_type IN ('summary_sync', 'dir_summary_sync')
GROUP BY event_type, status;

-- Claimable now vs still gated by the staircase.
SELECT SUM(available_at <= UTC_TIMESTAMP()) AS claimable,
       SUM(available_at >  UTC_TIMESTAMP()) AS scheduled
FROM veda_outbox
WHERE event_type = 'summary_sync' AND status = 'pending';

-- Dead letters (expect 0). A rising count means the upstream 429 wall —
-- stop and lower @per_min rather than letting it drain into section 4.
SELECT COUNT(*) AS dead FROM veda_outbox
WHERE event_type IN ('summary_sync', 'dir_summary_sync') AND status = 'dead';

-- Empty-abstract sentinel (expect 0). An empty l0_abstract written as
-- ready is the exact shape of the 2026-07 incident (reasoning tokens ate
-- max_tokens, content came back empty with HTTP 200) — investigate before
-- continuing if it rises.
SELECT COUNT(*) AS empty_l0 FROM veda_summaries
WHERE file_id IS NOT NULL AND l0_abstract = '';

-- The DoD query for this backfill: PDF/Word files still without a summary.
-- Should fall to 0 as the queue drains (files with a stale extract are
-- excluded above and will be picked up by their own ExtractSync).
SELECT COUNT(*) AS pdf_word_without_summary
FROM veda_files f
JOIN veda_file_extracts fe
  ON fe.file_id = f.id AND fe.source_sha256 = f.checksum_sha256
LEFT JOIN veda_summaries s ON s.file_id = f.id
WHERE f.source_type IN ('pdf', 'word')
  AND f.storage_type = 'blob'
  AND s.file_id IS NULL;

-- ── 4. requeue dead letters (after a 429 storm) ───────────────────────
-- Re-staircase them instead of releasing all at once — releasing them
-- immediately is exactly the burst that killed them the first time.
SET @per_min := 4;
UPDATE veda_outbox o
JOIN (
    SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS rn
    FROM veda_outbox
    WHERE event_type IN ('summary_sync', 'dir_summary_sync')
      AND status = 'dead'
) t ON t.id = o.id
SET o.status = 'pending', o.retry_count = 0, o.lease_until = NULL,
    o.available_at = DATE_ADD(UTC_TIMESTAMP(), INTERVAL ((t.rn - 1) DIV @per_min) MINUTE);
