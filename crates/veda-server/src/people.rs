//! HTTP person directory (company SSO/HR), the `PersonDirectory` impl
//! behind operator identity resolution (docs/plans/agent-memory-m3a.md §1.2).
//!
//! The company contract is SSO-gated and not final — `build_request` is the
//! single place that owns URL/params/headers, so wiring the real endpoint
//! touches one function. Current shape (placeholder until the SSO contract
//! lands): `GET {base_url}/person?source=<wecom|emp>&id=<external_id>` with
//! an optional Bearer token, answering
//! `{ "emp_no": "...", "name": "...", "dept_id": "...", "dept_name": "..." }`
//! (404 or an empty body = no such person).

use async_trait::async_trait;
use serde::Deserialize;
use veda_types::{PersonProfile, PrincipalSource, Result, VedaError};

use crate::config::PeopleConfig;

pub struct HttpPersonDirectory {
    http: reqwest::Client,
    base_url: String,
    token: Option<String>,
}

impl HttpPersonDirectory {
    pub fn new(cfg: &PeopleConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(cfg.timeout_secs))
            .build()
            .map_err(|e| VedaError::Internal(format!("people http client: {e}")))?;
        Ok(Self {
            http,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            token: cfg.token.clone(),
        })
    }

    fn build_request(&self, source: PrincipalSource, external_id: &str) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .get(format!("{}/person", self.base_url))
            .query(&[("source", source.as_str()), ("id", external_id)]);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        req
    }
}

#[derive(Deserialize)]
struct PersonResp {
    emp_no: Option<String>,
    name: Option<String>,
    dept_id: Option<String>,
    dept_name: Option<String>,
}

#[async_trait]
impl veda_core::store::PersonDirectory for HttpPersonDirectory {
    async fn lookup(
        &self,
        source: PrincipalSource,
        external_id: &str,
    ) -> Result<Option<PersonProfile>> {
        let resp = self
            .build_request(source, external_id)
            .send()
            .await
            .map_err(|e| VedaError::Internal(format!("person directory request: {e}")))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(VedaError::Internal(format!(
                "person directory returned {}",
                resp.status()
            )));
        }
        // A successful empty body (204 / "") is "no such person", not an
        // outage — an outage answer would wrongly push first-time
        // operators onto the degrade path.
        let raw = resp
            .text()
            .await
            .map_err(|e| VedaError::Internal(format!("person directory read: {e}")))?;
        if raw.trim().is_empty() {
            return Ok(None);
        }
        let body: PersonResp = serde_json::from_str(&raw)
            .map_err(|e| VedaError::Internal(format!("person directory decode: {e}")))?;
        let Some(emp_no) = body.emp_no.filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        Ok(Some(PersonProfile {
            emp_no,
            display_name: body.name.filter(|s| !s.is_empty()),
            dept_id: body.dept_id.filter(|s| !s.is_empty()),
            dept_name: body.dept_name.filter(|s| !s.is_empty()),
        }))
    }
}
