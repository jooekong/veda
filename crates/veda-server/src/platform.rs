//! Platform gateway context.
//!
//! The OnePaaS gateway forwards caller identity in a base64-encoded `user`
//! header and routes per `ai_workspace`. On direct (non-gateway) access these
//! are absent, so every field is optional and handlers fall back to native
//! `wk_` / `vk_` auth. Used by the `/v1/workspace/*` management surface to stamp
//! `creator` / `creator_name` and to drive the external authorization check.

use std::convert::Infallible;
use std::future::Future;
use std::sync::LazyLock;
use std::time::Duration;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use base64::Engine;
use serde::Deserialize;

/// Identity decoded from the gateway `user` header (base64 of a JSON blob).
/// Only the fields veda consumes are kept; the rest (empNo, mail, org, …) is
/// ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct PlatformUser {
    /// Domain account, e.g. `zhangzhenzhen01`. Stamped as `creator`.
    pub name: String,
    /// Chinese display name, e.g. `张振振`. Stamped as `creator_name`.
    #[serde(rename = "displayName", default)]
    pub display_name: String,
}

/// Optional gateway identity, extracted from the `user` header.
///
/// `Some` when the header is present and base64+JSON-decodes; `None` on direct
/// access (no header) or a malformed header. We never reject on it: direct
/// callers legitimately have no gateway identity, and authorization is enforced
/// separately by the external authz API (item 4) — this only carries *who* the
/// caller is, not *whether* they may act.
pub struct GatewayUser {
    user: Option<PlatformUser>,
    /// Raw `Cookie` header, forwarded to the platform APIs (authz / workspace
    /// lookup) so veda acts on behalf of the calling user.
    cookie: Option<String>,
}

impl GatewayUser {
    /// Domain account for `creator` / the authz `user` param, if present.
    pub fn creator(&self) -> Option<String> {
        self.user.as_ref().map(|u| u.name.clone())
    }

    /// Chinese display name for `creator_name`, if present and non-empty.
    pub fn creator_name(&self) -> Option<String> {
        self.user
            .as_ref()
            .filter(|u| !u.display_name.is_empty())
            .map(|u| u.display_name.clone())
    }

    /// Domain account as a borrow, for the authz `user` query param.
    pub fn user_name(&self) -> Option<&str> {
        self.user.as_ref().map(|u| u.name.as_str())
    }

    /// Raw `Cookie` header to forward to platform APIs.
    pub fn cookie(&self) -> Option<&str> {
        self.cookie.as_deref()
    }
}

impl<S: Send + Sync> FromRequestParts<S> for GatewayUser {
    type Rejection = Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let user = decode_user_header(parts);
        let cookie = parts
            .headers
            .get("cookie")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        async move { Ok(GatewayUser { user, cookie }) }
    }
}

/// Base64-decode the `user` header and parse the identity JSON. Any failure
/// (absent header, non-ASCII, bad base64, bad JSON) yields `None` — the caller
/// treats it as "no gateway identity" and falls back to native auth.
fn decode_user_header(parts: &Parts) -> Option<PlatformUser> {
    let raw = parts.headers.get("user")?.to_str().ok()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(raw.trim())
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Shared HTTP client for platform calls, with a hard timeout so a hung gateway
/// (accepts but never responds) can't park request tasks forever. `authorize`
/// fails closed and `resolve_workspace_name` returns `None` on timeout.
static PLATFORM_HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .expect("build platform HTTP client")
});

/// Platform API base, e.g. `https://paas-api-test.ddmc-inc.com/proxy/llm`. Read
/// from `VEDA_PLATFORM_BASE`. When unset, external authz is **not enforced** and
/// workspace-name lookup is skipped — so dev / integration without the platform
/// configured behaves as before.
fn platform_base() -> Option<String> {
    std::env::var("VEDA_PLATFORM_BASE")
        .ok()
        .filter(|s| !s.is_empty())
}

/// External authorization (item 4): check the caller may perform `action` in
/// `workspace`, forwarding the request `cookie` so the platform authenticates
/// the call as that user. Fail-closed — missing cookie/user, non-200, or any
/// transport error all deny (`PermissionDenied` → 403); no fallback. Skipped
/// (allowed) when `VEDA_PLATFORM_BASE` is unset. The platform registers a single
/// `workspace-create` action that gates all resource creation (verified: 200
/// `{}` = allowed, 403 = denied).
pub async fn authorize(
    cookie: Option<&str>,
    action: &str,
    workspace: &str,
    user: Option<&str>,
) -> Result<(), crate::error::AppError> {
    let base = match platform_base() {
        Some(b) => b,
        None => return Ok(()), // platform not configured → don't enforce
    };
    let (cookie, user) = match (cookie, user) {
        (Some(c), Some(u)) => (c, u),
        _ => return Err(veda_types::VedaError::PermissionDenied.into()),
    };
    let url = format!("{base}/open/v1/auth/service/veda-reach/action/{action}");
    let allowed = PLATFORM_HTTP
        .get(&url)
        .query(&[("workspace", workspace), ("user", user)])
        .header("Cookie", cookie)
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    if allowed {
        Ok(())
    } else {
        Err(veda_types::VedaError::PermissionDenied.into())
    }
}

/// Resolve a platform workspace's display name by code (= `workspace_id`),
/// forwarding the request `cookie`. Returns the `name` field; `None` on any
/// failure or when the platform isn't configured (response then carries
/// `workspace_name: null`).
///
/// Verified shape (2026-06-17): `GET {base}/open/v1/workspace/{code}` → 200 bare
/// object `{ id, code, name, description, creator, creator_name, ... }`
/// (`dbpaas-test` → "DBPaaS 测试").
pub async fn resolve_workspace_name(cookie: Option<&str>, code: &str) -> Option<String> {
    let base = platform_base()?;
    let cookie = cookie?;
    let url = format!("{base}/open/v1/workspace/{code}");
    let resp = PLATFORM_HTTP
        .get(&url)
        .header("Cookie", cookie)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    v.get("name").and_then(|n| n.as_str()).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_name_and_display_name() {
        // The exact blob shape the gateway forwards (extra fields ignored).
        let json = r#"{"name":"zhangzhenzhen01","displayName":"张振振","empNo":"D0227705","mail":"x@100.me"}"#;
        let u: PlatformUser = serde_json::from_slice(json.as_bytes()).unwrap();
        assert_eq!(u.name, "zhangzhenzhen01");
        assert_eq!(u.display_name, "张振振");
    }

    #[test]
    fn display_name_defaults_empty_when_absent() {
        let u: PlatformUser = serde_json::from_slice(br#"{"name":"u1"}"#).unwrap();
        assert_eq!(u.name, "u1");
        assert_eq!(u.display_name, "");
    }

    #[test]
    fn roundtrip_through_base64() {
        let json = r#"{"name":"fukang","displayName":"付康"}"#;
        let b64 = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .unwrap();
        let u: PlatformUser = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(u.display_name, "付康");
    }
}
