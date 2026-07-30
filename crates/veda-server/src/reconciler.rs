//! MySQL ↔ Milvus drift reconciler.
//!
//! On-demand diff between the metadata source-of-truth (MySQL `veda_files` /
//! `veda_summaries`) and the vector store (Milvus chunk / summary
//! collections), then heals the divergence:
//!
//! - MySQL has, Milvus missing → enqueue ChunkSync / SummarySync (worker's
//!   `last_embedded_content_hash` short-circuit avoids redundant embed when
//!   the content actually matches).
//! - Milvus has, MySQL missing → orphan; delete from Milvus.
//!
//! Invoked on demand via `POST /admin/v1/reconcile/{workspace_id}`, NOT on a
//! timer. The file write and its ChunkSync/SummarySync enqueue commit in one
//! MySQL transaction (see `fs::FsService`), so the normal write path cannot
//! drift. The only residual drift sources are dead-letter tasks (surfaced by
//! `veda_outbox_dead_total`) and Milvus-side data loss (disk / ops / a
//! destructive schema migration) — rare enough that an operator running this
//! with `?dry_run=true` to inspect, then `?dry_run=false` to heal, beats a
//! 6-hourly background scan that silently skipped the largest workspaces.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::Utc;
use serde::Serialize;
use tracing::{debug, info, warn};

use veda_core::store::{AuthStore, MetadataStore, TaskQueue, VectorStore};
use veda_types::{Dentry, OutboxEventType, SourceType};

use crate::outbox::enqueue_dedup;

/// Per-workspace reconciliation outcome.
#[derive(Debug, Default, Clone, Serialize)]
pub struct WorkspaceReport {
    pub workspace_id: String,
    pub chunk_missing: usize,  // MySQL has, Milvus missing — enqueued ChunkSync
    pub chunk_orphan: usize,   // Milvus has, MySQL missing — deleted
    pub summary_missing: usize,
    pub summary_orphan: usize,
}

/// Aggregate outcome across all workspaces, returned by `run_once`.
#[derive(Debug, Default, Clone)]
pub struct ReconcileReport {
    pub workspaces: Vec<WorkspaceReport>,
}

impl ReconcileReport {
    pub fn total_drift(&self) -> usize {
        self.workspaces
            .iter()
            .map(|w| w.chunk_missing + w.chunk_orphan + w.summary_missing + w.summary_orphan)
            .sum()
    }
}

/// Identifies an entity that should exist in the Milvus summary collection.
/// File summaries use the file_id as the Milvus entity id; directory summaries
/// use the dentry_id. The reconciler must know which kind it is to enqueue
/// the correct event (SummarySync vs DirSummarySync).
#[derive(Debug, Clone)]
enum SummaryEntity {
    File { file_id: String },
    Dir { dentry_id: String, dentry_path: String },
}

impl SummaryEntity {
    fn entity_id(&self) -> &str {
        match self {
            Self::File { file_id } => file_id,
            Self::Dir { dentry_id, .. } => dentry_id,
        }
    }
}

/// Page size used by the reconciler when paging dentries. 1000 is a
/// good balance for MySQL: small enough to keep peak memory low (~200KB
/// of Dentry rows per page), large enough to amortize round-trip
/// latency across an entire workspace scan.
const DENTRY_PAGE_SIZE: usize = 1000;

/// Streaming variant of [`materialize_summary_entities`]. The
/// `seen_files` set persists across pages so a file_id referenced by
/// dentries in different pages still emits exactly one SummaryEntity.
/// Returns only the entities discovered in this page.
fn materialize_summary_entities_into(
    dentries: &[Dentry],
    ready_file_ids: &HashSet<String>,
    ready_dentry_ids: &HashSet<String>,
    seen_files: &mut HashSet<String>,
) -> Vec<SummaryEntity> {
    let mut out: Vec<SummaryEntity> = Vec::new();
    for d in dentries {
        if let Some(fid) = &d.file_id {
            if ready_file_ids.contains(fid) && !seen_files.contains(fid) {
                out.push(SummaryEntity::File {
                    file_id: fid.clone(),
                });
                seen_files.insert(fid.clone());
            }
        }
        if d.is_dir && ready_dentry_ids.contains(&d.id) {
            out.push(SummaryEntity::Dir {
                dentry_id: d.id.clone(),
                dentry_path: d.path.clone(),
            });
        }
    }
    out
}

/// Test-only single-page convenience wrapper. Production callers use
/// `materialize_summary_entities_into` to thread `seen_files` across
/// pages; tests that exercise the pure join semantics in one shot want
/// the simpler signature.
#[cfg(test)]
fn materialize_summary_entities(
    dentries: &[Dentry],
    ready_file_ids: &HashSet<String>,
    ready_dentry_ids: &HashSet<String>,
) -> Vec<SummaryEntity> {
    let mut seen_files: HashSet<String> = HashSet::new();
    materialize_summary_entities_into(dentries, ready_file_ids, ready_dentry_ids, &mut seen_files)
}

pub struct Reconciler {
    meta: Arc<dyn MetadataStore>,
    auth: Arc<dyn AuthStore>,
    vector: Arc<dyn VectorStore>,
    task_queue: Arc<dyn TaskQueue>,
}

impl Reconciler {
    pub fn new(
        meta: Arc<dyn MetadataStore>,
        auth: Arc<dyn AuthStore>,
        vector: Arc<dyn VectorStore>,
        task_queue: Arc<dyn TaskQueue>,
    ) -> Self {
        Self {
            meta,
            auth,
            vector,
            task_queue,
        }
    }

    /// Run one reconciliation pass over every active workspace. Per-workspace
    /// errors are logged but do not abort the overall pass. `dry_run` reports
    /// drift without enqueuing repairs or deleting orphans.
    pub async fn run_once(&self, dry_run: bool) -> veda_types::Result<ReconcileReport> {
        let workspace_ids = self.auth.list_active_workspace_ids().await?;
        let mut report = ReconcileReport::default();
        info!(
            workspace_count = workspace_ids.len(),
            "reconciler pass starting"
        );
        for ws in workspace_ids {
            match self.reconcile_workspace(&ws, dry_run).await {
                Ok(r) => {
                    // Per-workspace drift goes to logs, not metrics labels:
                    // workspace_id is high-cardinality and effectively a
                    // tenant identifier, neither of which belongs in a
                    // Prometheus label (Codex finding #2). Aggregated drift
                    // by kind is emitted at the end of the pass below.
                    if r.chunk_missing
                        + r.chunk_orphan
                        + r.summary_missing
                        + r.summary_orphan
                        > 0
                    {
                        info!(
                            workspace_id = %ws,
                            chunk_missing = r.chunk_missing,
                            chunk_orphan = r.chunk_orphan,
                            summary_missing = r.summary_missing,
                            summary_orphan = r.summary_orphan,
                            "reconciler healed drift"
                        );
                    } else {
                        debug!(workspace_id = %ws, "reconciler: clean");
                    }
                    report.workspaces.push(r);
                }
                Err(e) => {
                    warn!(workspace_id = %ws, err = %e, "reconciler workspace failed");
                }
            }
        }

        // Cluster-wide drift gauges: sum across workspaces, only `kind` as
        // a label. Operators investigating "which workspace?" follow the
        // structured logs above (workspace_id is in tracing fields). This
        // intentionally trades workspace-level metric attribution for
        // bounded label cardinality and tenant privacy.
        let mut chunk_missing = 0u64;
        let mut chunk_orphan = 0u64;
        let mut summary_missing = 0u64;
        let mut summary_orphan = 0u64;
        for w in &report.workspaces {
            chunk_missing += w.chunk_missing as u64;
            chunk_orphan += w.chunk_orphan as u64;
            summary_missing += w.summary_missing as u64;
            summary_orphan += w.summary_orphan as u64;
        }
        ::metrics::gauge!("veda_drift_total", "kind" => "chunk_missing")
            .set(chunk_missing as f64);
        ::metrics::gauge!("veda_drift_total", "kind" => "chunk_orphan")
            .set(chunk_orphan as f64);
        ::metrics::gauge!("veda_drift_total", "kind" => "summary_missing")
            .set(summary_missing as f64);
        ::metrics::gauge!("veda_drift_total", "kind" => "summary_orphan")
            .set(summary_orphan as f64);
        info!(
            workspaces = report.workspaces.len(),
            total_drift = report.total_drift(),
            "reconciler pass complete"
        );
        Ok(report)
    }

    /// Run reconciliation for a single workspace. Public so tests sharing a
    /// MySQL/Milvus instance can scope each test to their own workspace
    /// without other parallel tests' transient drift confusing this run.
    /// Production callers use `run_once` to iterate all active workspaces.
    pub async fn reconcile_workspace(
        &self,
        workspace_id: &str,
        dry_run: bool,
    ) -> veda_types::Result<WorkspaceReport> {
        let mut report = WorkspaceReport {
            workspace_id: workspace_id.to_string(),
            ..Default::default()
        };
        self.reconcile_chunks(workspace_id, &mut report, dry_run)
            .await?;
        self.reconcile_summaries(workspace_id, &mut report, dry_run)
            .await?;
        Ok(report)
    }

    /// Diff veda_files (MySQL) vs distinct file_ids in Milvus chunks.
    async fn reconcile_chunks(
        &self,
        workspace_id: &str,
        report: &mut WorkspaceReport,
        dry_run: bool,
    ) -> veda_types::Result<()> {
        let mysql_ids: HashSet<String> = self
            .list_mysql_file_ids(workspace_id)
            .await?
            .into_iter()
            .collect();
        let milvus_ids: HashSet<String> = self
            .vector
            .list_chunk_file_ids(workspace_id)
            .await?
            .into_iter()
            .collect();

        // Missing in Milvus: enqueue a ChunkSync with force_reembed=true.
        // The watermark short-circuit (W1.3) is correct relative to "did we
        // ever finish embedding" but cannot detect Milvus-side data loss.
        // For reconciler-driven repairs we explicitly bypass the watermark
        // so the worker actually rebuilds the chunks.
        for fid in mysql_ids.difference(&milvus_ids) {
            // Route the repair by source type. Image/binary blobs are never
            // indexed, so their absence from Milvus is expected — skip them
            // (otherwise the worker would dead-letter a ChunkSync it can't run).
            // PDFs/Word docs are indexed via ExtractSync, text via ChunkSync.
            let event_type = match self.meta.get_file(fid).await? {
                Some(f) => match f.source_type {
                    SourceType::Text => OutboxEventType::ChunkSync,
                    SourceType::Pdf | SourceType::Word => OutboxEventType::ExtractSync,
                    SourceType::Image | SourceType::Binary => continue,
                },
                None => continue, // file vanished since the snapshot
            };
            if dry_run {
                info!(workspace_id, file_id = %fid, ?event_type, "dry-run: would enqueue repair (missing in Milvus)");
            } else {
                self.enqueue_index_force(workspace_id, fid, event_type).await?;
            }
            report.chunk_missing += 1;
        }

        // Orphan in Milvus: delete guarded by an in-pass re-check — re-fetch
        // MySQL state to handle the "user wrote between our two reads" race
        // (Codex finding #3). If the file is now present or there's a
        // queued ChunkSync for it (has_pending_event checks status
        // 'pending' only; an in-flight 'processing' one is a residual race
        // window accepted for this pass), skip. (Reconcile is an
        // attended, on-demand admin action, so a single-pass re-check is the
        // whole race window we need to cover.)
        let mut still_orphan: HashSet<String> = HashSet::new();
        for fid in milvus_ids.difference(&mysql_ids) {
            // Re-confirm MySQL state.
            if self.meta.get_file(fid).await?.is_some() {
                debug!(
                    workspace_id, file_id = %fid,
                    "skipping chunk orphan delete: file reappeared in MySQL"
                );
                continue;
            }
            // Re-confirm no in-flight ChunkSync.
            if self
                .task_queue
                .has_pending_event(
                    OutboxEventType::ChunkSync,
                    workspace_id,
                    "file_id",
                    fid,
                )
                .await?
            {
                debug!(
                    workspace_id, file_id = %fid,
                    "skipping chunk orphan delete: pending ChunkSync exists"
                );
                continue;
            }
            still_orphan.insert(fid.clone());
        }

        if dry_run {
            for fid in &still_orphan {
                info!(workspace_id, file_id = %fid, "dry-run: would delete chunk orphan (in Milvus, gone from MySQL)");
            }
            report.chunk_orphan += still_orphan.len();
        } else {
            for fid in still_orphan {
                self.vector.delete_chunks(workspace_id, &fid).await?;
                report.chunk_orphan += 1;
            }
        }
        Ok(())
    }

    /// Diff veda_summaries (MySQL) vs Milvus summary collection. Summary
    /// entity IDs are file_id (file summaries) or dentry_id (dir summaries);
    /// MySQL stores the same identifier as the entity ID.
    async fn reconcile_summaries(
        &self,
        workspace_id: &str,
        report: &mut WorkspaceReport,
        dry_run: bool,
    ) -> veda_types::Result<()> {
        let mysql_entities = self.list_mysql_summary_entities(workspace_id).await?;
        let mysql_id_set: HashSet<String> = mysql_entities
            .iter()
            .map(|e| e.entity_id().to_string())
            .collect();
        let milvus_ids: HashSet<String> = self
            .vector
            .list_summary_ids(workspace_id)
            .await?
            .into_iter()
            .collect();

        // Missing in Milvus: enqueue per entity type. Dir summaries do NOT
        // cascade from file SummarySync — the worker only triggers a
        // DirSummarySync when a child file's SummarySync runs. If the only
        // drift is a missing dir summary (no child file changes), nothing
        // would re-create it without an explicit DirSummarySync here.
        for entity in &mysql_entities {
            if milvus_ids.contains(entity.entity_id()) {
                continue;
            }
            match entity {
                SummaryEntity::File { file_id } => {
                    if dry_run {
                        info!(workspace_id, file_id = %file_id, "dry-run: would enqueue SummarySync (missing in Milvus)");
                    } else {
                        self.enqueue_summary_sync(workspace_id, file_id).await?;
                    }
                    report.summary_missing += 1;
                }
                SummaryEntity::Dir {
                    dentry_id,
                    dentry_path,
                } => {
                    if dry_run {
                        info!(workspace_id, dentry_id = %dentry_id, "dry-run: would enqueue DirSummarySync (missing in Milvus)");
                    } else {
                        self.enqueue_dir_summary_sync(workspace_id, dentry_id, dentry_path)
                            .await?;
                    }
                    report.summary_missing += 1;
                }
            }
        }

        // Orphan in Milvus: same in-pass re-check protection as chunks.
        let mut still_orphan: HashSet<String> = HashSet::new();
        for id in milvus_ids.difference(&mysql_id_set) {
            // Re-confirm: MySQL may have caught up between our two reads.
            if self.meta.get_summary_by_file(id).await?.is_some()
                || self.meta.get_summary_by_dentry(id).await?.is_some()
            {
                debug!(
                    workspace_id, id = %id,
                    "skipping summary orphan delete: summary reappeared in MySQL"
                );
                continue;
            }
            still_orphan.insert(id.clone());
        }
        if dry_run {
            for id in &still_orphan {
                info!(workspace_id, id = %id, "dry-run: would delete summary orphan (in Milvus, gone from MySQL)");
            }
            report.summary_orphan += still_orphan.len();
        } else {
            for id in still_orphan {
                self.vector.delete_summary(workspace_id, &id).await?;
                report.summary_orphan += 1;
            }
        }
        Ok(())
    }

    // ── Helpers ────────────────────────────────────────────

    /// All file_ids referenced by dentries in this workspace. We use dentries
    /// rather than veda_files directly because a file with ref_count > 1 may
    /// be referenced from multiple dentries — we still only need it embedded
    /// once.
    ///
    /// Streamed via paginated dentry queries so memory stays O(unique
    /// file_ids) regardless of workspace size — full-table SELECT used to
    /// materialize every Dentry row before extracting file_id (review C2).
    async fn list_mysql_file_ids(&self, workspace_id: &str) -> veda_types::Result<Vec<String>> {
        let mut out: HashSet<String> = HashSet::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self
                .meta
                .list_dentries_under_page(
                    workspace_id,
                    "/",
                    cursor.as_deref(),
                    DENTRY_PAGE_SIZE,
                )
                .await?;
            let n = page.len();
            for d in &page {
                if let Some(fid) = &d.file_id {
                    out.insert(fid.clone());
                }
            }
            if let Some(last) = page.last() {
                cursor = Some(last.path.clone());
            }
            if n < DENTRY_PAGE_SIZE {
                break;
            }
        }
        Ok(out.into_iter().collect())
    }

    /// Walk ready summaries and return them tagged with their kind (file vs
    /// directory). The reconciler needs the kind to enqueue the right event
    /// (SummarySync for files, DirSummarySync for directories).
    ///
    /// Was N+1: 1 dentry full-table SELECT + N per-row get_summary_by_*
    /// lookups. Now: paginated dentry pages + 1 batch summary-keys query
    /// + in-memory join, with peak memory bounded by `DENTRY_PAGE_SIZE`
    /// instead of total workspace size (review C2 + C8).
    async fn list_mysql_summary_entities(
        &self,
        workspace_id: &str,
    ) -> veda_types::Result<Vec<SummaryEntity>> {
        let (ready_file_ids, ready_dentry_ids) =
            self.meta.list_ready_summary_keys(workspace_id).await?;
        let mut out: Vec<SummaryEntity> = Vec::new();
        let mut seen_files: HashSet<String> = HashSet::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self
                .meta
                .list_dentries_under_page(
                    workspace_id,
                    "/",
                    cursor.as_deref(),
                    DENTRY_PAGE_SIZE,
                )
                .await?;
            let n = page.len();
            // Stream-merge: accumulate entities without ever holding all
            // dentries in memory.
            for entity in materialize_summary_entities_into(
                &page,
                &ready_file_ids,
                &ready_dentry_ids,
                &mut seen_files,
            ) {
                out.push(entity);
            }
            if let Some(last) = page.last() {
                cursor = Some(last.path.clone());
            }
            if n < DENTRY_PAGE_SIZE {
                break;
            }
        }
        Ok(out)
    }

    /// Enqueue a ChunkSync with `force_reembed=true` payload, used by
    /// reconciler to repair Milvus-side data loss. The flag tells the worker
    /// to bypass the watermark short-circuit; without it, a "Milvus chunks
    /// gone but watermark says embedded" file would be reported as healed
    /// but never actually re-embedded.
    async fn enqueue_index_force(
        &self,
        workspace_id: &str,
        file_id: &str,
        event_type: OutboxEventType,
    ) -> veda_types::Result<()> {
        enqueue_dedup(
            &*self.task_queue,
            workspace_id,
            event_type,
            "file_id",
            file_id,
            serde_json::json!({
                "file_id": file_id,
                "force_reembed": true,
            }),
            Utc::now(),
        )
        .await?;
        Ok(())
    }

    async fn enqueue_dir_summary_sync(
        &self,
        workspace_id: &str,
        dentry_id: &str,
        dentry_path: &str,
    ) -> veda_types::Result<()> {
        enqueue_dedup(
            &*self.task_queue,
            workspace_id,
            OutboxEventType::DirSummarySync,
            "dentry_id",
            dentry_id,
            serde_json::json!({
                "dentry_id": dentry_id,
                "parent_path": dentry_path,
            }),
            Utc::now(),
        )
        .await?;
        Ok(())
    }

    async fn enqueue_summary_sync(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> veda_types::Result<()> {
        enqueue_dedup(
            &*self.task_queue,
            workspace_id,
            OutboxEventType::SummarySync,
            "file_id",
            file_id,
            serde_json::json!({"file_id": file_id}),
            Utc::now(),
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_total_drift_sums_all_categories() {
        let report = ReconcileReport {
            workspaces: vec![
                WorkspaceReport {
                    workspace_id: "ws1".into(),
                    chunk_missing: 2,
                    chunk_orphan: 1,
                    summary_missing: 0,
                    summary_orphan: 3,
                },
                WorkspaceReport {
                    workspace_id: "ws2".into(),
                    chunk_missing: 5,
                    chunk_orphan: 0,
                    summary_missing: 1,
                    summary_orphan: 0,
                },
            ],
        };
        assert_eq!(report.total_drift(), 2 + 1 + 0 + 3 + 5 + 0 + 1 + 0);
    }

    #[test]
    fn report_zero_drift_when_clean() {
        let report = ReconcileReport {
            workspaces: vec![WorkspaceReport {
                workspace_id: "ws1".into(),
                ..Default::default()
            }],
        };
        assert_eq!(report.total_drift(), 0);
    }

    /// The bidirectional diff is implemented via `HashSet::difference`. This
    /// asserts the set semantics we depend on (std's invariants), guarding
    /// against accidental refactors that switch to an algorithm with
    /// different boundary behavior (e.g. Vec ordering games).
    #[test]
    fn hashset_difference_yields_correct_partitions() {
        let mysql: HashSet<&str> = ["a", "b", "c"].into_iter().collect();
        let milvus: HashSet<&str> = ["b", "c", "d"].into_iter().collect();

        let missing: Vec<&&str> = mysql.difference(&milvus).collect();
        let orphan: Vec<&&str> = milvus.difference(&mysql).collect();

        assert_eq!(missing.len(), 1);
        assert!(missing.contains(&&"a"));

        assert_eq!(orphan.len(), 1);
        assert!(orphan.contains(&&"d"));
    }

    fn dentry(id: &str, path: &str, is_dir: bool, file_id: Option<&str>) -> Dentry {
        Dentry {
            id: id.to_string(),
            workspace_id: "ws".to_string(),
            parent_path: "/".to_string(),
            name: path.trim_start_matches('/').to_string(),
            path: path.to_string(),
            is_dir,
            file_id: file_id.map(|s| s.to_string()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn materialize_emits_file_only_when_summary_ready() {
        let dentries = vec![
            dentry("d1", "/a.md", false, Some("f1")),
            dentry("d2", "/b.md", false, Some("f2")),
        ];
        let ready_files: HashSet<String> = ["f1".into()].into_iter().collect();
        let ready_dirs: HashSet<String> = HashSet::new();

        let out = materialize_summary_entities(&dentries, &ready_files, &ready_dirs);

        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], SummaryEntity::File { file_id } if file_id == "f1"));
    }

    #[test]
    fn materialize_dedupes_files_with_ref_count_gt_1() {
        // Same file_id linked from two dentries — should yield only one entity.
        let dentries = vec![
            dentry("d1", "/copy1.md", false, Some("shared-f")),
            dentry("d2", "/copy2.md", false, Some("shared-f")),
        ];
        let ready_files: HashSet<String> = ["shared-f".into()].into_iter().collect();
        let ready_dirs: HashSet<String> = HashSet::new();

        let out = materialize_summary_entities(&dentries, &ready_files, &ready_dirs);
        assert_eq!(out.len(), 1, "deduped on file_id");
    }

    #[test]
    fn materialize_emits_dir_when_dentry_ready() {
        let dentries = vec![
            dentry("d-root-docs", "/docs", true, None),
            dentry("d-readme", "/docs/readme.md", false, Some("f1")),
        ];
        let ready_files: HashSet<String> = HashSet::new();
        let ready_dirs: HashSet<String> = ["d-root-docs".into()].into_iter().collect();

        let out = materialize_summary_entities(&dentries, &ready_files, &ready_dirs);

        assert_eq!(out.len(), 1);
        match &out[0] {
            SummaryEntity::Dir { dentry_id, dentry_path } => {
                assert_eq!(dentry_id, "d-root-docs");
                assert_eq!(dentry_path, "/docs");
            }
            _ => panic!("expected Dir entity"),
        }
    }

    #[test]
    fn materialize_skips_files_without_ready_summary() {
        // file_id present in dentry but NOT in ready_files → skipped.
        let dentries = vec![dentry("d1", "/a.md", false, Some("f1"))];
        let ready_files: HashSet<String> = HashSet::new();
        let ready_dirs: HashSet<String> = HashSet::new();

        let out = materialize_summary_entities(&dentries, &ready_files, &ready_dirs);
        assert!(out.is_empty());
    }

    #[test]
    fn materialize_handles_files_and_dirs_in_one_pass() {
        let dentries = vec![
            dentry("d-docs", "/docs", true, None),
            dentry("d-readme", "/docs/readme.md", false, Some("f1")),
            dentry("d-other", "/other.md", false, Some("f2")),
        ];
        let ready_files: HashSet<String> =
            ["f1".into(), "f2".into()].into_iter().collect();
        let ready_dirs: HashSet<String> = ["d-docs".into()].into_iter().collect();

        let out = materialize_summary_entities(&dentries, &ready_files, &ready_dirs);
        assert_eq!(out.len(), 3);

        let ids: HashSet<&str> = out.iter().map(|e| e.entity_id()).collect();
        assert_eq!(ids, ["f1", "f2", "d-docs"].into_iter().collect());
    }

    #[test]
    fn materialize_empty_inputs_yield_empty_output() {
        let out =
            materialize_summary_entities(&[], &HashSet::new(), &HashSet::new());
        assert!(out.is_empty());
    }

    /// When dentries are split across paginated calls, a file_id
    /// referenced from dentries in different pages must STILL emit only
    /// one SummaryEntity. The shared `seen_files` set is what keeps the
    /// invariant across pages.
    #[test]
    fn materialize_into_dedupes_files_across_pages() {
        let ready_files: HashSet<String> = ["shared".into()].into_iter().collect();
        let ready_dirs: HashSet<String> = HashSet::new();
        let mut seen: HashSet<String> = HashSet::new();

        let page1 = vec![dentry("d1", "/a/copy.md", false, Some("shared"))];
        let page2 = vec![dentry("d2", "/b/copy.md", false, Some("shared"))];

        let out1 = materialize_summary_entities_into(&page1, &ready_files, &ready_dirs, &mut seen);
        let out2 = materialize_summary_entities_into(&page2, &ready_files, &ready_dirs, &mut seen);

        assert_eq!(out1.len(), 1, "file emitted on first page");
        assert!(out2.is_empty(), "same file suppressed on second page");
        assert_eq!(seen.len(), 1);
    }
}
