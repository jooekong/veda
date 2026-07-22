-- Backfill for the Word-document support + stored-extract feature.
-- Run ONCE per node after deploying the version that adds
-- veda_file_extracts (idempotent: safe to re-run, duplicate ExtractSync
-- events are deduped by the worker's watermark + extract-freshness check).
--
--   mysql -h <host> -u <user> -p <db> < scripts/backfill-word-extracts.sql
--
-- 1) Existing Word blobs were classified source_type='binary' before this
--    release (never indexed). Reclassify them so the reconciler and worker
--    route them to ExtractSync.
UPDATE veda_files
SET source_type = 'word'
WHERE source_type = 'binary'
  AND mime_type IN (
    'application/vnd.openxmlformats-officedocument.wordprocessingml.document',
    'application/msword',
    'application/x-ole-storage'
  );

-- 2) Enqueue ExtractSync for every extractable blob (word + pdf) whose
--    stored extract is missing or stale. Covers both the just-reclassified
--    Word files (full extract + embed) and pre-existing PDFs (extract-only
--    top-up: the worker sees the embed watermark is current and skips
--    re-embedding). Skips files that already have a pending/processing
--    ExtractSync.
INSERT INTO veda_outbox (workspace_id, event_type, payload, status)
SELECT f.workspace_id, 'extract_sync', JSON_OBJECT('file_id', f.id), 'pending'
FROM veda_files f
LEFT JOIN veda_file_extracts fe ON fe.file_id = f.id
WHERE f.source_type IN ('word', 'pdf')
  AND f.storage_type = 'blob'
  AND (fe.file_id IS NULL OR fe.source_sha256 <> f.checksum_sha256)
  AND NOT EXISTS (
    SELECT 1 FROM veda_outbox o
    WHERE o.event_type = 'extract_sync'
      AND o.status IN ('pending', 'processing')
      AND o.payload->>'$.file_id' = f.id
  );

-- Progress check afterwards:
--   SELECT status, COUNT(*) FROM veda_outbox
--     WHERE event_type='extract_sync' GROUP BY status;
--   SELECT COUNT(*) FROM veda_files f LEFT JOIN veda_file_extracts fe
--     ON fe.file_id=f.id WHERE f.source_type IN ('word','pdf')
--     AND f.storage_type='blob' AND fe.file_id IS NULL;
