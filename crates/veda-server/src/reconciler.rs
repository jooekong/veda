//! MySQL ↔ Milvus drift reconciler.
//!
//! Periodically diffs the metadata source-of-truth (MySQL `veda_files` /
//! `veda_summaries`) against the vector store (Milvus chunk / summary
//! collections), then heals the divergence:
//!
//! - MySQL has, Milvus missing → enqueue ChunkSync / SummarySync (worker's
//!   `last_embedded_content_hash` short-circuit avoids redundant embed when
//!   the content actually matches).
//! - Milvus has, MySQL missing → orphan; delete from Milvus.
//!
//! Single-replica alpha lives inside the same process as the worker; the
//! design is intentionally split so a future `veda-reconciler` binary just
//! re-wires `Reconciler::new` and `run`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use tokio::sync::watch;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use veda_core::store::{AuthStore, MetadataStore, TaskQueue, VectorStore};
use veda_types::{OutboxEvent, OutboxEventType, OutboxStatus};

/// Per-workspace reconciliation outcome.
#[derive(Debug, Default, Clone)]
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

pub struct Reconciler {
    meta: Arc<dyn MetadataStore>,
    auth: Arc<dyn AuthStore>,
    vector: Arc<dyn VectorStore>,
    task_queue: Arc<dyn TaskQueue>,
    interval: Duration,
    /// Tracks orphans seen on previous passes. An orphan is only deleted
    /// after being observed on N+1 consecutive passes (where N =
    /// `grace_passes`). This avoids the non-atomic snapshot race: user
    /// writes a file between our MySQL read and our Milvus read, worker
    /// upserts to Milvus, we then see the file only on the Milvus side
    /// and would otherwise misclassify it as orphan.
    ///
    /// Counts saturate at `grace_passes + 1`. Tests that want immediate
    /// deletion can construct with `grace_passes=0`.
    orphan_seen: Mutex<HashMap<(String, String), usize>>,
    grace_passes: usize,
}

impl Reconciler {
    pub fn new(
        meta: Arc<dyn MetadataStore>,
        auth: Arc<dyn AuthStore>,
        vector: Arc<dyn VectorStore>,
        task_queue: Arc<dyn TaskQueue>,
        interval_secs: u64,
    ) -> Self {
        Self::with_grace_passes(meta, auth, vector, task_queue, interval_secs, 1)
    }

    pub fn with_grace_passes(
        meta: Arc<dyn MetadataStore>,
        auth: Arc<dyn AuthStore>,
        vector: Arc<dyn VectorStore>,
        task_queue: Arc<dyn TaskQueue>,
        interval_secs: u64,
        grace_passes: usize,
    ) -> Self {
        Self {
            meta,
            auth,
            vector,
            task_queue,
            interval: Duration::from_secs(interval_secs.max(60)),
            orphan_seen: Mutex::new(HashMap::new()),
            grace_passes,
        }
    }

    /// Periodic loop. Runs `run_once` on a fixed cadence; exits when the
    /// shutdown channel fires.
    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) {
        info!(
            interval_secs = self.interval.as_secs(),
            "reconciler started"
        );
        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    info!("reconciler shutting down");
                    break;
                }
                _ = sleep(self.interval) => {
                    if let Err(e) = self.run_once().await {
                        warn!(err = %e, "reconciler run failed");
                    }
                }
            }
        }
    }

    /// Run one reconciliation pass over every active workspace. Per-workspace
    /// errors are logged but do not abort the overall pass.
    pub async fn run_once(&self) -> veda_types::Result<ReconcileReport> {
        let workspace_ids = self.auth.list_active_workspace_ids().await?;
        let mut report = ReconcileReport::default();
        info!(
            workspace_count = workspace_ids.len(),
            "reconciler pass starting"
        );
        for ws in workspace_ids {
            match self.reconcile_workspace(&ws).await {
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
    ) -> veda_types::Result<WorkspaceReport> {
        let mut report = WorkspaceReport {
            workspace_id: workspace_id.to_string(),
            ..Default::default()
        };
        self.reconcile_chunks(workspace_id, &mut report).await?;
        self.reconcile_summaries(workspace_id, &mut report).await?;
        Ok(report)
    }

    /// Diff veda_files (MySQL) vs distinct file_ids in Milvus chunks.
    async fn reconcile_chunks(
        &self,
        workspace_id: &str,
        report: &mut WorkspaceReport,
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
            self.enqueue_chunk_sync_force(workspace_id, fid).await?;
            report.chunk_missing += 1;
        }

        // Orphan in Milvus: delete with two safety nets, in this order:
        //   1. Re-fetch MySQL state to handle "user wrote between our two
        //      reads" race (Codex finding #3). If the file is now present
        //      or there's a pending/processing ChunkSync for it, skip.
        //   2. Grace period: only delete on the second consecutive pass
        //      where this id is still orphaned. First-time orphans get
        //      recorded; we delete only if seen at least one full interval
        //      ago. This handles races that span multiple reconciler runs
        //      and operator races (e.g. someone mid-import).
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

        // Grace period: an orphan must be observed on `grace_passes + 1`
        // consecutive passes before deletion. Entries that no longer appear
        // orphan are dropped (their counter resets if they reappear later).
        let to_delete = self.advance_orphan_counter(
            workspace_id,
            "chunk:",
            &still_orphan,
        );
        for fid in to_delete {
            self.vector.delete_chunks(workspace_id, &fid).await?;
            report.chunk_orphan += 1;
        }
        Ok(())
    }

    /// Increment the orphan-seen counter for each id in `still_orphan` and
    /// return ids that have crossed the grace threshold. Entries no longer
    /// orphan are pruned. `kind_prefix` namespaces chunk vs summary orphans
    /// since the same id (e.g. file_id) can be tracked independently.
    fn advance_orphan_counter(
        &self,
        workspace_id: &str,
        kind_prefix: &'static str,
        still_orphan: &HashSet<String>,
    ) -> Vec<String> {
        let mut tracked = self.orphan_seen.lock().unwrap();
        // Drop any tracked entry of this kind in this workspace that is no
        // longer orphan — its counter must reset before the next time.
        tracked.retain(|(ws, key), _| {
            if ws != workspace_id || !key.starts_with(kind_prefix) {
                return true;
            }
            let id = key.trim_start_matches(kind_prefix);
            still_orphan.contains(id)
        });

        let mut to_delete = Vec::new();
        for id in still_orphan {
            let key = (workspace_id.to_string(), format!("{kind_prefix}{id}"));
            let count = tracked.entry(key.clone()).or_insert(0);
            *count += 1;
            if *count > self.grace_passes {
                to_delete.push(id.clone());
                tracked.remove(&key);
            } else {
                debug!(
                    workspace_id, id = %id, count = *count, grace_passes = self.grace_passes,
                    "orphan in grace period"
                );
            }
        }
        to_delete
    }

    /// Diff veda_summaries (MySQL) vs Milvus summary collection. Summary
    /// entity IDs are file_id (file summaries) or dentry_id (dir summaries);
    /// MySQL stores the same identifier as the entity ID.
    async fn reconcile_summaries(
        &self,
        workspace_id: &str,
        report: &mut WorkspaceReport,
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
                    self.enqueue_summary_sync(workspace_id, file_id).await?;
                    report.summary_missing += 1;
                }
                SummaryEntity::Dir {
                    dentry_id,
                    dentry_path,
                } => {
                    self.enqueue_dir_summary_sync(workspace_id, dentry_id, dentry_path)
                        .await?;
                    report.summary_missing += 1;
                }
            }
        }

        // Orphan in Milvus: same race + grace protection as chunks.
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
        let to_delete = self.advance_orphan_counter(
            workspace_id,
            "summary:",
            &still_orphan,
        );
        for id in to_delete {
            self.vector.delete_summary(workspace_id, &id).await?;
            report.summary_orphan += 1;
        }
        Ok(())
    }

    // ── Helpers ────────────────────────────────────────────

    /// All file_ids referenced by dentries in this workspace. We use dentries
    /// rather than veda_files directly because a file with ref_count > 1 may
    /// be referenced from multiple dentries — we still only need it embedded
    /// once.
    async fn list_mysql_file_ids(&self, workspace_id: &str) -> veda_types::Result<Vec<String>> {
        // Walk all dentries under "/" and collect non-null file_ids.
        let dentries = self.meta.list_dentries_under(workspace_id, "/").await?;
        let mut out: HashSet<String> = HashSet::new();
        for d in dentries {
            if let Some(fid) = d.file_id {
                out.insert(fid);
            }
        }
        Ok(out.into_iter().collect())
    }

    /// Walk ready summaries and return them tagged with their kind (file vs
    /// directory). The reconciler needs the kind to enqueue the right event
    /// (SummarySync for files, DirSummarySync for directories).
    async fn list_mysql_summary_entities(
        &self,
        workspace_id: &str,
    ) -> veda_types::Result<Vec<SummaryEntity>> {
        let dentries = self.meta.list_dentries_under(workspace_id, "/").await?;
        let mut out: Vec<SummaryEntity> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for d in dentries {
            if let Some(fid) = &d.file_id {
                if !seen.contains(fid) {
                    if let Some(s) = self.meta.get_summary_by_file(fid).await? {
                        if matches!(s.status, veda_types::SummaryStatus::Ready) {
                            out.push(SummaryEntity::File {
                                file_id: fid.clone(),
                            });
                            seen.insert(fid.clone());
                        }
                    }
                }
            }
            if d.is_dir {
                if let Some(s) = self.meta.get_summary_by_dentry(&d.id).await? {
                    if matches!(s.status, veda_types::SummaryStatus::Ready) {
                        out.push(SummaryEntity::Dir {
                            dentry_id: d.id.clone(),
                            dentry_path: d.path.clone(),
                        });
                    }
                }
            }
        }
        Ok(out)
    }

    /// Enqueue a ChunkSync with `force_reembed=true` payload, used by
    /// reconciler to repair Milvus-side data loss. The flag tells the worker
    /// to bypass the watermark short-circuit; without it, a "Milvus chunks
    /// gone but watermark says embedded" file would be reported as healed
    /// but never actually re-embedded.
    async fn enqueue_chunk_sync_force(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> veda_types::Result<()> {
        if self
            .task_queue
            .has_pending_event(
                OutboxEventType::ChunkSync,
                workspace_id,
                "file_id",
                file_id,
            )
            .await?
        {
            return Ok(());
        }
        let now = Utc::now();
        let event = OutboxEvent {
            id: 0,
            workspace_id: workspace_id.to_string(),
            event_type: OutboxEventType::ChunkSync,
            payload: serde_json::json!({
                "file_id": file_id,
                "force_reembed": true,
            }),
            status: OutboxStatus::Pending,
            retry_count: 0,
            max_retries: 3,
            available_at: now,
            lease_until: None,
            created_at: now,
        };
        self.task_queue.enqueue(&event).await?;
        Ok(())
    }

    async fn enqueue_dir_summary_sync(
        &self,
        workspace_id: &str,
        dentry_id: &str,
        dentry_path: &str,
    ) -> veda_types::Result<()> {
        if self
            .task_queue
            .has_pending_event(
                OutboxEventType::DirSummarySync,
                workspace_id,
                "dentry_id",
                dentry_id,
            )
            .await?
        {
            return Ok(());
        }
        let now = Utc::now();
        let event = OutboxEvent {
            id: 0,
            workspace_id: workspace_id.to_string(),
            event_type: OutboxEventType::DirSummarySync,
            payload: serde_json::json!({
                "dentry_id": dentry_id,
                "parent_path": dentry_path,
            }),
            status: OutboxStatus::Pending,
            retry_count: 0,
            max_retries: 3,
            available_at: now,
            lease_until: None,
            created_at: now,
        };
        self.task_queue.enqueue(&event).await?;
        Ok(())
    }

    async fn enqueue_summary_sync(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> veda_types::Result<()> {
        if self
            .task_queue
            .has_pending_event(
                OutboxEventType::SummarySync,
                workspace_id,
                "file_id",
                file_id,
            )
            .await?
        {
            return Ok(());
        }
        let now = Utc::now();
        let event = OutboxEvent {
            id: 0,
            workspace_id: workspace_id.to_string(),
            event_type: OutboxEventType::SummarySync,
            payload: serde_json::json!({"file_id": file_id}),
            status: OutboxStatus::Pending,
            retry_count: 0,
            max_retries: 3,
            available_at: now,
            lease_until: None,
            created_at: now,
        };
        self.task_queue.enqueue(&event).await?;
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
}
