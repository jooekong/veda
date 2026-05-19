use std::time::Duration;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// HTTP error preserving the server's status code so callers can match
/// on specific codes via [`status_code`]. Wrap with `.context()` as
/// usual — the chain walk recovers `ApiError` through any layers of
/// context wrapping.
#[derive(Debug)]
pub struct ApiError {
    pub status: reqwest::StatusCode,
    pub body: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}: {}", self.status, self.body)
    }
}

impl std::error::Error for ApiError {}

/// Pull a status code out of an anyhow error chain, if any link is an
/// `ApiError`. Returns `None` for network/parse errors so callers can
/// distinguish "server said 409" from "couldn't reach server."
pub fn status_code(err: &anyhow::Error) -> Option<u16> {
    err.chain()
        .find_map(|e| e.downcast_ref::<ApiError>())
        .map(|a| a.status.as_u16())
}

pub struct Client {
    base: String,
    http: reqwest::Client,
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

impl Client {
    pub fn new(base_url: &str) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            base: base_url.trim_end_matches('/').to_string(),
            http,
        }
    }

    async fn check(resp: reqwest::Response) -> Result<serde_json::Value> {
        let status = resp.status();
        let text = resp.text().await.context("read response body")?;
        if !status.is_success() {
            return Err(ApiError { status, body: text }.into());
        }
        serde_json::from_str(&text).context("parse JSON response")
    }

    pub async fn create_account(
        &self,
        name: &str,
        email: &str,
        password: &str,
    ) -> Result<serde_json::Value> {
        let resp = self
            .http
            .post(format!("{}/v1/accounts", self.base))
            .json(&serde_json::json!({"name": name, "email": email, "password": password}))
            .send()
            .await?;
        Self::check(resp).await
    }

    pub async fn login(&self, email: &str, password: &str) -> Result<serde_json::Value> {
        let resp = self
            .http
            .post(format!("{}/v1/accounts/login", self.base))
            .json(&serde_json::json!({"email": email, "password": password}))
            .send()
            .await?;
        Self::check(resp).await
    }

    /// Zero-input onboarding: server mints account + vk_ + default
    /// workspace + wk_ in one shot. Used by `veda init` when no
    /// `--email` is given.
    pub async fn anonymous_onboard(&self) -> Result<serde_json::Value> {
        let resp = self
            .http
            .post(format!("{}/v1/accounts/anonymous", self.base))
            .send()
            .await?;
        Self::check(resp).await
    }

    /// Upgrade the current anonymous account (identified by `api_key`)
    /// to a named one. The same `api_key` keeps working after the
    /// claim. `name` is optional — `None` keeps the auto-generated
    /// `anon-xxxx`.
    pub async fn claim_account(
        &self,
        api_key: &str,
        email: &str,
        password: &str,
        name: Option<&str>,
    ) -> Result<serde_json::Value> {
        let mut body = serde_json::json!({"email": email, "password": password});
        if let Some(n) = name {
            body["name"] = serde_json::Value::String(n.to_string());
        }
        let resp = self
            .http
            .post(format!("{}/v1/accounts/claim", self.base))
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await?;
        Self::check(resp).await
    }

    pub async fn create_workspace(&self, api_key: &str, name: &str) -> Result<serde_json::Value> {
        let resp = self
            .http
            .post(format!("{}/v1/workspaces", self.base))
            .bearer_auth(api_key)
            .json(&serde_json::json!({"name": name}))
            .send()
            .await?;
        Self::check(resp).await
    }

    pub async fn list_workspaces(&self, api_key: &str) -> Result<serde_json::Value> {
        let resp = self
            .http
            .get(format!("{}/v1/workspaces", self.base))
            .bearer_auth(api_key)
            .send()
            .await?;
        Self::check(resp).await
    }

    pub async fn create_workspace_key(
        &self,
        api_key: &str,
        ws_id: &str,
    ) -> Result<serde_json::Value> {
        let resp = self
            .http
            .post(format!("{}/v1/workspaces/{ws_id}/keys", self.base))
            .bearer_auth(api_key)
            .json(&serde_json::json!({"name": "cli"}))
            .send()
            .await?;
        Self::check(resp).await
    }

    pub async fn write_file(
        &self,
        ws_key: &str,
        path: &str,
        content: &str,
    ) -> Result<serde_json::Value> {
        let path = path.trim_start_matches('/');
        // Pre-hash and send If-None-Match so the server can short-circuit the
        // write when the content already matches what's stored (no dedup of
        // chunks, no revision bump).
        let digest = sha256_hex(content.as_bytes());
        let resp = self
            .http
            .put(format!("{}/v1/fs/{path}", self.base))
            .bearer_auth(ws_key)
            .header("If-None-Match", format!("\"{digest}\""))
            .body(content.to_string())
            .send()
            .await?;
        Self::check(resp).await
    }

    pub async fn append_file(
        &self,
        ws_key: &str,
        path: &str,
        content: &str,
    ) -> Result<serde_json::Value> {
        let path = path.trim_start_matches('/');
        let resp = self
            .http
            .post(format!("{}/v1/fs/{path}", self.base))
            .bearer_auth(ws_key)
            .body(content.to_string())
            .send()
            .await?;
        Self::check(resp).await
    }

    pub async fn read_file(&self, ws_key: &str, path: &str, lines: Option<&str>) -> Result<String> {
        let path = path.trim_start_matches('/');
        let mut url = format!("{}/v1/fs/{path}", self.base);
        if let Some(l) = lines {
            url.push_str(&format!("?lines={l}"));
        }
        let resp = self.http.get(&url).bearer_auth(ws_key).send().await?;
        if !resp.status().is_success() {
            let text = resp.text().await?;
            bail!("read failed: {text}");
        }
        Ok(resp.text().await?)
    }

    pub async fn list_dir(&self, ws_key: &str, path: &str) -> Result<serde_json::Value> {
        let path = path.trim_start_matches('/');
        let url = if path.is_empty() {
            format!("{}/v1/fs?list", self.base)
        } else {
            format!("{}/v1/fs/{path}?list", self.base)
        };
        let resp = self.http.get(&url).bearer_auth(ws_key).send().await?;
        Self::check(resp).await
    }

    pub async fn rename_file(
        &self,
        ws_key: &str,
        src: &str,
        dst: &str,
    ) -> Result<serde_json::Value> {
        let resp = self
            .http
            .post(format!("{}/v1/fs-rename", self.base))
            .bearer_auth(ws_key)
            .json(&serde_json::json!({"from": src, "to": dst}))
            .send()
            .await?;
        Self::check(resp).await
    }

    pub async fn delete_file(&self, ws_key: &str, path: &str) -> Result<serde_json::Value> {
        let path = path.trim_start_matches('/');
        let url = if path.is_empty() {
            format!("{}/v1/fs", self.base)
        } else {
            format!("{}/v1/fs/{path}", self.base)
        };
        let resp = self.http.delete(&url).bearer_auth(ws_key).send().await?;
        Self::check(resp).await
    }

    pub async fn mkdir(&self, ws_key: &str, path: &str) -> Result<serde_json::Value> {
        let resp = self
            .http
            .post(format!("{}/v1/fs-mkdir", self.base))
            .bearer_auth(ws_key)
            .json(&serde_json::json!({"path": path}))
            .send()
            .await?;
        Self::check(resp).await
    }

    pub async fn search(
        &self,
        ws_key: &str,
        query: &str,
        mode: &str,
        limit: usize,
        detail_level: &str,
        path_prefix: Option<&str>,
    ) -> Result<serde_json::Value> {
        let mut body = serde_json::json!({
            "query": query,
            "mode": mode,
            "limit": limit,
            "detail_level": detail_level,
        });
        if let Some(p) = path_prefix {
            body["path_prefix"] = serde_json::Value::String(p.to_string());
        }
        let resp = self
            .http
            .post(format!("{}/v1/search", self.base))
            .bearer_auth(ws_key)
            .json(&body)
            .send()
            .await?;
        Self::check(resp).await
    }

    pub async fn grep(
        &self,
        ws_key: &str,
        pattern: &str,
        path_prefix: Option<&str>,
        ignore_case: bool,
        max_results: usize,
    ) -> Result<serde_json::Value> {
        let mut body = serde_json::json!({
            "pattern": pattern,
            "ignore_case": ignore_case,
            "max_results": max_results,
        });
        if let Some(p) = path_prefix {
            body["path_prefix"] = serde_json::Value::String(p.to_string());
        }
        let resp = self
            .http
            .post(format!("{}/v1/grep", self.base))
            .bearer_auth(ws_key)
            .json(&body)
            .send()
            .await?;
        Self::check(resp).await
    }

    /// Returns (status_code, body). Both summary endpoints have the same
    /// three-state contract (200 ready / 202 pending / 501 disabled) that
    /// the CLI renders differently. `check()` returns only the body on
    /// success, so we sidestep it to keep the status code on every branch.
    pub async fn get_summary_layer(
        &self,
        ws_key: &str,
        path: &str,
        endpoint: &str,
    ) -> Result<(u16, serde_json::Value)> {
        let path = path.trim_start_matches('/');
        let resp = self
            .http
            .get(format!("{}/v1/{endpoint}/{path}", self.base))
            .bearer_auth(ws_key)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let text = resp.text().await.context("read response body")?;
        let body: serde_json::Value =
            serde_json::from_str(&text).context("parse JSON response")?;
        Ok((status, body))
    }

    pub async fn create_collection(
        &self,
        ws_key: &str,
        name: &str,
        schema: &serde_json::Value,
        embed_source: Option<&str>,
    ) -> Result<serde_json::Value> {
        let mut body = serde_json::json!({
            "name": name,
            "collection_type": "structured",
            "fields": schema,
        });
        if let Some(src) = embed_source {
            body["embedding_source"] = serde_json::Value::String(src.to_string());
        }
        let resp = self
            .http
            .post(format!("{}/v1/collections", self.base))
            .bearer_auth(ws_key)
            .json(&body)
            .send()
            .await?;
        Self::check(resp).await
    }

    pub async fn list_collections(&self, ws_key: &str) -> Result<serde_json::Value> {
        let resp = self
            .http
            .get(format!("{}/v1/collections", self.base))
            .bearer_auth(ws_key)
            .send()
            .await?;
        Self::check(resp).await
    }

    pub async fn describe_collection(&self, ws_key: &str, name: &str) -> Result<serde_json::Value> {
        let resp = self
            .http
            .get(format!("{}/v1/collections/{name}", self.base))
            .bearer_auth(ws_key)
            .send()
            .await?;
        Self::check(resp).await
    }

    pub async fn delete_collection(&self, ws_key: &str, name: &str) -> Result<serde_json::Value> {
        let resp = self
            .http
            .delete(format!("{}/v1/collections/{name}", self.base))
            .bearer_auth(ws_key)
            .send()
            .await?;
        Self::check(resp).await
    }

    pub async fn insert_rows(
        &self,
        ws_key: &str,
        name: &str,
        rows: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let resp = self
            .http
            .post(format!("{}/v1/collections/{name}/rows", self.base))
            .bearer_auth(ws_key)
            .json(&serde_json::json!({"rows": rows}))
            .send()
            .await?;
        Self::check(resp).await
    }

    pub async fn search_collection(
        &self,
        ws_key: &str,
        name: &str,
        query: &str,
        limit: usize,
    ) -> Result<serde_json::Value> {
        let resp = self
            .http
            .post(format!("{}/v1/collections/{name}/search", self.base))
            .bearer_auth(ws_key)
            .json(&serde_json::json!({"query": query, "limit": limit}))
            .send()
            .await?;
        Self::check(resp).await
    }

    pub async fn execute_sql(&self, ws_key: &str, sql: &str) -> Result<serde_json::Value> {
        let resp = self
            .http
            .post(format!("{}/v1/sql", self.base))
            .bearer_auth(ws_key)
            .json(&serde_json::json!({"sql": sql}))
            .send()
            .await?;
        Self::check(resp).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn api_error_display_includes_status_and_body() {
        let err = ApiError {
            status: reqwest::StatusCode::CONFLICT,
            body: "email already registered".into(),
        };
        let s = err.to_string();
        assert!(s.contains("409"), "got: {s}");
        assert!(s.contains("email already registered"), "got: {s}");
    }

    #[test]
    fn status_code_extracts_through_context_wrap() {
        // Real-world shape: ApiError wrapped by .context() in init.rs.
        // Pins that downcast still finds it after wrapping.
        let api = ApiError {
            status: reqwest::StatusCode::CONFLICT,
            body: "x".into(),
        };
        let err: anyhow::Error = anyhow::Error::from(api).context("account creation failed");
        assert_eq!(status_code(&err), Some(409));
    }

    #[test]
    fn status_code_returns_none_for_non_http_error() {
        let err: anyhow::Error = anyhow::anyhow!("network glitch");
        assert_eq!(status_code(&err), None);
    }

    #[tokio::test]
    async fn check_2xx_returns_parsed_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})),
            )
            .mount(&server)
            .await;
        let resp = reqwest::get(server.uri()).await.unwrap();
        let v = Client::check(resp).await.unwrap();
        assert_eq!(v["ok"], true);
    }

    #[tokio::test]
    async fn check_4xx_returns_apierror_with_status_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(409).set_body_string("email taken"))
            .mount(&server)
            .await;
        let resp = reqwest::get(server.uri()).await.unwrap();
        let err = Client::check(resp).await.unwrap_err();
        assert_eq!(status_code(&err), Some(409));
        let api: &ApiError = err.downcast_ref().expect("err should carry ApiError");
        assert!(api.body.contains("email taken"), "got: {}", api.body);
    }

    #[tokio::test]
    async fn check_5xx_also_carries_status() {
        // Server errors must be distinguishable from client errors so
        // callers can decide retry vs surface-to-user.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
            .mount(&server)
            .await;
        let resp = reqwest::get(server.uri()).await.unwrap();
        let err = Client::check(resp).await.unwrap_err();
        assert_eq!(status_code(&err), Some(503));
    }
}
