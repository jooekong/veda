//! Server end-to-end tests. Run with: `cargo test -p veda-server -- --ignored`
//!
//! Covers account → workspace → workspace-key bootstrap plus fs API behavior.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{post, put};
use axum::{Json, Router};
use serde::Deserialize;
use tokio::net::TcpListener;

// ── Config loading ─────────────────────────────────────

#[derive(Debug, Deserialize)]
struct MysqlSection {
    database_url: String,
}
#[derive(Debug, Deserialize)]
struct MilvusSection {
    url: String,
    token: Option<String>,
    db: Option<String>,
}
#[derive(Debug, Deserialize)]
struct EmbeddingSection {
    api_url: String,
    api_key: String,
    model: String,
    dimension: u32,
}
#[derive(Debug, Deserialize)]
struct TestConfig {
    mysql: MysqlSection,
    milvus: MilvusSection,
    embedding: EmbeddingSection,
}

fn load_config() -> TestConfig {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .join("config/test.toml");
    let raw = std::fs::read_to_string(&path).unwrap();
    toml::from_str(&raw).unwrap()
}

// ── Minimal in-process server ──────────────────────────

struct St {
    fs: veda_core::service::fs::FsService,
    auth: Arc<dyn veda_core::store::AuthStore>,
    jwt_secret: String,
}

#[derive(Debug, Clone)]
struct WsAuth {
    workspace_id: String,
    read_only: bool,
}

fn resolve_ws_sync(st: &St, auth: &str) -> Option<WsAuth> {
    let token = auth.strip_prefix("Bearer ")?;
    #[derive(Deserialize)]
    struct C {
        workspace_id: String,
    }
    if let Ok(td) = jsonwebtoken::decode::<C>(
        token,
        &jsonwebtoken::DecodingKey::from_secret(st.jwt_secret.as_bytes()),
        &jsonwebtoken::Validation::default(),
    ) {
        return Some(WsAuth {
            workspace_id: td.claims.workspace_id,
            read_only: false,
        });
    }
    None
}

async fn resolve_ws(st: &St, auth: &str) -> Option<WsAuth> {
    if let Some(ws) = resolve_ws_sync(st, auth) {
        return Some(ws);
    }
    let token = auth.strip_prefix("Bearer ")?;
    let kh = veda_core::checksum::sha256_hex(token.as_bytes());
    st.auth
        .get_workspace_key_by_hash(&kh)
        .await
        .ok()?
        .map(|wk| WsAuth {
            workspace_id: wk.workspace_id,
            read_only: wk.permission == veda_types::KeyPermission::Read,
        })
}

async fn resolve_acct(st: &St, auth: &str) -> Option<String> {
    let token = auth.strip_prefix("Bearer ")?;
    let kh = veda_core::checksum::sha256_hex(token.as_bytes());
    st.auth
        .get_api_key_by_hash(&kh)
        .await
        .ok()?
        .map(|ak| ak.account_id)
}

fn auth_hdr(h: &HeaderMap) -> &str {
    h.get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

type S = Arc<St>;

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(veda_types::ApiResponse::<()>::err("unauthorized")),
    )
        .into_response()
}

fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(veda_types::ApiResponse::<()>::err("permission denied")),
    )
        .into_response()
}

fn veda_error_to_response(err: veda_types::VedaError) -> Response {
    let (status, msg) = match err {
        veda_types::VedaError::NotFound(p) => (StatusCode::NOT_FOUND, format!("not found: {p}")),
        veda_types::VedaError::AlreadyExists(p) => {
            (StatusCode::CONFLICT, format!("already exists: {p}"))
        }
        veda_types::VedaError::PermissionDenied => {
            (StatusCode::FORBIDDEN, "permission denied".to_string())
        }
        veda_types::VedaError::InvalidPath(p) => {
            (StatusCode::BAD_REQUEST, format!("invalid path: {p}"))
        }
        veda_types::VedaError::InvalidInput(p) => {
            (StatusCode::BAD_REQUEST, format!("invalid input: {p}"))
        }
        veda_types::VedaError::QuotaExceeded(p) => (
            StatusCode::TOO_MANY_REQUESTS,
            format!("quota exceeded: {p}"),
        ),
        veda_types::VedaError::PayloadTooLarge(p) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("payload too large: {p}"),
        ),
        veda_types::VedaError::PreconditionFailed(p) => (
            StatusCode::PRECONDITION_FAILED,
            format!("precondition failed: {p}"),
        ),
        veda_types::VedaError::Storage(_) | veda_types::VedaError::Internal(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal server error".to_string(),
        ),
        veda_types::VedaError::Deadlock(p) => {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("deadlock: {p}"))
        }
        veda_types::VedaError::EmbeddingFailed(p) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("embedding failed: {p}"),
        ),
    };
    (status, Json(veda_types::ApiResponse::<()>::err(msg))).into_response()
}

async fn h_create_account(
    State(st): State<S>,
    Json(req): Json<veda_types::api::CreateAccountRequest>,
) -> Response {
    use argon2::password_hash::{rand_core::OsRng, SaltString};
    use argon2::{Argon2, PasswordHasher};
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(req.password.as_bytes(), &salt)
        .unwrap()
        .to_string();
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    st.auth
        .create_account(&veda_types::Account {
            id: id.clone(),
            name: req.name,
            email: Some(req.email),
            password_hash: Some(hash),
            status: veda_types::AccountStatus::Active,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let raw = format!("vk_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    let kh = veda_core::checksum::sha256_hex(raw.as_bytes());
    st.auth
        .create_api_key(&veda_types::ApiKeyRecord {
            id: uuid::Uuid::new_v4().to_string(),
            account_id: id.clone(),
            name: "default".into(),
            key_hash: kh,
            status: veda_types::KeyStatus::Active,
            created_at: now,
        })
        .await
        .unwrap();
    Json(veda_types::ApiResponse::ok(
        veda_types::api::CreateAccountResponse {
            account_id: id,
            api_key: raw,
        },
    ))
    .into_response()
}

async fn h_create_ws(
    State(st): State<S>,
    headers: HeaderMap,
    Json(req): Json<veda_types::api::CreateWorkspaceRequest>,
) -> Response {
    let Some(aid) = resolve_acct(&st, auth_hdr(&headers)).await else {
        return unauthorized();
    };
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let ws = veda_types::Workspace {
        id: id.clone(),
        account_id: aid,
        name: req.name,
        status: veda_types::WorkspaceStatus::Active,
        created_at: now,
        updated_at: now,
    };
    st.auth.create_workspace(&ws).await.unwrap();
    Json(veda_types::ApiResponse::ok(ws)).into_response()
}

async fn h_create_wk(
    State(st): State<S>,
    headers: HeaderMap,
    Path(ws_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if resolve_acct(&st, auth_hdr(&headers)).await.is_none() {
        return unauthorized();
    };
    let permission = match body.get("permission").and_then(|v| v.as_str()) {
        Some("read") => veda_types::KeyPermission::Read,
        _ => veda_types::KeyPermission::ReadWrite,
    };
    let permission_text = if permission == veda_types::KeyPermission::Read {
        "read"
    } else {
        "readwrite"
    };
    let raw = format!("wk_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    let kh = veda_core::checksum::sha256_hex(raw.as_bytes());
    st.auth
        .create_workspace_key(&veda_types::WorkspaceKey {
            id: uuid::Uuid::new_v4().to_string(),
            workspace_id: ws_id,
            name: "test".into(),
            key_hash: kh,
            permission,
            status: veda_types::KeyStatus::Active,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    Json(veda_types::ApiResponse::ok(
        serde_json::json!({"key": raw, "permission": permission_text}),
    ))
    .into_response()
}

async fn h_write(
    State(st): State<S>,
    headers: HeaderMap,
    Path(p): Path<String>,
    body: Bytes,
) -> Response {
    let Some(ws) = resolve_ws(&st, auth_hdr(&headers)).await else {
        return unauthorized();
    };
    if ws.read_only {
        return forbidden();
    }
    let Ok(body) = std::str::from_utf8(&body) else {
        return veda_error_to_response(veda_types::VedaError::InvalidInput(
            "file content must be valid UTF-8".into(),
        ));
    };
    let expected_rev = parse_if_match(&headers);
    let if_none_match = parse_if_none_match_sha256(&headers);
    match st
        .fs
        .write_file(
            &ws.workspace_id,
            &format!("/{p}"),
            body,
            expected_rev,
            if_none_match.as_deref(),
        )
        .await
    {
        Ok(r) => {
            let mut resp = Json(veda_types::ApiResponse::ok(r.clone())).into_response();
            resp.headers_mut().insert(
                header::ETAG,
                HeaderValue::from_str(&format!("\"{}\"", r.revision)).unwrap(),
            );
            resp
        }
        Err(e) => veda_error_to_response(e),
    }
}

fn parse_if_match(h: &HeaderMap) -> Option<i32> {
    let v = h.get(header::IF_MATCH)?.to_str().ok()?;
    v.trim().trim_matches('"').parse().ok()
}

fn parse_if_none_match_sha256(h: &HeaderMap) -> Option<String> {
    let v = h.get(header::IF_NONE_MATCH)?.to_str().ok()?;
    let trimmed = v.trim().trim_matches('"');
    if trimmed.len() != 64 || !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(trimmed.to_ascii_lowercase())
}

#[derive(Deserialize, Default)]
struct FsQ {
    list: Option<String>,
    lines: Option<String>,
    stat: Option<String>,
}

async fn h_read_root(State(st): State<S>, headers: HeaderMap, Query(q): Query<FsQ>) -> Response {
    let Some(ws) = resolve_ws(&st, auth_hdr(&headers)).await else {
        return unauthorized();
    };
    if q.stat.is_some() {
        return match st.fs.stat(&ws.workspace_id, "/").await {
            Ok(info) => Json(veda_types::ApiResponse::ok(info)).into_response(),
            Err(e) => veda_error_to_response(e),
        };
    }
    if q.list.is_some() {
        return match st.fs.list_dir(&ws.workspace_id, "/").await {
            Ok(entries) => Json(veda_types::ApiResponse::ok(entries)).into_response(),
            Err(e) => veda_error_to_response(e),
        };
    }
    veda_error_to_response(veda_types::VedaError::InvalidInput(
        "use ?stat or ?list".into(),
    ))
}

async fn h_stat_root(State(st): State<S>, headers: HeaderMap) -> Response {
    let Some(ws) = resolve_ws(&st, auth_hdr(&headers)).await else {
        return unauthorized();
    };
    match st.fs.stat(&ws.workspace_id, "/").await {
        Ok(info) => Json(veda_types::ApiResponse::ok(info)).into_response(),
        Err(e) => veda_error_to_response(e),
    }
}

async fn h_del_root(State(_st): State<S>, headers: HeaderMap) -> Response {
    let Some(_ws) = resolve_ws(&_st, auth_hdr(&headers)).await else {
        return unauthorized();
    };
    veda_error_to_response(veda_types::VedaError::InvalidPath(
        "cannot delete root".into(),
    ))
}

async fn h_append(
    State(st): State<S>,
    headers: HeaderMap,
    Path(p): Path<String>,
    body: Bytes,
) -> Response {
    let Some(ws) = resolve_ws(&st, auth_hdr(&headers)).await else {
        return unauthorized();
    };
    if ws.read_only {
        return forbidden();
    }
    let Ok(body) = std::str::from_utf8(&body) else {
        return veda_error_to_response(veda_types::VedaError::InvalidInput(
            "file content must be valid UTF-8".into(),
        ));
    };
    match st
        .fs
        .append_file(&ws.workspace_id, &format!("/{p}"), body)
        .await
    {
        Ok(r) => Json(veda_types::ApiResponse::ok(r)).into_response(),
        Err(e) => veda_error_to_response(e),
    }
}

async fn h_read(
    State(st): State<S>,
    headers: HeaderMap,
    Path(p): Path<String>,
    Query(q): Query<FsQ>,
) -> Response {
    let Some(ws) = resolve_ws(&st, auth_hdr(&headers)).await else {
        return unauthorized();
    };
    let path = format!("/{p}");
    if q.stat.is_some() {
        return match st.fs.stat(&ws.workspace_id, &path).await {
            Ok(info) => Json(veda_types::ApiResponse::ok(info)).into_response(),
            Err(e) => veda_error_to_response(e),
        };
    }
    if q.list.is_some() {
        return match st.fs.list_dir(&ws.workspace_id, &path).await {
            Ok(entries) => Json(veda_types::ApiResponse::ok(entries)).into_response(),
            Err(e) => veda_error_to_response(e),
        };
    }
    if let Some(lines) = q.lines.as_deref() {
        let parts: Vec<&str> = lines.split(':').collect();
        if parts.len() != 2 {
            return veda_error_to_response(veda_types::VedaError::InvalidInput(
                "lines format: start:end".into(),
            ));
        }
        let Ok(start) = parts[0].parse::<i32>() else {
            return veda_error_to_response(veda_types::VedaError::InvalidInput(
                "invalid line number".into(),
            ));
        };
        let Ok(end) = parts[1].parse::<i32>() else {
            return veda_error_to_response(veda_types::VedaError::InvalidInput(
                "invalid line number".into(),
            ));
        };
        return match st
            .fs
            .read_file_lines(&ws.workspace_id, &path, start, end)
            .await
        {
            Ok(content) => content.into_response(),
            Err(e) => veda_error_to_response(e),
        };
    }
    if let Some(range_val) = headers.get(header::RANGE) {
        if let Ok(range_str) = range_val.to_str() {
            if let Some((offset, length)) = parse_range_header(range_str) {
                return match st
                    .fs
                    .read_file_range(&ws.workspace_id, &path, offset, length)
                    .await
                {
                    Ok((data, total)) => {
                        if data.is_empty() {
                            return (
                                StatusCode::RANGE_NOT_SATISFIABLE,
                                [(header::CONTENT_RANGE, format!("bytes */{total}"))],
                            )
                                .into_response();
                        }
                        let end = offset + data.len() as u64 - 1;
                        (
                            StatusCode::PARTIAL_CONTENT,
                            [
                                (
                                    header::CONTENT_RANGE,
                                    format!("bytes {offset}-{end}/{total}"),
                                ),
                                (header::CONTENT_LENGTH, data.len().to_string()),
                            ],
                            data,
                        )
                            .into_response()
                    }
                    Err(e) => veda_error_to_response(e),
                };
            }
        }
    }
    match st.fs.read_file(&ws.workspace_id, &path).await {
        Ok(c) => c.into_response(),
        Err(e) => veda_error_to_response(e),
    }
}

fn parse_range_header(s: &str) -> Option<(u64, u64)> {
    let s = s.strip_prefix("bytes=")?;
    let parts: Vec<&str> = s.splitn(2, '-').collect();
    if parts.len() != 2 {
        return None;
    }
    let start: u64 = parts[0].parse().ok()?;
    if parts[1].is_empty() {
        return Some((start, u64::MAX));
    }
    let end: u64 = parts[1].parse().ok()?;
    if end < start {
        return None;
    }
    Some((start, end - start + 1))
}

async fn h_stat(State(st): State<S>, headers: HeaderMap, Path(p): Path<String>) -> Response {
    let Some(ws) = resolve_ws(&st, auth_hdr(&headers)).await else {
        return unauthorized();
    };
    match st.fs.stat(&ws.workspace_id, &format!("/{p}")).await {
        Ok(info) => Json(veda_types::ApiResponse::ok(info)).into_response(),
        Err(e) => veda_error_to_response(e),
    }
}

async fn h_del(State(st): State<S>, headers: HeaderMap, Path(p): Path<String>) -> Response {
    let Some(ws) = resolve_ws(&st, auth_hdr(&headers)).await else {
        return unauthorized();
    };
    if ws.read_only {
        return forbidden();
    }
    match st.fs.delete(&ws.workspace_id, &format!("/{p}")).await {
        Ok(_count) => Json(veda_types::ApiResponse::ok(())).into_response(),
        Err(e) => veda_error_to_response(e),
    }
}

#[derive(Deserialize)]
struct CopyRenameBody {
    from: String,
    to: String,
}

async fn h_copy(
    State(st): State<S>,
    headers: HeaderMap,
    Json(body): Json<CopyRenameBody>,
) -> Response {
    let Some(ws) = resolve_ws(&st, auth_hdr(&headers)).await else {
        return unauthorized();
    };
    if ws.read_only {
        return forbidden();
    }
    match st
        .fs
        .copy_file(&ws.workspace_id, &body.from, &body.to)
        .await
    {
        Ok(_) => Json(veda_types::ApiResponse::ok(())).into_response(),
        Err(e) => veda_error_to_response(e),
    }
}

async fn h_rename(
    State(st): State<S>,
    headers: HeaderMap,
    Json(body): Json<CopyRenameBody>,
) -> Response {
    let Some(ws) = resolve_ws(&st, auth_hdr(&headers)).await else {
        return unauthorized();
    };
    if ws.read_only {
        return forbidden();
    }
    match st.fs.rename(&ws.workspace_id, &body.from, &body.to).await {
        Ok(_) => Json(veda_types::ApiResponse::ok(())).into_response(),
        Err(e) => veda_error_to_response(e),
    }
}

#[derive(Deserialize)]
struct MkdirBody {
    path: String,
}

async fn h_mkdir(State(st): State<S>, headers: HeaderMap, Json(body): Json<MkdirBody>) -> Response {
    let Some(ws) = resolve_ws(&st, auth_hdr(&headers)).await else {
        return unauthorized();
    };
    if ws.read_only {
        return forbidden();
    }
    match st.fs.mkdir(&ws.workspace_id, &body.path).await {
        Ok(_) => Json(veda_types::ApiResponse::ok(())).into_response(),
        Err(e) => veda_error_to_response(e),
    }
}

async fn start_server() -> (String, reqwest::Client) {
    let cfg = load_config();
    let mysql = Arc::new(
        veda_store::MysqlStore::new(&cfg.mysql.database_url)
            .await
            .unwrap(),
    );
    mysql.migrate().await.unwrap();
    let st = Arc::new(St {
        fs: veda_core::service::fs::FsService::new(mysql.clone()),
        auth: mysql.clone(),
        jwt_secret: "test-jwt-secret".into(),
    });
    let app = Router::new()
        .route("/v1/accounts", post(h_create_account))
        .route("/v1/workspaces", post(h_create_ws))
        .route("/v1/workspaces/{id}/keys", post(h_create_wk))
        .route(
            "/v1/fs",
            axum::routing::get(h_read_root)
                .head(h_stat_root)
                .delete(h_del_root),
        )
        .route(
            "/v1/fs/{*path}",
            put(h_write)
                .post(h_append)
                .get(h_read)
                .head(h_stat)
                .delete(h_del),
        )
        .route("/v1/fs-copy", post(h_copy))
        .route("/v1/fs-rename", post(h_rename))
        .route("/v1/fs-mkdir", post(h_mkdir))
        .with_state(st);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (base, reqwest::Client::new())
}

async fn post_json(
    c: &reqwest::Client,
    url: &str,
    key: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let mut req = c.post(url);
    if !key.is_empty() {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let resp = req.json(&body).send().await.unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    assert!(status.is_success(), "POST {url} returned {status}: {text}");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("json parse: {e}\nbody: {text}"))
}

async fn json_response(resp: reqwest::Response) -> (StatusCode, serde_json::Value) {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("json parse: {e}\nbody: {text}"));
    (status, value)
}

#[derive(Debug)]
struct TestCtx {
    base: String,
    c: reqwest::Client,
    rw_key: String,
    ro_key: String,
}

async fn create_workspace_key(
    c: &reqwest::Client,
    base: &str,
    api_key: &str,
    ws_id: &str,
    permission: &str,
) -> String {
    let r = post_json(
        c,
        &format!("{base}/v1/workspaces/{ws_id}/keys"),
        api_key,
        serde_json::json!({"permission": permission}),
    )
    .await;
    r["data"]["key"].as_str().unwrap().to_string()
}

async fn bootstrap_ctx() -> TestCtx {
    let (base, c) = start_server().await;
    let r = post_json(
        &c,
        &format!("{base}/v1/accounts"),
        "",
        serde_json::json!({
            "name":"u",
            "email":format!("t{}@x.com",uuid::Uuid::new_v4()),
            "password":"pass"
        }),
    )
    .await;
    let api_key = r["data"]["api_key"].as_str().unwrap().to_string();

    let r = post_json(
        &c,
        &format!("{base}/v1/workspaces"),
        &api_key,
        serde_json::json!({"name":"ws"}),
    )
    .await;
    let ws_id = r["data"]["id"].as_str().unwrap().to_string();

    let rw_key = create_workspace_key(&c, &base, &api_key, &ws_id, "readwrite").await;
    let ro_key = create_workspace_key(&c, &base, &api_key, &ws_id, "read").await;

    TestCtx {
        base,
        c,
        rw_key,
        ro_key,
    }
}

#[tokio::test]
#[ignore]
async fn server_e2e_basic_fs_crud_and_stat() {
    let ctx = bootstrap_ctx().await;
    let auth = format!("Bearer {}", ctx.rw_key);

    let resp = ctx
        .c
        .put(format!("{}/v1/fs/docs/hello.txt", ctx.base))
        .header("Authorization", &auth)
        .body("Hello Veda!")
        .send()
        .await
        .unwrap();
    let etag = resp.headers().get(header::ETAG).cloned();
    let (status, body) = json_response(resp).await;
    assert!(status.is_success(), "write file: {body}");
    assert_eq!(body["data"]["revision"], 1);
    assert_eq!(etag.as_ref().and_then(|v| v.to_str().ok()), Some("\"1\""));

    let resp = ctx
        .c
        .get(format!("{}/v1/fs/docs/hello.txt", ctx.base))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), "Hello Veda!");

    let resp = ctx
        .c
        .get(format!("{}/v1/fs/docs/hello.txt?stat=1", ctx.base))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["is_dir"], false);
    assert_eq!(body["data"]["size_bytes"], 11);
    assert_eq!(body["data"]["revision"], 1);

    let resp = ctx
        .c
        .get(format!("{}/v1/fs/docs?list=1", ctx.base))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["name"] == "hello.txt"));

    let resp = ctx
        .c
        .get(format!("{}/v1/fs?stat=1", ctx.base))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["path"], "/");
    assert_eq!(body["data"]["is_dir"], true);

    let resp = ctx
        .c
        .delete(format!("{}/v1/fs/docs/hello.txt", ctx.base))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK, "delete file: {body}");

    let resp = ctx
        .c
        .get(format!("{}/v1/fs/docs/hello.txt", ctx.base))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore]
async fn server_e2e_mkdir_copy_rename_lines_and_range() {
    let ctx = bootstrap_ctx().await;
    let auth = format!("Bearer {}", ctx.rw_key);

    let resp = ctx
        .c
        .post(format!("{}/v1/fs-mkdir", ctx.base))
        .header("Authorization", &auth)
        .json(&serde_json::json!({"path":"/docs/nested/deep"}))
        .send()
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK, "mkdir: {body}");

    let resp = ctx
        .c
        .get(format!("{}/v1/fs/docs/nested/deep?stat=1", ctx.base))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["is_dir"], true);

    let content = "line1\nline2\nline3\n";
    let resp = ctx
        .c
        .put(format!("{}/v1/fs/docs/src.txt", ctx.base))
        .header("Authorization", &auth)
        .body(content)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = ctx
        .c
        .post(format!("{}/v1/fs-copy", ctx.base))
        .header("Authorization", &auth)
        .json(&serde_json::json!({"from":"/docs/src.txt","to":"/docs/dst.txt"}))
        .send()
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK, "copy: {body}");

    let resp = ctx
        .c
        .post(format!("{}/v1/fs-rename", ctx.base))
        .header("Authorization", &auth)
        .json(&serde_json::json!({"from":"/docs/dst.txt","to":"/docs/final.txt"}))
        .send()
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK, "rename: {body}");

    let resp = ctx
        .c
        .get(format!("{}/v1/fs/docs/dst.txt", ctx.base))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = ctx
        .c
        .get(format!("{}/v1/fs/docs/final.txt", ctx.base))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), content);

    let resp = ctx
        .c
        .get(format!("{}/v1/fs/docs/final.txt?lines=2:3", ctx.base))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), "line2\nline3");

    let resp = ctx
        .c
        .get(format!("{}/v1/fs/docs/final.txt", ctx.base))
        .header("Authorization", &auth)
        .header(header::RANGE, "bytes=6-10")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(resp.headers()[header::CONTENT_RANGE], "bytes 6-10/18");
    assert_eq!(resp.text().await.unwrap(), "line2");

    let resp = ctx
        .c
        .get(format!("{}/v1/fs/docs/final.txt", ctx.base))
        .header("Authorization", &auth)
        .header(header::RANGE, "bytes=200-")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(resp.headers()[header::CONTENT_RANGE], "bytes */18");
}

#[tokio::test]
#[ignore]
async fn server_e2e_conditional_write_and_append() {
    let ctx = bootstrap_ctx().await;
    let auth = format!("Bearer {}", ctx.rw_key);
    let path = format!("{}/v1/fs/docs/cas.txt", ctx.base);

    let resp = ctx
        .c
        .put(&path)
        .header("Authorization", &auth)
        .body("hello")
        .send()
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["revision"], 1);

    let resp = ctx
        .c
        .put(&path)
        .header("Authorization", &auth)
        .header(header::IF_MATCH, "\"0\"")
        .body("stale")
        .send()
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(body["success"], false);

    let resp = ctx
        .c
        .put(&path)
        .header("Authorization", &auth)
        .header(header::IF_MATCH, "\"1\"")
        .body("hello v2")
        .send()
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["revision"], 2);

    let hash = veda_core::checksum::sha256_hex("hello v2".as_bytes());
    let resp = ctx
        .c
        .put(&path)
        .header("Authorization", &auth)
        .header(header::IF_MATCH, "\"2\"")
        .header(header::IF_NONE_MATCH, format!("\"{hash}\""))
        .body("hello v2")
        .send()
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["revision"], 2);
    assert_eq!(body["data"]["content_unchanged"], true);

    let resp = ctx
        .c
        .post(&path)
        .header("Authorization", &auth)
        .body("++")
        .send()
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["revision"], 3);

    let resp = ctx
        .c
        .get(&path)
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), "hello v2++");

    let resp = ctx
        .c
        .post(format!("{}/v1/fs/docs/new-append.txt", ctx.base))
        .header("Authorization", &auth)
        .body("new")
        .send()
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["revision"], 1);
}

#[tokio::test]
#[ignore]
async fn server_e2e_auth_and_root_guards() {
    let ctx = bootstrap_ctx().await;
    let rw_auth = format!("Bearer {}", ctx.rw_key);
    let ro_auth = format!("Bearer {}", ctx.ro_key);

    let resp = ctx
        .c
        .put(format!("{}/v1/fs/docs/ro.txt", ctx.base))
        .header("Authorization", &ro_auth)
        .body("x")
        .send()
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "permission denied");

    let resp = ctx
        .c
        .post(format!("{}/v1/fs-mkdir", ctx.base))
        .header("Authorization", &ro_auth)
        .json(&serde_json::json!({"path":"/docs/readonly"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let resp = ctx
        .c
        .get(format!("{}/v1/fs?list=1", ctx.base))
        .header("Authorization", &ro_auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = ctx
        .c
        .delete(format!("{}/v1/fs", ctx.base))
        .header("Authorization", &rw_auth)
        .send()
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["success"], false);

    let resp = ctx
        .c
        .get(format!("{}/v1/fs?list=1", ctx.base))
        .send()
        .await
        .unwrap();
    let (status, body) = json_response(resp).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "unauthorized");
}
