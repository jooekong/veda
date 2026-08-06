use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, FixedOffset, NaiveDate, Utc};
use tracing::warn;
use veda_types::Result;

use crate::store::{DocAccessRow, MetadataStore};

/// In-process aggregation of per-document access counts, flushed to
/// `veda_doc_access_daily` on a timer. Heat metrics are best-effort by
/// design: a failed flush drops that window's deltas (same semantic tier as
/// crash loss) rather than retrying — retries can't be made exactly-once
/// once a commit outcome is unknown, and double counts are worse than a
/// missing 30s window.
///
/// Recording is a mutex-guarded HashMap bump — no I/O, no await. Callers on
/// the search/read hot paths pay nanoseconds. Single-writer deployment is a
/// standing architectural constraint, so one process owns all counting.
pub struct AccessRecorder {
    enabled: bool,
    /// Fixed offset for the day boundary (default +08:00). Deliberately a
    /// config value, not process TZ: TZ drifts between boxes and CI, and
    /// this project has been bitten by implicit-timezone bugs before.
    day_offset: FixedOffset,
    buf: Mutex<HashMap<BufKey, Counts>>,
    meta: Arc<dyn MetadataStore>,
}

/// (workspace_id, day, dentry_id)
type BufKey = (String, NaiveDate, String);

#[derive(Default, Clone, Copy)]
struct Counts {
    search_hits: u64,
    reads: u64,
}

impl AccessRecorder {
    /// `enabled = false` turns recording into a no-op while queries (and
    /// the day boundary they use) keep working — the kill switch must not
    /// hide data already accumulated in the table.
    pub fn new(meta: Arc<dyn MetadataStore>, day_utc_offset_hours: i8, enabled: bool) -> Self {
        let day_offset = FixedOffset::east_opt(i32::from(day_utc_offset_hours) * 3600)
            .unwrap_or_else(|| FixedOffset::east_opt(0).expect("UTC offset"));
        Self {
            enabled,
            day_offset,
            buf: Mutex::new(HashMap::new()),
            meta,
        }
    }

    /// No-op recorder. Used for surfaces exempt from heat counting (the SQL
    /// engine gets an FsService built with this) and for tests.
    pub fn disabled(meta: Arc<dyn MetadataStore>) -> Self {
        Self::new(meta, 0, false)
    }

    /// Heat ranking over the last `days` days (inclusive of today under the
    /// recorder's day boundary, so the window matches how rows were
    /// bucketed at record time).
    pub async fn query(
        &self,
        workspace_id: &str,
        days: u32,
        order: crate::store::DocAccessOrder,
        limit: usize,
    ) -> Result<Vec<veda_types::api::DocAccessEntry>> {
        let since = self.today() - chrono::Days::new(u64::from(days.saturating_sub(1)));
        self.meta
            .query_doc_access(workspace_id, since, order, limit)
            .await
    }

    pub fn record_read(&self, workspace_id: &str, dentry_id: &str) {
        self.record_read_at(workspace_id, dentry_id, Utc::now());
    }

    pub fn record_search_hits(&self, workspace_id: &str, dentry_ids: &[String]) {
        self.record_search_hits_at(workspace_id, dentry_ids, Utc::now());
    }

    /// Clock-injected variant for tests (day-boundary assertions need a
    /// pinned `now`). Not part of the public contract.
    #[doc(hidden)]
    pub fn record_read_at(&self, workspace_id: &str, dentry_id: &str, now: DateTime<Utc>) {
        if !self.enabled {
            return;
        }
        let day = self.day_of(now);
        let mut buf = self.buf.lock().expect("access buf poisoned");
        let c = buf
            .entry((workspace_id.to_string(), day, dentry_id.to_string()))
            .or_default();
        c.reads = c.reads.saturating_add(1);
    }

    /// Clock-injected variant for tests. Not part of the public contract.
    #[doc(hidden)]
    pub fn record_search_hits_at(
        &self,
        workspace_id: &str,
        dentry_ids: &[String],
        now: DateTime<Utc>,
    ) {
        if !self.enabled || dentry_ids.is_empty() {
            return;
        }
        let day = self.day_of(now);
        let mut buf = self.buf.lock().expect("access buf poisoned");
        for id in dentry_ids {
            let c = buf
                .entry((workspace_id.to_string(), day, id.clone()))
                .or_default();
            c.search_hits = c.search_hits.saturating_add(1);
        }
    }

    fn day_of(&self, now: DateTime<Utc>) -> NaiveDate {
        now.with_timezone(&self.day_offset).date_naive()
    }

    /// Drain the buffer into MySQL. Returns rows flushed. On store error the
    /// batch is dropped (see type-level comment) — counted in
    /// `veda_doc_access_dropped_total` so silent loss stays visible.
    pub async fn flush(&self) -> Result<usize> {
        let drained: Vec<DocAccessRow> = {
            let mut buf = self.buf.lock().expect("access buf poisoned");
            std::mem::take(&mut *buf)
                .into_iter()
                .map(|((ws, day, dentry_id), c)| DocAccessRow {
                    workspace_id: ws,
                    day,
                    dentry_id,
                    search_hits: c.search_hits,
                    reads: c.reads,
                })
                .collect()
        };
        if drained.is_empty() {
            return Ok(0);
        }
        let n = drained.len();
        let started = std::time::Instant::now();
        match self.meta.upsert_doc_access_daily(&drained).await {
            Ok(()) => {
                ::metrics::histogram!("veda_doc_access_flush_seconds", "outcome" => "ok")
                    .record(started.elapsed().as_secs_f64());
                ::metrics::counter!("veda_doc_access_flushed_rows_total").increment(n as u64);
                Ok(n)
            }
            Err(e) => {
                ::metrics::histogram!("veda_doc_access_flush_seconds", "outcome" => "error")
                    .record(started.elapsed().as_secs_f64());
                ::metrics::counter!("veda_doc_access_dropped_total").increment(n as u64);
                warn!(err = %e, rows = n, "doc access flush failed; window dropped");
                Err(e)
            }
        }
    }

    /// Delete stats rows older than `retention_days` counting back from
    /// today's day boundary. The server flush loop calls this once per day.
    pub async fn sweep(&self, retention_days: u32) -> Result<u64> {
        let cutoff = self.day_of(Utc::now()) - chrono::Days::new(u64::from(retention_days));
        let n = self.meta.sweep_doc_access(cutoff).await?;
        if n > 0 {
            ::metrics::counter!("veda_doc_access_retention_swept_total").increment(n);
        }
        Ok(n)
    }

    /// Today under the recorder's day boundary — the query route uses this
    /// so "last N days" windows agree with how rows were bucketed.
    pub fn today(&self) -> NaiveDate {
        self.day_of(Utc::now())
    }
}
