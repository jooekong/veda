//! Shared outbox enqueue helper for worker + reconciler.
//!
//! Both modules need the same "insert if no duplicate pending/processing
//! event exists" semantics. Keeping the OutboxEvent construction in one
//! place ensures the retry budget, status, and column defaults stay in
//! sync — past drift between these call sites is exactly what this
//! helper exists to prevent.

use chrono::{DateTime, Utc};

use veda_core::store::TaskQueue;
use veda_types::{OutboxEvent, OutboxEventType, OutboxStatus, Result};

/// Insert an outbox event, deduping against any pending/processing entry
/// keyed by `(event_type, workspace_id, dedup_field, dedup_value)`.
/// Returns `true` if a new event was inserted, `false` if a duplicate
/// already existed and the call was a no-op.
///
/// `available_at` lets callers schedule the work for the future (used by
/// the worker's burst debounce); reconciler passes `Utc::now()` for
/// immediate visibility. Retry budget is fixed at 3 — every current
/// caller uses that value.
pub async fn enqueue_dedup(
    task_queue: &dyn TaskQueue,
    workspace_id: &str,
    event_type: OutboxEventType,
    dedup_field: &str,
    dedup_value: &str,
    payload: serde_json::Value,
    available_at: DateTime<Utc>,
) -> Result<bool> {
    if task_queue
        .has_pending_event(event_type, workspace_id, dedup_field, dedup_value)
        .await?
    {
        return Ok(false);
    }
    let now = Utc::now();
    task_queue
        .enqueue(&OutboxEvent {
            id: 0,
            workspace_id: workspace_id.to_string(),
            event_type,
            payload,
            status: OutboxStatus::Pending,
            retry_count: 0,
            max_retries: 3,
            available_at,
            lease_until: None,
            created_at: now,
        })
        .await?;
    Ok(true)
}
