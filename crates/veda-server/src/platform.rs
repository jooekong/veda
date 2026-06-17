//! Platform gateway context.
//!
//! The OnePaaS gateway forwards caller identity in a base64-encoded `user`
//! header and routes per `ai_workspace`. On direct (non-gateway) access these
//! are absent, so every field is optional and handlers fall back to native
//! `wk_` / `vk_` auth. Used by the `/v1/apps/*` management surface to stamp
//! `creator` / `creator_name` and to drive the external authorization check.

use std::convert::Infallible;
use std::future::Future;

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
pub struct GatewayUser(pub Option<PlatformUser>);

impl GatewayUser {
    /// Domain account for `creator`, if present.
    pub fn creator(&self) -> Option<String> {
        self.0.as_ref().map(|u| u.name.clone())
    }

    /// Chinese display name for `creator_name`, if present and non-empty.
    pub fn creator_name(&self) -> Option<String> {
        self.0
            .as_ref()
            .filter(|u| !u.display_name.is_empty())
            .map(|u| u.display_name.clone())
    }
}

impl<S: Send + Sync> FromRequestParts<S> for GatewayUser {
    type Rejection = Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let user = decode_user_header(parts);
        async move { Ok(GatewayUser(user)) }
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

/// Resolve a platform workspace's display name by its code (= `workspace_id`).
///
/// Verified against the AI Workbench API (2026-06-17):
/// ```text
/// GET {base}/proxy/llm/open/v1/workspace/{code}
/// 200 (bare object, NOT the company envelope):
///   { "id", "code", "name", "description", "creator", "creator_name",
///     "created_at", "updated_at", "removed_at" }
/// ```
/// `workspace_name` is the `name` field (code `dbpaas-test` → "DBPaaS 测试").
/// base = paas-api-test.ddmc-inc.com (test) / paas-api.ddmc-inc.com (prod).
///
/// Returns `None` for now (stub): the call needs veda's OWN credential to the
/// platform — the working test auth is a user JWT cookie (`DDMC-INC`), so a
/// server-side service token is the real path. That's part of the frozen
/// external-authz work; wire the reqwest GET + `.name` extraction once it lands.
pub async fn resolve_workspace_name(_code: &str) -> Option<String> {
    None
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
