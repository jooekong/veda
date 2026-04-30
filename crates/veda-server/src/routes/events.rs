use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures::stream::Stream;
use serde::Deserialize;

use crate::auth::AuthWorkspace;
use crate::state::AppState;

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const BATCH_SIZE: usize = 100;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/v1/events", get(sse_events))
}

#[derive(Deserialize, Default)]
struct EventQuery {
    since_id: Option<i64>,
    /// Optional server-side path filter — only events whose `path` starts
    /// with this prefix are streamed. Empty / missing means "everything".
    /// Validated as an absolute, normalized path before use.
    path_prefix: Option<String>,
}

async fn sse_events(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Query(q): Query<EventQuery>,
) -> Response {
    let workspace_id = auth.workspace_id.clone();
    let cursor_init = q.since_id.unwrap_or(0);

    // 410 gate: a non-zero cursor older than MIN(id) means the client slept
    // past retention. We tell them the new floor so they can resync via
    // list_dir then resubscribe with the returned `current_min_id`. cursor=0
    // is the legitimate "fresh subscription" case and never returns 410.
    //
    // We also fetch MAX(id) at the same point so the 410 body carries an
    // unambiguous resync cursor: client should call list_dir then resubscribe
    // with `since_id = current_max_id`, which closes the race window where
    // events arrive between list_dir and resubscribe.
    if cursor_init > 0 {
        let min_id = match state.fs_service.events_min_id(&workspace_id).await {
            Ok(v) => v,
            Err(e) => {
                // Fail closed. A transient DB error here would otherwise let a
                // stale cursor silently consume the post-retention window,
                // dropping events the client thinks they've seen. 503 forces
                // them to retry — they'll either get a 410 with the correct
                // floor or a clean stream once the DB is back.
                tracing::warn!(err = %e, workspace_id, "events_min_id failed; returning 503");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({ "error": "events_min_id lookup failed" })),
                )
                    .into_response();
            }
        };
        if let Some(min_id) = min_id {
            if cursor_init < min_id {
                let max_id = state
                    .fs_service
                    .events_max_id(&workspace_id)
                    .await
                    .ok()
                    .flatten();
                return build_410_response(min_id, max_id);
            }
        }
    }

    // Validate path_prefix at the boundary. An invalid prefix is a hard 400 —
    // silently widening to "all events" would be surprising for the caller.
    let path_prefix: Option<String> = match q.path_prefix.as_deref() {
        None => None,
        Some(s) if s.is_empty() => None,
        Some(s) => match veda_core::path::normalize(s) {
            Ok(n) => Some(n),
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": "invalid path_prefix" })),
                )
                    .into_response();
            }
        },
    };

    let mut cursor = cursor_init;
    let stream = async_stream::stream! {
        let mut backoff = POLL_INTERVAL;
        loop {
            match state
                .fs_service
                .query_events_filtered(&workspace_id, cursor, path_prefix.as_deref(), BATCH_SIZE)
                .await
            {
                Ok(events) => {
                    backoff = POLL_INTERVAL;
                    for event in events {
                        cursor = event.id;
                        let data = serde_json::json!({
                            "id": event.id,
                            "event_type": event.event_type.as_str(),
                            "path": event.path,
                            "file_id": event.file_id,
                        });
                        yield Ok::<_, Infallible>(Event::default()
                            .id(event.id.to_string())
                            .data(data.to_string()));
                    }
                }
                Err(e) => {
                    tracing::warn!(err = %e, backoff_ms = backoff.as_millis(), "SSE poll error");
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
            tokio::time::sleep(backoff).await;
        }
    };

    let stream: std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        Box::pin(stream);
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
        .into_response()
}

/// Build the HTTP 410 body returned when an SSE client's cursor has fallen
/// off the retention window. Extracted so the contract can be unit-tested
/// without spinning a full router; the body shape is part of the public
/// SSE protocol (FUSE / CLI clients parse `current_min_id` and
/// `current_max_id`), so changes here need a coordinated client release.
fn build_410_response(current_min_id: i64, current_max_id: Option<i64>) -> Response {
    (
        StatusCode::GONE,
        Json(serde_json::json!({
            "error": "cursor expired",
            "current_min_id": current_min_id,
            "current_max_id": current_max_id,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_size_is_modest_to_keep_first_byte_low() {
        // Larger batch = lower throughput on a slow client because we'd hold
        // the read transaction longer per loop. Pin this to catch silent bumps.
        assert_eq!(BATCH_SIZE, 100);
    }

    #[test]
    fn poll_interval_smaller_than_keepalive() {
        // SSE keep-alive fires every 30s; if our poll interval ever exceeded
        // that the keep-alive would arrive before we'd noticed new events.
        assert!(POLL_INTERVAL < Duration::from_secs(30));
    }

    fn parse_query(q: &str) -> EventQuery {
        serde_urlencoded::from_str(q).unwrap()
    }

    #[test]
    fn parses_optional_query_fields() {
        let q = parse_query("");
        assert_eq!(q.since_id, None);
        assert_eq!(q.path_prefix, None);

        let q = parse_query("since_id=42");
        assert_eq!(q.since_id, Some(42));
        assert_eq!(q.path_prefix, None);

        let q = parse_query("since_id=0&path_prefix=/docs");
        assert_eq!(q.since_id, Some(0));
        assert_eq!(q.path_prefix.as_deref(), Some("/docs"));
    }

    #[test]
    fn invalid_path_prefix_is_rejected_at_normalize() {
        // The route maps any veda_core::path::normalize error to 400.
        assert!(veda_core::path::normalize("../escape").is_err());
        assert!(veda_core::path::normalize("/").is_ok());
        assert!(veda_core::path::normalize("/docs").is_ok());
    }

    #[tokio::test]
    async fn build_410_body_carries_min_and_max_ids() {
        use http_body_util::BodyExt;
        let resp = build_410_response(1000, Some(2042));
        assert_eq!(resp.status(), StatusCode::GONE);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "cursor expired");
        assert_eq!(json["current_min_id"], 1000);
        assert_eq!(json["current_max_id"], 2042);
    }

    #[tokio::test]
    async fn build_410_body_carries_null_max_when_none() {
        use http_body_util::BodyExt;
        let resp = build_410_response(7, None);
        assert_eq!(resp.status(), StatusCode::GONE);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["current_max_id"].is_null());
    }
}
