//! Agent/team memory REST surface (docs/plans/agent-memory-m1.md Step 3).
//!
//! Thin shell over `MemoryService`: identity resolution (wk_ key →
//! principal) happens here, everything else — scope resolution, origin
//! defaults, recheck reads — lives in the service, shared with the MCP
//! memory_* tools so the two surfaces cannot drift.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use veda_core::service::memory::{MemoryActor, SaveMemoryInput, UpdateMemoryInput};
use veda_types::api::{
    MemoryItem, MemoryListResponse, MemoryPageResponse, MemoryTopicCount, MemoryTopicsResponse,
    SaveMemoryApiRequest, SaveMemoryResponse, UpdateMemoryApiRequest,
};
use veda_types::{ApiResponse, MemoryKind, MemoryScope, VedaError};

use crate::auth::{parse_operator, AuthWorkspace};
use crate::error::AppError;
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/memory", post(save_memory))
        .route("/v1/memory/search", get(search_memory))
        .route("/v1/memory/context", get(memory_context))
        .route("/v1/memory/list", get(list_memory))
        .route("/v1/memory/topics", get(memory_topics))
        .route("/v1/memory/{id}", patch(update_memory))
        .route("/v1/memory/{id}", delete(delete_memory))
}

/// Request actor: the asserted operator when the header is present, else the
/// key (M1 semantics). Operator present → the key principal is resolved too
/// so `scope=self` keeps targeting agent state. Operator asserted but
/// unresolvable (directory down, identity never seen) → the actor collapses
/// to team-only: the service rejects personal/dept scopes and reads stay in
/// the team domain — degraded humans never touch the shared key's private
/// rows (M3a §1.2).
async fn actor(
    state: &Arc<AppState>,
    auth: &AuthWorkspace,
    headers: &HeaderMap,
) -> Result<MemoryActor, AppError> {
    let op = parse_operator(headers).map_err(|m| AppError(VedaError::InvalidInput(m)))?;
    let key_actor = state
        .memory_service
        .resolve_key_actor(&auth.workspace_id, &auth.key_id)
        .await?;
    let Some((source, external_id)) = op else {
        return Ok(key_actor);
    };
    match state
        .memory_service
        .resolve_operator_actor(&auth.workspace_id, source, &external_id)
        .await?
    {
        Some(mut a) => {
            a.self_principal_id = Some(key_actor.principal_id);
            Ok(a)
        }
        None => Ok(MemoryActor {
            team_only: true,
            ..key_actor
        }),
    }
}

async fn save_memory(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    headers: HeaderMap,
    Json(req): Json<SaveMemoryApiRequest>,
) -> Result<Json<ApiResponse<SaveMemoryResponse>>, AppError> {
    auth.require_write()?;
    let actor = actor(&state, &auth, &headers).await?;
    let out = state
        .memory_service
        .save(
            &actor,
            SaveMemoryInput {
                content: req.content,
                kind: req.kind.unwrap_or(MemoryKind::Fact),
                scope: req.scope.unwrap_or_default(),
                topic: req.topic,
                origin: req.origin,
                source_ref: req.source_ref,
                expires_at: req.expires_at,
            },
        )
        .await?;
    Ok(Json(ApiResponse::ok(SaveMemoryResponse {
        memory: MemoryItem::from_memory(out.memory, None),
        duplicate: out.duplicate,
        neighbors: out
            .neighbors
            .into_iter()
            .map(|n| MemoryItem::from_memory(n.memory, Some(n.score)))
            .collect(),
    })))
}

#[derive(Debug, Deserialize)]
struct MemoryQuery {
    query: String,
    scope: Option<MemoryScope>,
    limit: Option<usize>,
}

async fn search_memory(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    headers: HeaderMap,
    Query(q): Query<MemoryQuery>,
) -> Result<Json<ApiResponse<MemoryListResponse>>, AppError> {
    let actor = actor(&state, &auth, &headers).await?;
    let hits = state
        .memory_service
        .search(&actor, &q.query, q.scope, q.limit.unwrap_or(10))
        .await?;
    Ok(Json(ApiResponse::ok(MemoryListResponse {
        items: hits
            .into_iter()
            .map(|h| MemoryItem::from_memory(h.memory, Some(h.score)))
            .collect(),
    })))
}

async fn memory_context(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    headers: HeaderMap,
    Query(q): Query<MemoryQuery>,
) -> Result<Json<ApiResponse<MemoryListResponse>>, AppError> {
    let actor = actor(&state, &auth, &headers).await?;
    let hits = state
        .memory_service
        .context(&actor, &q.query, q.limit.unwrap_or(10))
        .await?;
    Ok(Json(ApiResponse::ok(MemoryListResponse {
        items: hits
            .into_iter()
            .map(|h| MemoryItem::from_memory(h.memory, Some(h.score)))
            .collect(),
    })))
}

#[derive(Debug, Deserialize)]
struct MemoryBrowseQuery {
    tab: MemoryScope,
    /// Exact topic; "" selects the uncategorized bucket; absent = all.
    topic: Option<String>,
    kind: Option<MemoryKind>,
    page: Option<u32>,
    size: Option<u32>,
}

/// The personal/dept tabs are meaningless without an operator identity — a
/// bare shared key would browse the agent's own domain as "mine". Reject
/// loudly instead of showing the wrong domain (m4a §1.2).
fn require_operator_for_private_tab(
    tab: MemoryScope,
    headers: &HeaderMap,
) -> Result<(), AppError> {
    match parse_operator(headers).map_err(|m| AppError(VedaError::InvalidInput(m)))? {
        None if !matches!(tab, MemoryScope::Team) => Err(AppError(VedaError::InvalidInput(
            "this tab needs an operator identity — send X-Veda-Operator: <source>:<id>".into(),
        ))),
        _ => Ok(()),
    }
}

async fn list_memory(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    headers: HeaderMap,
    Query(q): Query<MemoryBrowseQuery>,
) -> Result<Json<ApiResponse<MemoryPageResponse>>, AppError> {
    require_operator_for_private_tab(q.tab, &headers)?;
    let actor = actor(&state, &auth, &headers).await?;
    let page = q.page.unwrap_or(1).max(1);
    let size = q.size.unwrap_or(50).clamp(1, 100);
    let (rows, total) = state
        .memory_service
        .list(&actor, q.tab, q.topic.as_deref(), q.kind, page, size)
        .await?;
    Ok(Json(ApiResponse::ok(MemoryPageResponse {
        items: rows
            .into_iter()
            .map(|m| MemoryItem::from_memory(m, None))
            .collect(),
        total,
        page,
        size,
    })))
}

async fn memory_topics(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    headers: HeaderMap,
    Query(q): Query<MemoryBrowseQuery>,
) -> Result<Json<ApiResponse<MemoryTopicsResponse>>, AppError> {
    require_operator_for_private_tab(q.tab, &headers)?;
    let actor = actor(&state, &auth, &headers).await?;
    let topics = state
        .memory_service
        .topics(&actor, q.tab)
        .await?
        .into_iter()
        .map(|(topic, count)| MemoryTopicCount { topic, count })
        .collect();
    Ok(Json(ApiResponse::ok(MemoryTopicsResponse { topics })))
}

async fn update_memory(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(req): Json<UpdateMemoryApiRequest>,
) -> Result<Json<ApiResponse<MemoryItem>>, AppError> {
    auth.require_write()?;
    let actor = actor(&state, &auth, &headers).await?;
    let m = state
        .memory_service
        .update(
            &actor,
            id,
            UpdateMemoryInput {
                content: req.content,
                topic: req.topic,
                source_ref: req.source_ref,
                expires_at: req.expires_at,
                scope: req.scope,
            },
        )
        .await?;
    Ok(Json(ApiResponse::ok(MemoryItem::from_memory(m, None))))
}

async fn delete_memory(
    State(state): State<Arc<AppState>>,
    auth: AuthWorkspace,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<ApiResponse<serde_json::Value>>, AppError> {
    auth.require_write()?;
    let actor = actor(&state, &auth, &headers).await?;
    state.memory_service.delete(&actor, id).await?;
    Ok(Json(ApiResponse::ok(serde_json::json!({ "deleted": id }))))
}
