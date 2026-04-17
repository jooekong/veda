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
    Io(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "not found"),
            Self::AlreadyExists => write!(f, "already exists"),
            Self::PermissionDenied => write!(f, "permission denied"),
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

    fn check_status(status: reqwest::StatusCode, body: &str) -> Result<()> {
        if status.is_success() {
            return Ok(());
        }
        match status.as_u16() {
            404 => Err(ClientError::NotFound),
            409 => Err(ClientError::AlreadyExists),
            403 => Err(ClientError::PermissionDenied),
            _ => Err(ClientError::Io(format!("HTTP {status}: {body}"))),
        }
    }

    pub fn stat(&self, path: &str) -> Result<FileInfo> {
        let path = path.trim_start_matches('/');
        let url = if path.is_empty() {
            format!("{}/v1/fs/.?stat", self.base)
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

    pub fn read_file(&self, path: &str) -> Result<String> {
        let path = path.trim_start_matches('/');
        let resp = self.http.get(format!("{}/v1/fs/{path}", self.base))
            .bearer_auth(&self.key)
            .send()
            .map_err(|e| ClientError::Io(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().map_err(|e| ClientError::Io(e.to_string()))?;
        Self::check_status(status, &body)?;
        Ok(body)
    }

    pub fn write_file(&self, path: &str, content: &[u8]) -> Result<()> {
        let path = path.trim_start_matches('/');
        let resp = self.http.put(format!("{}/v1/fs/{path}", self.base))
            .bearer_auth(&self.key)
            .body(content.to_vec())
            .send()
            .map_err(|e| ClientError::Io(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().map_err(|e| ClientError::Io(e.to_string()))?;
        Self::check_status(status, &body)
    }

    pub fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>> {
        let path = path.trim_start_matches('/');
        let url = if path.is_empty() {
            format!("{}/v1/fs/.?list", self.base)
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
        let resp = self.http.delete(format!("{}/v1/fs/{path}", self.base))
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
}
