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
}

#[derive(Debug, Clone, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size_bytes: Option<i64>,
}

#[derive(Debug)]
pub enum ClientError {
    NotFound,
    AlreadyExists,
    PermissionDenied,
    Conflict,
    Io(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "not found"),
            Self::AlreadyExists => write!(f, "already exists"),
            Self::PermissionDenied => write!(f, "permission denied"),
            Self::Conflict => write!(f, "conflict"),
            Self::Io(s) => write!(f, "{s}"),
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
            http: reqwest::blocking::Client::new(),
        }
    }

    /// Map an HTTP error status + body to a ClientError.
    /// Caller must have already confirmed the status is not success.
    fn map_error_status(status: reqwest::StatusCode, body: &str) -> ClientError {
        match status.as_u16() {
            404 => ClientError::NotFound,
            409 => ClientError::AlreadyExists,
            403 => ClientError::PermissionDenied,
            412 => ClientError::Conflict,
            _ => ClientError::Io(format!("HTTP {status}: {body}")),
        }
    }

    fn check_status(status: reqwest::StatusCode, body: &str) -> Result<()> {
        if status.is_success() {
            return Ok(());
        }
        Err(Self::map_error_status(status, body))
    }

    pub fn stat(&self, path: &str) -> Result<FileInfo> {
        let path = path.trim_start_matches('/');
        let url = if path.is_empty() {
            format!("{}/v1/fs?stat", self.base)
        } else {
            format!("{}/v1/fs/{path}?stat", self.base)
        };
        let resp = self.http.get(&url)
            .bearer_auth(&self.key)
            .send()
            .map_err(|e| ClientError::Io(e.to_string()))?;
        let status = resp.status();

        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ClientError::NotFound);
        }

        let body = resp.text().map_err(|e| ClientError::Io(e.to_string()))?;
        Self::check_status(status, &body)?;

        let api: ApiResponse<FileInfo> =
            serde_json::from_str(&body).map_err(|e| ClientError::Io(e.to_string()))?;

        api.data.ok_or_else(|| ClientError::Io("no data in response".into()))
    }

    pub fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let path = path.trim_start_matches('/');
        let resp = self.http.get(format!("{}/v1/fs/{path}", self.base))
            .bearer_auth(&self.key)
            .send()
            .map_err(|e| ClientError::Io(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            // Error bodies are small + expected UTF-8; read as text for diagnostics.
            let body = resp.text().unwrap_or_default();
            return Err(Self::map_error_status(status, &body));
        }
        resp.bytes()
            .map(|b| b.to_vec())
            .map_err(|e| ClientError::Io(e.to_string()))
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
        let resp = req.send().map_err(|e| ClientError::Io(e.to_string()))?;
        let status = resp.status();
        let etag = resp.headers().get("ETag")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().trim_matches('"').parse::<i32>().ok());
        let body = resp.text().map_err(|e| ClientError::Io(e.to_string()))?;
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
        let resp = self.http.get(&url)
            .bearer_auth(&self.key)
            .send()
            .map_err(|e| ClientError::Io(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().map_err(|e| ClientError::Io(e.to_string()))?;
        Self::check_status(status, &body)?;

        let api: ApiResponse<Vec<DirEntry>> =
            serde_json::from_str(&body).map_err(|e| ClientError::Io(e.to_string()))?;

        Ok(api.data.unwrap_or_default())
    }

    pub fn delete(&self, path: &str) -> Result<()> {
        let path = path.trim_start_matches('/');
        let url = if path.is_empty() {
            format!("{}/v1/fs", self.base)
        } else {
            format!("{}/v1/fs/{path}", self.base)
        };
        let resp = self.http.delete(&url)
            .bearer_auth(&self.key)
            .send()
            .map_err(|e| ClientError::Io(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().map_err(|e| ClientError::Io(e.to_string()))?;
        Self::check_status(status, &body)
    }

    pub fn mkdir(&self, path: &str) -> Result<()> {
        let resp = self.http.post(format!("{}/v1/fs-mkdir", self.base))
            .bearer_auth(&self.key)
            .json(&serde_json::json!({"path": path}))
            .send()
            .map_err(|e| ClientError::Io(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().map_err(|e| ClientError::Io(e.to_string()))?;
        Self::check_status(status, &body)
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let resp = self.http.post(format!("{}/v1/fs-rename", self.base))
            .bearer_auth(&self.key)
            .json(&serde_json::json!({"from": from, "to": to}))
            .send()
            .map_err(|e| ClientError::Io(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().map_err(|e| ClientError::Io(e.to_string()))?;
        Self::check_status(status, &body)
    }

    /// Read a byte range from a file. Returns the raw bytes.
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
            .map_err(|e| ClientError::Io(e.to_string()))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ClientError::NotFound);
        }
        if !status.is_success() && status.as_u16() != 206 {
            return Err(ClientError::Io(format!("HTTP {status}")));
        }
        resp.bytes()
            .map(|b| b.to_vec())
            .map_err(|e| ClientError::Io(e.to_string()))
    }
}
