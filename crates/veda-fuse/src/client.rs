use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ApiResponse<T> {
    pub success: bool,
    pub data: Option<T>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileInfo {
    pub path: String,
    pub is_dir: bool,
    pub size_bytes: Option<i64>,
    pub revision: Option<i32>,
    // Server returns RFC3339 strings via chrono::DateTime<Utc>'s default serde
    // impl. Optional so unauthenticated/legacy responses still parse, but
    // when present they let the FUSE layer report real mtime/ctime instead
    // of `now`, which makes `make`-style timestamp tests work correctly.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

/// Which L*-level summary layer the FUSE sidecar is fetching.
/// Maps to the server endpoints `/v1/abstract/{path}` (L0, one
/// sentence) and `/v1/overview/{path}` (L1, ~2k tokens).
#[derive(Debug, Clone, Copy)]
pub enum SummaryKind {
    Abstract,
    Overview,
}

impl SummaryKind {
    fn endpoint(self) -> &'static str {
        match self {
            SummaryKind::Abstract => "abstract",
            SummaryKind::Overview => "overview",
        }
    }
}

/// Tri-state outcome of the summary endpoint, modelling the
/// server's 200 / 202 / 501 / 404 contract. `NotFound` collapses
/// both "path doesn't exist in this workspace" and "server can't
/// route to that path" (e.g. root, see `get_summary` comment).
#[derive(Debug, Clone)]
pub enum SidecarOutcome {
    /// Server returned 200 with a summary body.
    Body(Vec<u8>),
    /// 202 — summary generation is enqueued but not done yet.
    /// Caller renders a placeholder so `cat` sees a hint rather
    /// than an opaque empty file or an error.
    Pending,
    /// 501 — server has no `[llm]` configured; this workspace
    /// will never have summaries. Caller hides the sidecar (it
    /// effectively doesn't exist for this deployment).
    Disabled,
    /// 404 — the path itself doesn't exist in the workspace, or
    /// (as a known edge case) the server doesn't route summary
    /// requests at the workspace root.
    NotFound,
}

/// Server-side `SummaryResponse` is a union of two body shapes
/// (one per endpoint). Both fields are optional so a single
/// deserializer can handle either path. The field that's set
/// depends on the endpoint we hit.
#[derive(Debug, Deserialize)]
struct SummaryResponseRaw {
    #[serde(default)]
    l0_abstract: Option<String>,
    #[serde(default)]
    l1_overview: Option<String>,
}

/// Subset of `/capabilities` we currently care about. Forward-
/// compatible: unknown fields are ignored, so a server that grows
/// the response can ship before the client catches up.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub summary_enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size_bytes: Option<i64>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug)]
pub enum ClientError {
    NotFound,
    AlreadyExists,
    PermissionDenied,
    Conflict,
    Network(String),
    Server(u16, String),
    Parse(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "not found"),
            Self::AlreadyExists => write!(f, "already exists"),
            Self::PermissionDenied => write!(f, "permission denied"),
            Self::Conflict => write!(f, "conflict"),
            Self::Network(s) => write!(f, "network: {s}"),
            Self::Server(code, s) => write!(f, "HTTP {code}: {s}"),
            Self::Parse(s) => write!(f, "parse: {s}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, ClientError>;

pub struct VedaClient {
    base: String,
    key: String,
    http: reqwest::blocking::Client,
}

impl VedaClient {
    pub fn new(base_url: &str, key: &str) -> Self {
        Self {
            base: base_url.trim_end_matches('/').to_string(),
            key: key.to_string(),
            http: reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(30))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
        }
    }

    fn map_error_status(status: reqwest::StatusCode, body: &str) -> ClientError {
        match status.as_u16() {
            404 => ClientError::NotFound,
            409 => ClientError::AlreadyExists,
            403 => ClientError::PermissionDenied,
            412 => ClientError::Conflict,
            code => ClientError::Server(code, body.to_string()),
        }
    }

    fn check_status(status: reqwest::StatusCode, body: &str) -> Result<()> {
        if status.is_success() {
            return Ok(());
        }
        Err(Self::map_error_status(status, body))
    }

    /// Send request, drain body as text, return (status, body). Network errors become Network.
    fn send_text(req: reqwest::blocking::RequestBuilder) -> Result<(reqwest::StatusCode, String)> {
        let resp = req.send().map_err(|e| ClientError::Network(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().map_err(|e| ClientError::Network(e.to_string()))?;
        Ok((status, body))
    }

    pub fn stat(&self, path: &str) -> Result<FileInfo> {
        let path = path.trim_start_matches('/');
        let url = if path.is_empty() {
            format!("{}/v1/fs?stat", self.base)
        } else {
            format!("{}/v1/fs/{path}?stat", self.base)
        };
        let (status, body) = Self::send_text(self.http.get(&url).bearer_auth(&self.key))?;
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ClientError::NotFound);
        }
        Self::check_status(status, &body)?;
        let api: ApiResponse<FileInfo> =
            serde_json::from_str(&body).map_err(|e| ClientError::Parse(e.to_string()))?;
        api.data.ok_or_else(|| ClientError::Parse("no data in response".into()))
    }

    pub fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let path = path.trim_start_matches('/');
        let resp = self.http.get(format!("{}/v1/fs/{path}", self.base))
            .bearer_auth(&self.key)
            .send()
            .map_err(|e| ClientError::Network(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            return Err(Self::map_error_status(status, &body));
        }
        resp.bytes()
            .map(|b| b.to_vec())
            .map_err(|e| ClientError::Network(e.to_string()))
    }

    pub fn write_file(
        &self,
        path: &str,
        content: &[u8],
        expected_rev: Option<i32>,
    ) -> Result<Option<i32>> {
        use sha2::{Digest, Sha256};
        let path = path.trim_start_matches('/');
        let digest = {
            let mut h = Sha256::new();
            h.update(content);
            format!("{:x}", h.finalize())
        };
        let mut req = self.http.put(format!("{}/v1/fs/{path}", self.base))
            .bearer_auth(&self.key)
            .header("If-None-Match", format!("\"{digest}\""))
            .body(content.to_vec());
        if let Some(rev) = expected_rev {
            req = req.header("If-Match", format!("\"{rev}\""));
        }
        let resp = req.send().map_err(|e| ClientError::Network(e.to_string()))?;
        let status = resp.status();
        let etag = resp.headers().get("ETag")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().trim_matches('"').parse::<i32>().ok());
        let body = resp.text().map_err(|e| ClientError::Network(e.to_string()))?;
        Self::check_status(status, &body)?;
        Ok(etag)
    }

    pub fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>> {
        let path = path.trim_start_matches('/');
        let url = if path.is_empty() {
            format!("{}/v1/fs?list", self.base)
        } else {
            format!("{}/v1/fs/{path}?list", self.base)
        };
        let (status, body) = Self::send_text(self.http.get(&url).bearer_auth(&self.key))?;
        Self::check_status(status, &body)?;
        let api: ApiResponse<Vec<DirEntry>> =
            serde_json::from_str(&body).map_err(|e| ClientError::Parse(e.to_string()))?;
        Ok(api.data.unwrap_or_default())
    }

    pub fn delete(&self, path: &str) -> Result<()> {
        let path = path.trim_start_matches('/');
        let url = if path.is_empty() {
            format!("{}/v1/fs", self.base)
        } else {
            format!("{}/v1/fs/{path}", self.base)
        };
        let (status, body) = Self::send_text(self.http.delete(&url).bearer_auth(&self.key))?;
        Self::check_status(status, &body)
    }

    pub fn mkdir(&self, path: &str) -> Result<()> {
        let req = self.http.post(format!("{}/v1/fs-mkdir", self.base))
            .bearer_auth(&self.key)
            .json(&serde_json::json!({"path": path}));
        let (status, body) = Self::send_text(req)?;
        Self::check_status(status, &body)
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let req = self.http.post(format!("{}/v1/fs-rename", self.base))
            .bearer_auth(&self.key)
            .json(&serde_json::json!({"from": from, "to": to}));
        let (status, body) = Self::send_text(req)?;
        Self::check_status(status, &body)
    }

    /// Probe the server's `/capabilities` endpoint. Used at FUSE
    /// mount-init time to decide whether to advertise summary
    /// sidecars in `readdir`. Unauthenticated — the endpoint
    /// reports only public bits about server config. Lives outside
    /// the `/v1/*` namespace on purpose so a hardened reverse proxy
    /// that gates `/v1/*` on auth still lets the probe through (same
    /// reasoning as `/healthz`). On any error the FUSE layer defaults
    /// to "assume the feature is on" and relies on the per-directory
    /// ENOENT cache to suppress phantom entries — see `fs.rs`
    /// `sidecar_recently_missing`.
    pub fn get_capabilities(&self) -> Result<Capabilities> {
        let url = format!("{}/capabilities", self.base);
        let (status, body) = Self::send_text(self.http.get(&url))?;
        Self::check_status(status, &body)?;
        let api: ApiResponse<Capabilities> =
            serde_json::from_str(&body).map_err(|e| ClientError::Parse(e.to_string()))?;
        api.data
            .ok_or_else(|| ClientError::Parse("capabilities response missing data".into()))
    }

    /// Fetch the L0 or L1 summary for `path` from the server.
    ///
    /// Returns a [`SidecarOutcome`] rather than `Result<Vec<u8>>`
    /// because the three "no body" cases (pending / disabled /
    /// not-found) drive different FUSE semantics: pending must
    /// still render a one-line placeholder, disabled must hide
    /// the sidecar entirely, not-found is just ENOENT. Anything
    /// outside that contract becomes a regular `ClientError`.
    ///
    /// Edge case: the server's wildcard path matcher
    /// (`/v1/{endpoint}/{*path}`) refuses to match an empty
    /// segment, and there is no separate root-summary route yet
    /// (the workspace root has no dentry to anchor a summary row
    /// against). Return `NotFound` early so the FUSE mount-root
    /// sidecar lookup becomes ENOENT without an HTTP round-trip
    /// and without depending on the server's 404 behaviour. Lift
    /// when workspace-level summaries become a first-class feature.
    pub fn get_summary(&self, path: &str, kind: SummaryKind) -> Result<SidecarOutcome> {
        let path = path.trim_start_matches('/');
        if path.is_empty() {
            return Ok(SidecarOutcome::NotFound);
        }
        let url = format!("{}/v1/{}/{path}", self.base, kind.endpoint());
        let (status, body) = Self::send_text(self.http.get(&url).bearer_auth(&self.key))?;
        match status.as_u16() {
            200 => {
                let api: ApiResponse<SummaryResponseRaw> = serde_json::from_str(&body)
                    .map_err(|e| ClientError::Parse(e.to_string()))?;
                let data = api
                    .data
                    .ok_or_else(|| ClientError::Parse("summary response missing data".into()))?;
                let text = match kind {
                    SummaryKind::Abstract => data.l0_abstract.unwrap_or_default(),
                    SummaryKind::Overview => data.l1_overview.unwrap_or_default(),
                };
                Ok(SidecarOutcome::Body(text.into_bytes()))
            }
            202 => Ok(SidecarOutcome::Pending),
            501 => Ok(SidecarOutcome::Disabled),
            404 => Ok(SidecarOutcome::NotFound),
            code => Err(ClientError::Server(code, body)),
        }
    }

    pub fn read_range(&self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }
        let path = path.trim_start_matches('/');
        let end = offset + length - 1;
        let resp = self.http.get(format!("{}/v1/fs/{path}", self.base))
            .bearer_auth(&self.key)
            .header("Range", format!("bytes={offset}-{end}"))
            .send()
            .map_err(|e| ClientError::Network(e.to_string()))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ClientError::NotFound);
        }
        if !status.is_success() && status.as_u16() != 206 {
            let body = resp.text().unwrap_or_default();
            return Err(ClientError::Server(status.as_u16(), body));
        }
        resp.bytes()
            .map(|b| b.to_vec())
            .map_err(|e| ClientError::Network(e.to_string()))
    }
}

#[cfg(test)]
mod summary_tests {
    //! Pins the server's summary endpoint status contract
    //! (200 ready / 202 pending / 501 disabled / 404 absent) onto
    //! the `SidecarOutcome` variants the FUSE layer relies on.
    //!
    //! The production client is `reqwest::blocking`, which spawns
    //! its own internal Tokio current-thread runtime; pairing that
    //! with `wiremock` (async-only) triggers a "cannot drop a
    //! runtime in a context where blocking is not allowed" panic.
    //! We side-step the whole thing by speaking raw HTTP/1.1 on a
    //! one-shot `TcpListener`, the same approach as
    //! `tests/binary_roundtrip.rs`.
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    /// Spawn an HTTP listener that answers exactly one request with
    /// `(status, body)` and shuts down. Returns `(base_url,
    /// captured_request_path)`. The captured path lets a test pin
    /// what URL the client actually built — important for the
    /// no-path workspace-root branch.
    fn spawn_status_mock(
        status: u16,
        reason: &'static str,
        body: String,
        content_type: &'static str,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(false).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .ok();
                let mut hb: Vec<u8> = Vec::with_capacity(2048);
                let mut chunk = [0u8; 1024];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            hb.extend_from_slice(&chunk[..n]);
                            if hb.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                // First line: "GET /path HTTP/1.1" — capture path so
                // the test can assert on it.
                let req_line = std::str::from_utf8(&hb)
                    .ok()
                    .and_then(|s| s.lines().next())
                    .unwrap_or("")
                    .to_string();
                let path = req_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                let _ = tx.send(path);
                let header = format!(
                    "HTTP/1.1 {status} {reason}\r\n\
                     Content-Type: {content_type}\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            }
        });
        (format!("http://127.0.0.1:{port}"), rx)
    }

    #[test]
    fn get_summary_200_returns_body_with_l0_text() {
        let resp_body = serde_json::json!({
            "success": true,
            "data": {
                "path": "/docs",
                "l0_abstract": "single-sentence summary",
            },
        })
        .to_string();
        let (base, path_rx) =
            spawn_status_mock(200, "OK", resp_body, "application/json");
        let client = VedaClient::new(&base, "wk-test");
        let outcome = client.get_summary("/docs", SummaryKind::Abstract).unwrap();
        let captured = path_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(captured, "/v1/abstract/docs");
        match outcome {
            SidecarOutcome::Body(b) => {
                assert_eq!(String::from_utf8(b).unwrap(), "single-sentence summary");
            }
            other => panic!("expected Body, got {other:?}"),
        }
    }

    #[test]
    fn get_summary_200_picks_overview_field_when_kind_is_overview() {
        // Same response shape (`data` envelope) is shared by both
        // endpoints, but the field we extract must match the
        // requested kind so a misrouted endpoint can't quietly
        // surface as an empty body.
        let resp_body = serde_json::json!({
            "success": true,
            "data": { "path": "/notes", "l1_overview": "long-form prose" },
        })
        .to_string();
        let (base, _rx) = spawn_status_mock(200, "OK", resp_body, "application/json");
        let client = VedaClient::new(&base, "wk-test");
        let outcome = client.get_summary("/notes", SummaryKind::Overview).unwrap();
        if let SidecarOutcome::Body(b) = outcome {
            assert_eq!(String::from_utf8(b).unwrap(), "long-form prose");
        } else {
            panic!("expected Body");
        }
    }

    #[test]
    fn get_summary_202_maps_to_pending() {
        let (base, _rx) = spawn_status_mock(202, "Accepted", "".into(), "text/plain");
        let client = VedaClient::new(&base, "wk-test");
        let outcome = client
            .get_summary("/queued", SummaryKind::Abstract)
            .unwrap();
        assert!(matches!(outcome, SidecarOutcome::Pending));
    }

    #[test]
    fn get_summary_501_maps_to_disabled() {
        let (base, _rx) =
            spawn_status_mock(501, "Not Implemented", "disabled".into(), "text/plain");
        let client = VedaClient::new(&base, "wk-test");
        let outcome = client
            .get_summary("/anywhere", SummaryKind::Abstract)
            .unwrap();
        assert!(matches!(outcome, SidecarOutcome::Disabled));
    }

    #[test]
    fn get_summary_404_maps_to_not_found() {
        let (base, _rx) = spawn_status_mock(404, "Not Found", "absent".into(), "text/plain");
        let client = VedaClient::new(&base, "wk-test");
        let outcome = client.get_summary("/ghost", SummaryKind::Abstract).unwrap();
        assert!(matches!(outcome, SidecarOutcome::NotFound));
    }

    #[test]
    fn get_summary_root_short_circuits_to_not_found_without_http() {
        // Workspace root has no dentry, so the server has no
        // summary row to return. Asking would always 404 — but
        // the previous implementation routed root to a separate
        // no-path endpoint that itself returned 404, masquerading
        // as a valid feature. Now the client refuses to make the
        // request at all. A mock that would otherwise serve 200
        // is mounted with expect(0)-style assertion: if any HTTP
        // call goes out, `path_rx.recv_timeout` succeeds and the
        // test fails.
        let resp_body = serde_json::json!({ "ok": true }).to_string();
        let (base, path_rx) =
            spawn_status_mock(200, "OK", resp_body, "application/json");
        let client = VedaClient::new(&base, "wk-test");
        let outcome = client.get_summary("/", SummaryKind::Abstract).unwrap();
        assert!(
            matches!(outcome, SidecarOutcome::NotFound),
            "root must map to NotFound, got {outcome:?}"
        );
        // No HTTP call should have happened — the mock's path
        // channel must be empty. recv_timeout returns err on
        // timeout, which is what we want.
        assert!(
            path_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "root summary lookup must not hit the server"
        );
    }

    // ── capabilities ───────────────────────────────────────────────

    #[test]
    fn get_capabilities_parses_summary_enabled_true() {
        let resp_body = serde_json::json!({
            "success": true,
            "data": { "summary_enabled": true },
        })
        .to_string();
        let (base, path_rx) =
            spawn_status_mock(200, "OK", resp_body, "application/json");
        let client = VedaClient::new(&base, "wk-test");
        let caps = client.get_capabilities().unwrap();
        assert!(caps.summary_enabled);
        let captured = path_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(captured, "/capabilities");
    }

    #[test]
    fn get_capabilities_parses_summary_enabled_false() {
        let resp_body = serde_json::json!({
            "success": true,
            "data": { "summary_enabled": false },
        })
        .to_string();
        let (base, _rx) = spawn_status_mock(200, "OK", resp_body, "application/json");
        let client = VedaClient::new(&base, "wk-test");
        let caps = client.get_capabilities().unwrap();
        assert!(!caps.summary_enabled);
    }

    #[test]
    fn get_capabilities_unknown_fields_ignored() {
        // Forward-compat: server may grow new capability bits; the
        // client must keep parsing successfully so an old fuse binary
        // talking to a newer server doesn't disable sidecars by
        // accident.
        let resp_body = serde_json::json!({
            "success": true,
            "data": {
                "summary_enabled": true,
                "future_feature_x": "yes",
                "version": 7,
            },
        })
        .to_string();
        let (base, _rx) = spawn_status_mock(200, "OK", resp_body, "application/json");
        let client = VedaClient::new(&base, "wk-test");
        let caps = client.get_capabilities().unwrap();
        assert!(caps.summary_enabled);
    }

    #[test]
    fn get_summary_500_surfaces_as_server_error() {
        // Anything outside the documented 200/202/501/404 contract
        // becomes a regular ClientError so the FUSE layer can map
        // it to EIO instead of inventing a sidecar state.
        let (base, _rx) =
            spawn_status_mock(500, "Internal Server Error", "db down".into(), "text/plain");
        let client = VedaClient::new(&base, "wk-test");
        let err = client.get_summary("/x", SummaryKind::Abstract).unwrap_err();
        assert!(matches!(err, ClientError::Server(500, _)), "got {err:?}");
    }
}
