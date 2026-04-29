pub mod account;
pub mod collection;
pub mod events;
pub mod fs;
pub mod search;
pub mod sql;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

const READY_TIMEOUT: Duration = Duration::from_secs(3);

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/ready", get(ready))
        .merge(account::routes())
        .merge(fs::routes())
        .merge(events::routes())
        .merge(search::routes())
        .merge(collection::routes())
        .merge(sql::routes())
        .with_state(state)
}

use crate::state::AppState;

#[derive(Serialize)]
struct ReadyResponse {
    status: &'static str,
    components: Vec<ComponentHealth>,
}

#[derive(Serialize)]
struct ComponentHealth {
    name: &'static str,
    ok: bool,
}

async fn ready(State(state): State<Arc<AppState>>) -> Response {
    let (mysql_res, milvus_res) = tokio::join!(
        tokio::time::timeout(READY_TIMEOUT, state.meta_store.ping()),
        tokio::time::timeout(READY_TIMEOUT, state.vector_store.ping()),
    );
    let mysql_ok = mysql_res.map(|r| r.is_ok()).unwrap_or(false);
    let milvus_ok = milvus_res.map(|r| r.is_ok()).unwrap_or(false);
    let all_ok = mysql_ok && milvus_ok;
    let body = ReadyResponse {
        status: if all_ok { "ready" } else { "not_ready" },
        components: vec![
            ComponentHealth {
                name: "mysql",
                ok: mysql_ok,
            },
            ComponentHealth {
                name: "milvus",
                ok: milvus_ok,
            },
        ],
    };
    let status = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_response_serializes_correctly() {
        let resp = ReadyResponse {
            status: "ready",
            components: vec![
                ComponentHealth {
                    name: "mysql",
                    ok: true,
                },
                ComponentHealth {
                    name: "milvus",
                    ok: true,
                },
            ],
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["status"], "ready");
        assert_eq!(json["components"][0]["name"], "mysql");
        assert_eq!(json["components"][0]["ok"], true);
        assert_eq!(json["components"][1]["name"], "milvus");
    }

    #[test]
    fn ready_response_not_ready() {
        let resp = ReadyResponse {
            status: "not_ready",
            components: vec![
                ComponentHealth {
                    name: "mysql",
                    ok: true,
                },
                ComponentHealth {
                    name: "milvus",
                    ok: false,
                },
            ],
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["status"], "not_ready");
        assert_eq!(json["components"][1]["ok"], false);
    }
}
