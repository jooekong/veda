use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use axum::Router;
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
}

async fn sse_events(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    Query(q): Query<EventQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let workspace_id = auth.workspace_id.clone();
    let mut cursor = q.since_id.unwrap_or(0);

    let stream = async_stream::stream! {
        let mut backoff = POLL_INTERVAL;
        loop {
            match state
                .fs_service
                .query_events(&workspace_id, cursor, BATCH_SIZE)
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
                        yield Ok(Event::default()
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

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
}
