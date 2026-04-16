//! Server integration tests. Run with: `cargo test -p veda-server -- --ignored`
//!
//! Tests the full account → workspace → workspace-key → fs CRUD flow via HTTP.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{post, put};
use axum::{Json, Router};
use serde::Deserialize;
use tokio::net::TcpListener;

// ── Config loading ─────────────────────────────────────

#[derive(Debug, Deserialize)]
struct MysqlSection { database_url: String }
#[derive(Debug, Deserialize)]
struct MilvusSection { url: String, token: Option<String>, db: Option<String> }
#[derive(Debug, Deserialize)]
struct EmbeddingSection { api_url: String, api_key: String, model: String, dimension: u32 }
#[derive(Debug, Deserialize)]
struct TestConfig { mysql: MysqlSection, milvus: MilvusSection, embedding: EmbeddingSection }

fn load_config() -> TestConfig {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors().nth(2).unwrap().join("config/test.toml");
    let raw = std::fs::read_to_string(&path).unwrap();
    toml::from_str(&raw).unwrap()
}

// ── Minimal in-process server ──────────────────────────

struct St {
    fs: veda_core::service::fs::FsService,
    auth: Arc<dyn veda_core::store::AuthStore>,
    jwt_secret: String,
}

fn resolve_ws_sync(st: &St, auth: &str) -> Option<String> {
    let token = auth.strip_prefix("Bearer ")?;
    #[derive(Deserialize)]
    struct C { workspace_id: String }
    if let Ok(td) = jsonwebtoken::decode::<C>(
        token,
        &jsonwebtoken::DecodingKey::from_secret(st.jwt_secret.as_bytes()),
        &jsonwebtoken::Validation::default(),
    ) {
        return Some(td.claims.workspace_id);
    }
    None
}

async fn resolve_ws(st: &St, auth: &str) -> Option<String> {
    if let Some(ws) = resolve_ws_sync(st, auth) {
        return Some(ws);
    }
    let token = auth.strip_prefix("Bearer ")?;
    let kh = veda_core::checksum::sha256_hex(token.as_bytes());
    st.auth.get_workspace_key_by_hash(&kh).await.ok()?
        .map(|wk| wk.workspace_id)
}

async fn resolve_acct(st: &St, auth: &str) -> Option<String> {
    let token = auth.strip_prefix("Bearer ")?;
    let kh = veda_core::checksum::sha256_hex(token.as_bytes());
    st.auth.get_api_key_by_hash(&kh).await.ok()?
        .map(|ak| ak.account_id)
}

fn auth_hdr(h: &axum::http::HeaderMap) -> &str {
    h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("")
}

type S = Arc<St>;

async fn h_create_account(State(st): State<S>, Json(req): Json<veda_types::api::CreateAccountRequest>) -> Response {
    use argon2::password_hash::{rand_core::OsRng, SaltString};
    use argon2::{Argon2, PasswordHasher};
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default().hash_password(req.password.as_bytes(), &salt).unwrap().to_string();
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    st.auth.create_account(&veda_types::Account {
        id: id.clone(), name: req.name, email: Some(req.email),
        password_hash: Some(hash), status: veda_types::AccountStatus::Active,
        created_at: now, updated_at: now,
    }).await.unwrap();
    let raw = format!("vk_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    let kh = veda_core::checksum::sha256_hex(raw.as_bytes());
    st.auth.create_api_key(&veda_types::ApiKeyRecord {
        id: uuid::Uuid::new_v4().to_string(), account_id: id.clone(), name: "default".into(),
        key_hash: kh, status: veda_types::KeyStatus::Active, created_at: now,
    }).await.unwrap();
    Json(veda_types::ApiResponse::ok(veda_types::api::CreateAccountResponse { account_id: id, api_key: raw })).into_response()
}

async fn h_create_ws(State(st): State<S>, headers: axum::http::HeaderMap, Json(req): Json<veda_types::api::CreateWorkspaceRequest>) -> Response {
    let aid = resolve_acct(&st, auth_hdr(&headers)).await.unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let ws = veda_types::Workspace { id: id.clone(), account_id: aid, name: req.name, status: veda_types::WorkspaceStatus::Active, created_at: now, updated_at: now };
    st.auth.create_workspace(&ws).await.unwrap();
    Json(veda_types::ApiResponse::ok(ws)).into_response()
}

async fn h_create_wk(State(st): State<S>, headers: axum::http::HeaderMap, Path(ws_id): Path<String>) -> Response {
    let _aid = resolve_acct(&st, auth_hdr(&headers)).await.unwrap();
    let raw = format!("wk_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    let kh = veda_core::checksum::sha256_hex(raw.as_bytes());
    st.auth.create_workspace_key(&veda_types::WorkspaceKey {
        id: uuid::Uuid::new_v4().to_string(), workspace_id: ws_id, name: "test".into(),
        key_hash: kh, permission: veda_types::KeyPermission::ReadWrite,
        status: veda_types::KeyStatus::Active, created_at: chrono::Utc::now(),
    }).await.unwrap();
    Json(veda_types::ApiResponse::ok(serde_json::json!({"key": raw}))).into_response()
}

async fn h_write(State(st): State<S>, headers: axum::http::HeaderMap, Path(p): Path<String>, body: String) -> Response {
    let ws = match resolve_ws(&st, auth_hdr(&headers)).await { Some(w) => w, None => return StatusCode::UNAUTHORIZED.into_response() };
    match st.fs.write_file(&ws, &format!("/{p}"), &body).await {
        Ok(r) => Json(veda_types::ApiResponse::ok(r)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize, Default)]
struct FsQ { list: Option<String> }

async fn h_read(State(st): State<S>, headers: axum::http::HeaderMap, Path(p): Path<String>, Query(q): Query<FsQ>) -> Response {
    let ws = match resolve_ws(&st, auth_hdr(&headers)).await { Some(w) => w, None => return StatusCode::UNAUTHORIZED.into_response() };
    let path = format!("/{p}");
    if q.list.is_some() {
        match st.fs.list_dir(&ws, &path).await {
            Ok(entries) => {
                let v: Vec<serde_json::Value> = entries.iter().map(|d| serde_json::json!({"name": d.name, "is_dir": d.is_dir})).collect();
                return Json(veda_types::ApiResponse::ok(v)).into_response();
            }
            Err(e) => return (StatusCode::NOT_FOUND, e.to_string()).into_response(),
        }
    }
    match st.fs.read_file(&ws, &path).await {
        Ok(c) => c.into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn h_del(State(st): State<S>, headers: axum::http::HeaderMap, Path(p): Path<String>) -> Response {
    let ws = match resolve_ws(&st, auth_hdr(&headers)).await { Some(w) => w, None => return StatusCode::UNAUTHORIZED.into_response() };
    match st.fs.delete(&ws, &format!("/{p}")).await {
        Ok(()) => Json(veda_types::ApiResponse::ok(())).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn start_server() -> (String, reqwest::Client) {
    let cfg = load_config();
    let mysql = Arc::new(veda_store::MysqlStore::new(&cfg.mysql.database_url).await.unwrap());
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
        .route("/v1/fs/{*path}", put(h_write).get(h_read).delete(h_del))
        .with_state(st);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    tokio::spawn(async move { axum::serve(listener, app).await.ok(); });
    (base, reqwest::Client::new())
}

async fn post_json(c: &reqwest::Client, url: &str, key: &str, body: serde_json::Value) -> serde_json::Value {
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

#[tokio::test]
#[ignore]
async fn server_account_workspace_fs_flow() {
    let (base, c) = start_server().await;

    let r = post_json(&c, &format!("{base}/v1/accounts"), "",
        serde_json::json!({"name":"u","email":format!("t{}@x.com",uuid::Uuid::new_v4()),"password":"pass"})).await;
    assert!(r["success"].as_bool().unwrap(), "create account: {r}");
    let api_key = r["data"]["api_key"].as_str().unwrap().to_string();

    let r = post_json(&c, &format!("{base}/v1/workspaces"), &api_key,
        serde_json::json!({"name":"ws"})).await;
    assert!(r["success"].as_bool().unwrap_or(false), "create workspace: {r}");
    let ws_id = r["data"]["id"].as_str().unwrap().to_string();

    let r = post_json(&c, &format!("{base}/v1/workspaces/{ws_id}/keys"), &api_key,
        serde_json::json!({})).await;
    assert!(r["data"]["key"].is_string(), "create ws key: {r}");
    let wk = r["data"]["key"].as_str().unwrap().to_string();

    // Write file
    let resp = c.put(format!("{base}/v1/fs/docs/hello.txt"))
        .header("Authorization", format!("Bearer {wk}"))
        .body("Hello Veda!").send().await.unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    assert!(status.is_success(), "write file {status}: {text}");
    let r: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(r["data"]["revision"], 1);

    // Read file
    let resp = c.get(format!("{base}/v1/fs/docs/hello.txt"))
        .header("Authorization", format!("Bearer {wk}"))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "read file");
    assert_eq!(resp.text().await.unwrap(), "Hello Veda!");

    // List directory
    let resp = c.get(format!("{base}/v1/fs/docs?list"))
        .header("Authorization", format!("Bearer {wk}"))
        .send().await.unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    assert!(status.is_success(), "list dir {status}: {text}");
    let r: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(r["data"].as_array().unwrap().iter().any(|e| e["name"] == "hello.txt"));

    // Delete file
    let resp = c.delete(format!("{base}/v1/fs/docs/hello.txt"))
        .header("Authorization", format!("Bearer {wk}"))
        .send().await.unwrap();
    assert!(resp.status().is_success(), "delete file");

    // Verify deleted
    let resp = c.get(format!("{base}/v1/fs/docs/hello.txt"))
        .header("Authorization", format!("Bearer {wk}"))
        .send().await.unwrap();
    assert_eq!(resp.status(), 404);
}
