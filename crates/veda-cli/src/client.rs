use std::time::Duration;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

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
            bail!("HTTP {status}: {text}");
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
    ) -> Result<serde_json::Value> {
        let resp = self
            .http
            .post(format!("{}/v1/search", self.base))
            .bearer_auth(ws_key)
            .json(&serde_json::json!({
                "query": query,
                "mode": mode,
                "limit": limit,
                "detail_level": detail_level,
            }))
            .send()
            .await?;
        Self::check(resp).await
    }

    pub async fn get_summary(&self, ws_key: &str, path: &str) -> Result<serde_json::Value> {
        let path = path.trim_start_matches('/');
        let resp = self
            .http
            .get(format!("{}/v1/summary/{path}", self.base))
            .bearer_auth(ws_key)
            .send()
            .await?;
        Self::check(resp).await
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
