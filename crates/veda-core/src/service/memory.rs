//! Agent/team memory service (docs/plans/agent-memory-m1.md §2).
//!
//! Write path: embed → same-domain neighbors → topic default → MySQL insert
//! (idempotent on content hash) → Milvus upsert, outbox MemorySync on failure.
//! Read path: Milvus produces candidate ids only; MySQL recheck under the
//! same scope filter is the authority (deleted rows vanish, expired rows are
//! filtered SQL-side, cross-domain candidates are dropped even if the vector
//! index misbehaves). M1 ranking is pure similarity; last_used_at is only
//! recorded (multipliers come with M2, tuned on real data).

use std::sync::Arc;

use chrono::{DateTime, Utc};
use tracing::warn;
use veda_types::{
    Memory, MemoryInsert, MemoryKind, MemoryPatch, MemoryScope, MemoryScopeFilter,
    MemoryScopeType, MemoryWithEmbedding, NewMemory, PrincipalKind, PrincipalSource, Result,
    VedaError,
};

use crate::checksum::sha256_hex;
use crate::store::{
    EmbeddingService, MemoryListOrder, MemoryStore, MemoryVectorStore, PersonDirectory,
};

/// Memories are one-liners; anything longer belongs in a document.
const MAX_CONTENT_CHARS: usize = 4096;
const MAX_TOPIC_CHARS: usize = 128;
/// Neighbors returned by save (guides the agent toward update-vs-new).
const NEIGHBOR_LIMIT: usize = 3;
/// Candidate over-fetch: the MySQL recheck drops deleted/expired rows and
/// cross-domain hash duplicates (a fact can live in up to three domains),
/// so under-fetching would return fewer than `limit` live hits.
const OVERFETCH: usize = 3;
/// A topicless save inherits the top neighbor's topic only above this
/// cosine score — below it the memory is genuinely new, leave topic unset.
const TOPIC_INHERIT_MIN_SCORE: f32 = 0.75;
/// Evidence pointers are references, not payloads.
const MAX_SOURCE_REF_BYTES: usize = 4096;
/// Directory profile freshness window: within it a cached principal row is
/// authoritative and no directory call happens (调岗 lag upper bound).
const PROFILE_TTL_HOURS: i64 = 24;

/// Server-resolved identities for one call. `principal_id`/`dept_id` come
/// from the request identity (the wk_ key, or the operator the caller
/// asserted via X-Veda-Operator), never from scope-id client input — that
/// rule is what keeps domains server-resolved.
#[derive(Debug, Clone)]
pub struct MemoryActor {
    pub workspace_id: String,
    pub principal_id: String,
    /// The wk_ key's own principal when it differs from `principal_id`
    /// (operator present): `scope=self` targets this — agent state stays
    /// with the agent while `mine` follows the human. None = no separate
    /// agent identity (self ≡ mine, M1 semantics).
    pub self_principal_id: Option<String>,
    /// Operator's department (directory-resolved). None = no operator, no
    /// directory, or the directory reports no department.
    pub dept_id: Option<String>,
    /// Degraded operator (asserted but unresolvable — directory down on a
    /// never-seen identity): reads collapse to the team domain and
    /// personal/dept scopes reject (M3a §1.2). Never set on clean paths.
    pub team_only: bool,
}

impl MemoryActor {
    /// Plain actor: one principal, no operator baggage.
    pub fn new(workspace_id: String, principal_id: String) -> Self {
        Self {
            workspace_id,
            principal_id,
            self_principal_id: None,
            dept_id: None,
            team_only: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SaveMemoryInput {
    pub content: String,
    pub kind: MemoryKind,
    pub scope: MemoryScope,
    pub topic: Option<String>,
    /// None = default by kind (preference → portable, others → pinned to
    /// the current workspace); Some("") = force portable; Some(ws) = pin.
    /// Ignored for team scope.
    pub origin: Option<String>,
    pub source_ref: Option<serde_json::Value>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct MemoryHit {
    pub memory: Memory,
    pub score: f32,
}

#[derive(Debug, Clone)]
pub struct SaveMemoryOutcome {
    pub memory: Memory,
    /// True when an identical memory already existed in the target domain —
    /// the returned row is that one (save is idempotent, retries heal).
    pub duplicate: bool,
    pub neighbors: Vec<MemoryHit>,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateMemoryInput {
    pub content: Option<String>,
    pub topic: Option<String>,
    pub source_ref: Option<serde_json::Value>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Move to another domain (mine → team promotion etc.), resolved
    /// server-side against the actor like save's scope.
    pub scope: Option<MemoryScope>,
}

// Durability note: every store write (insert/update-with-content/delete)
// commits a MemorySync outbox task in the same transaction, so the worker
// replay — not the synchronous Milvus writes below — is what guarantees the
// vector index converges. The sync writes only buy immediate searchability;
// their failures are warnings, never data loss.
pub struct MemoryService {
    store: Arc<dyn MemoryStore>,
    vector: Arc<dyn MemoryVectorStore>,
    embedding: Arc<dyn EmbeddingService>,
    /// None = `[people]` not configured: operators resolve to per-entrance
    /// principals (emp_no NULL, no departments) — the pre-SSO mode.
    directory: Option<Arc<dyn PersonDirectory>>,
}

impl MemoryService {
    pub fn new(
        store: Arc<dyn MemoryStore>,
        vector: Arc<dyn MemoryVectorStore>,
        embedding: Arc<dyn EmbeddingService>,
        directory: Option<Arc<dyn PersonDirectory>>,
    ) -> Self {
        Self {
            store,
            vector,
            embedding,
            directory,
        }
    }

    /// Key-identity resolution (M1 semantics, still the no-operator path):
    /// the wk_ key is the person. First sighting lazily creates the
    /// principal. Keys never resolve through the directory and never carry
    /// a department.
    pub async fn resolve_key_actor(&self, workspace_id: &str, key_id: &str) -> Result<MemoryActor> {
        let principal = self
            .store
            .ensure_principal_for_identity(PrincipalSource::Key, key_id, PrincipalKind::Human, None)
            .await?;
        Ok(MemoryActor::new(workspace_id.to_string(), principal.id))
    }

    /// Operator resolution (M3a §1.2/§1.3). Returns:
    /// - Ok(Some) — resolved actor (with dept when the directory knows one);
    /// - Ok(None) — degrade to no-operator: brand-new identity while the
    ///   directory is unreachable. Callers fall back to key semantics for
    ///   reads and reject mine/dept writes;
    /// - Err — the caller's assertion is bad (directory answered "no such
    ///   person" for a never-seen identity).
    pub async fn resolve_operator_actor(
        &self,
        workspace_id: &str,
        source: PrincipalSource,
        external_id: &str,
    ) -> Result<Option<MemoryActor>> {
        let cached = self.store.get_principal_by_identity(source, external_id).await?;
        let fresh = |p: &veda_types::Principal| {
            p.profile_synced_at
                .is_some_and(|t| Utc::now() - t < chrono::Duration::hours(PROFILE_TTL_HOURS))
        };
        let principal = match (&self.directory, cached) {
            // Directory not configured: identity-only mode, per-entrance
            // principals, no profile ever.
            (None, _) => {
                self.store
                    .ensure_principal_for_identity(source, external_id, PrincipalKind::Human, None)
                    .await?
            }
            (Some(_), Some(p)) if fresh(&p) => p,
            (Some(dir), cached) => match dir.lookup(source, external_id).await {
                Ok(Some(profile)) => {
                    self.store
                        .ensure_principal_for_identity(
                            source,
                            external_id,
                            PrincipalKind::Human,
                            Some(&profile),
                        )
                        .await?
                }
                // Directory answered and knows no such person: a stale
                // known identity keeps its personal notes working, but the
                // department authorization must not outlive the documented
                // TTL bound — strip it for this request (Codex M3a-impl R2;
                // the row stays stale, so every later call re-asks the
                // directory). A never-seen identity is a bad assertion.
                Ok(None) => match cached {
                    Some(p) => {
                        warn!(
                            principal = %p.id,
                            "directory no longer knows this identity; dept dropped"
                        );
                        veda_types::Principal {
                            dept_id: None,
                            dept_name: None,
                            ..p
                        }
                    }
                    None => {
                        return Err(VedaError::InvalidInput(format!(
                            "operator {}:{external_id} not found in person directory",
                            source.as_str()
                        )))
                    }
                },
                // Directory unreachable: cached profile continues (read
                // fail-open); a brand-new identity degrades to no-operator
                // rather than minting a half-identity (R4).
                Err(e) => match cached {
                    Some(p) => {
                        warn!(err = %e, "person directory unavailable; using cached profile");
                        p
                    }
                    None => {
                        warn!(err = %e, "person directory unavailable; unknown operator degrades");
                        return Ok(None);
                    }
                },
            },
        };
        Ok(Some(MemoryActor {
            workspace_id: workspace_id.to_string(),
            principal_id: principal.id,
            self_principal_id: None,
            dept_id: principal.dept_id,
            team_only: false,
        }))
    }

    pub async fn save(
        &self,
        actor: &MemoryActor,
        input: SaveMemoryInput,
    ) -> Result<SaveMemoryOutcome> {
        validate_source_ref(input.source_ref.as_ref())?;
        let content = input.content.trim();
        if content.is_empty() {
            return Err(VedaError::InvalidInput("content is empty".into()));
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            return Err(VedaError::InvalidInput(format!(
                "content exceeds {MAX_CONTENT_CHARS} chars — a memory is one fact, put long text in a document"
            )));
        }
        if let Some(t) = &input.topic {
            if t.chars().count() > MAX_TOPIC_CHARS {
                return Err(VedaError::InvalidInput(format!(
                    "topic exceeds {MAX_TOPIC_CHARS} chars"
                )));
            }
        }

        let (scope_type, scope_id) = resolve_scope(actor, input.scope)?;
        let origin = resolve_origin(actor, input.scope, input.kind, input.origin.as_deref());

        let vector = self.embed_one(content).await?;
        let domain = MemoryScopeFilter::Scope {
            scope_type,
            scope_id: scope_id.clone(),
        };
        let neighbors = self
            .lookup_candidates(content, &vector, &domain, NEIGHBOR_LIMIT)
            .await?;

        // Topicless writes join the nearest existing topic when the
        // neighborhood is close enough; otherwise the topic stays unset
        // rather than inventing a cluster (design §7: assignment happens at
        // write time, where the context is).
        let topic = input.topic.clone().or_else(|| {
            neighbors
                .first()
                .filter(|n| n.score >= TOPIC_INHERIT_MIN_SCORE)
                .and_then(|n| n.memory.topic.clone())
        });

        let inserted = self
            .store
            .insert_memory(&NewMemory {
                scope_type,
                scope_id: scope_id.clone(),
                origin_workspace_id: origin,
                topic,
                kind: input.kind,
                content: content.to_string(),
                content_hash: sha256_hex(content.as_bytes()),
                source_ref: input.source_ref,
                expires_at: input.expires_at,
                created_by: actor.principal_id.clone(),
            })
            .await?;
        let (memory, duplicate) = match inserted {
            MemoryInsert::Inserted(m) => (m, false),
            // Retried saves and racing writers land here; upserting the
            // vector again below heals a previously failed index write.
            MemoryInsert::Duplicate(m) => (m, true),
        };

        // Latency optimization only — the transactional MemorySync task
        // the store just committed makes the index converge regardless.
        if let Err(e) = self
            .vector
            .upsert_memory_vectors(&[embedding_row(&memory, vector)])
            .await
        {
            warn!(memory_id = memory.id, err = %e, "sync memory vector upsert failed; outbox heal will cover");
        }

        Ok(SaveMemoryOutcome {
            memory,
            duplicate,
            neighbors,
        })
    }

    /// Search with an explicit scope: None = the full context union
    /// (team + operator's personal, origin-filtered), Team / Mine / Self
    /// narrow to one domain.
    pub async fn search(
        &self,
        actor: &MemoryActor,
        query: &str,
        scope: Option<MemoryScope>,
        limit: usize,
    ) -> Result<Vec<MemoryHit>> {
        let filter = match scope {
            None => context_filter(actor),
            Some(s) => {
                let (scope_type, scope_id) = resolve_scope(actor, s)?;
                MemoryScopeFilter::Scope {
                    scope_type,
                    scope_id,
                }
            }
        };
        self.retrieve(query, &filter, limit).await
    }

    /// The session-start call (design §8): one query over team + personal.
    pub async fn context(
        &self,
        actor: &MemoryActor,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryHit>> {
        self.retrieve(query, &context_filter(actor), limit).await
    }

    /// Team-domain retrieval without an operator identity — the answer
    /// path's injection source (M2a). Personal domains need an operator and
    /// stay out until identity passthrough lands (M3).
    pub async fn team_memories(
        &self,
        workspace_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryHit>> {
        let filter = MemoryScopeFilter::Scope {
            scope_type: MemoryScopeType::Workspace,
            scope_id: workspace_id.to_string(),
        };
        self.retrieve(query, &filter, limit).await
    }

    pub async fn update(
        &self,
        actor: &MemoryActor,
        id: i64,
        input: UpdateMemoryInput,
    ) -> Result<Memory> {
        if input.content.is_none()
            && input.topic.is_none()
            && input.source_ref.is_none()
            && input.expires_at.is_none()
            && input.scope.is_none()
        {
            return Err(VedaError::InvalidInput("nothing to update".into()));
        }
        validate_source_ref(input.source_ref.as_ref())?;
        let content = match &input.content {
            Some(c) => {
                let c = c.trim();
                if c.is_empty() {
                    return Err(VedaError::InvalidInput("content is empty".into()));
                }
                if c.chars().count() > MAX_CONTENT_CHARS {
                    return Err(VedaError::InvalidInput(format!(
                        "content exceeds {MAX_CONTENT_CHARS} chars"
                    )));
                }
                Some(c.to_string())
            }
            None => None,
        };
        if let Some(t) = &input.topic {
            if t.chars().count() > MAX_TOPIC_CHARS {
                return Err(VedaError::InvalidInput(format!(
                    "topic exceeds {MAX_TOPIC_CHARS} chars"
                )));
            }
        }
        let patch = MemoryPatch {
            content_hash: content.as_deref().map(|c| sha256_hex(c.as_bytes())),
            content,
            topic: input.topic,
            source_ref: input.source_ref,
            expires_at: input.expires_at,
            scope: input.scope.map(|s| resolve_scope(actor, s)).transpose()?,
        };
        let memory = self
            .store
            .update_memory(id, &allowed_scopes(actor), &patch, &actor.principal_id)
            .await?;

        if patch.content.is_some() || patch.scope.is_some() {
            // Sync re-embed for immediate freshness — a scope move must
            // refresh the Milvus scalars even with unchanged content, or
            // the target domain misses the row until the outbox heals. The
            // store committed a MemorySync task with the row change, so
            // failures here cost latency, not convergence.
            match self.embed_one(&memory.content).await {
                Ok(v) => {
                    if let Err(e) = self
                        .vector
                        .upsert_memory_vectors(&[embedding_row(&memory, v)])
                        .await
                    {
                        warn!(memory_id = memory.id, err = %e, "sync re-embed upsert failed; outbox heal will cover");
                    }
                }
                Err(e) => {
                    warn!(memory_id = memory.id, err = %e, "sync re-embed failed; outbox heal will cover");
                }
            }
        }
        Ok(memory)
    }

    pub async fn delete(&self, actor: &MemoryActor, id: i64) -> Result<()> {
        self.delete_in(id, &allowed_scopes(actor)).await
    }

    async fn delete_in(&self, id: i64, allowed: &[(MemoryScopeType, String)]) -> Result<()> {
        let deleted = self.store.delete_memory(id, allowed).await?;
        if !deleted {
            return Err(VedaError::NotFound(format!("memory {id}")));
        }
        // A leftover vector cannot resurface the memory (recheck finds no
        // row); the store committed a delete task alongside the row delete,
        // which also supersedes any in-flight upsert heal. This sync delete
        // is immediacy only.
        if let Err(e) = self.vector.delete_memory_vectors(&[id]).await {
            warn!(memory_id = id, err = %e, "sync memory vector delete failed; outbox task will cover");
        }
        Ok(())
    }

    /// Browse-page enumeration (M4a): one domain per tab — a governance
    /// view wants crisp boundaries, so no context union here. `self` is
    /// deliberately not a tab.
    pub async fn list(
        &self,
        actor: &MemoryActor,
        tab: MemoryScope,
        topic: Option<&str>,
        kind: Option<MemoryKind>,
        page: u32,
        size: u32,
    ) -> Result<(Vec<Memory>, i64)> {
        self.store
            .list_memories(
                &browse_filter(actor, tab)?,
                topic,
                kind,
                MemoryListOrder::UpdatedAt,
                page,
                size.clamp(1, 100),
            )
            .await
    }

    /// Topic directory for one browse tab: (topic, live count).
    pub async fn topics(
        &self,
        actor: &MemoryActor,
        tab: MemoryScope,
    ) -> Result<Vec<(Option<String>, i64)>> {
        self.store.topic_counts(&browse_filter(actor, tab)?).await
    }

    /// Admin cleanup surface (M4a): TEAM domain only, with the workspace as
    /// an explicit parameter instead of a key/operator identity. Personal
    /// domains stay out of the admin view (design: owner-only visibility).
    /// Shares the scoped store primitives — this is a parameterized domain,
    /// not a bypass query.
    pub async fn admin_list_team(
        &self,
        workspace_id: &str,
        kind: Option<MemoryKind>,
        order: MemoryListOrder,
        page: u32,
        size: u32,
    ) -> Result<(Vec<Memory>, i64)> {
        let filter = MemoryScopeFilter::Scope {
            scope_type: MemoryScopeType::Workspace,
            scope_id: workspace_id.to_string(),
        };
        self.store
            .list_memories(&filter, None, kind, order, page, size.clamp(1, 100))
            .await
    }

    pub async fn admin_delete_team(&self, workspace_id: &str, id: i64) -> Result<()> {
        self.delete_in(id, &[(MemoryScopeType::Workspace, workspace_id.to_string())])
            .await
    }

    async fn retrieve(
        &self,
        query: &str,
        filter: &MemoryScopeFilter,
        limit: usize,
    ) -> Result<Vec<MemoryHit>> {
        let query = query.trim();
        if query.is_empty() {
            return Err(VedaError::InvalidInput("query is empty".into()));
        }
        let limit = limit.clamp(1, 50);
        let vector = self.embed_one(query).await?;
        let hits = self
            .lookup_candidates(query, &vector, filter, limit)
            .await?;
        let ids: Vec<i64> = hits.iter().map(|h| h.memory.id).collect();
        if !ids.is_empty() {
            if let Err(e) = self.store.touch_memories(&ids).await {
                warn!(err = %e, "touch_memories failed");
            }
        }
        Ok(hits)
    }

    /// Candidates from Milvus (hybrid, over-fetched), rechecked against
    /// MySQL under the same filter, returned in candidate (fused-score)
    /// order. Cross-domain exact duplicates (same content_hash living in
    /// several domains, e.g. a personal note later shared to team) collapse
    /// to the widest domain: team > dept > mine.
    async fn lookup_candidates(
        &self,
        query_text: &str,
        vector: &[f32],
        filter: &MemoryScopeFilter,
        limit: usize,
    ) -> Result<Vec<MemoryHit>> {
        let candidates = self
            .vector
            .search_memory_candidates(query_text, vector, filter, limit * OVERFETCH)
            .await?;
        if candidates.is_empty() {
            return Ok(vec![]);
        }
        let ids: Vec<i64> = candidates.iter().map(|c| c.id).collect();
        let rows = self.store.get_memories_by_ids(&ids, filter).await?;
        let by_id: std::collections::HashMap<i64, Memory> =
            rows.into_iter().map(|m| (m.id, m)).collect();
        // Walk every candidate (bounded by limit×OVERFETCH) so a wider-domain
        // duplicate ranked lower can still displace a held narrow one, then
        // truncate. Positions keep the fused-score order of first appearance.
        let mut out: Vec<MemoryHit> = Vec::with_capacity(limit);
        for c in &candidates {
            let Some(m) = by_id.get(&c.id) else { continue };
            match out
                .iter_mut()
                .find(|h| h.memory.content_hash == m.content_hash)
            {
                Some(held) => {
                    if scope_width(m.scope_type) > scope_width(held.memory.scope_type) {
                        held.memory = m.clone();
                    }
                }
                None => out.push(MemoryHit {
                    memory: m.clone(),
                    score: c.score,
                }),
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut vs = self.embedding.embed(&[text.to_string()]).await?;
        vs.pop()
            .ok_or_else(|| VedaError::EmbeddingFailed("empty embedding batch".into()))
    }
}

fn validate_source_ref(source_ref: Option<&serde_json::Value>) -> Result<()> {
    if let Some(v) = source_ref {
        if !v.is_object() {
            return Err(VedaError::InvalidInput(
                "source_ref must be a JSON object like {\"files\": [...]}".into(),
            ));
        }
        let len = serde_json::to_string(v).map(|s| s.len()).unwrap_or(0);
        if len > MAX_SOURCE_REF_BYTES {
            return Err(VedaError::InvalidInput(format!(
                "source_ref exceeds {MAX_SOURCE_REF_BYTES} bytes — store pointers, not payloads"
            )));
        }
    }
    Ok(())
}

fn resolve_scope(actor: &MemoryActor, scope: MemoryScope) -> Result<(MemoryScopeType, String)> {
    if actor.team_only && !matches!(scope, MemoryScope::Team) {
        return Err(VedaError::InvalidInput(
            "person directory unavailable; only team-scope memory operations are available"
                .into(),
        ));
    }
    Ok(match scope {
        MemoryScope::Team => (MemoryScopeType::Workspace, actor.workspace_id.clone()),
        MemoryScope::Dept => match &actor.dept_id {
            Some(d) => (MemoryScopeType::Dept, d.clone()),
            None => {
                return Err(VedaError::InvalidInput(
                    "dept scope needs an operator with a directory-resolved department".into(),
                ))
            }
        },
        MemoryScope::Mine => (MemoryScopeType::Principal, actor.principal_id.clone()),
        // Self = the agent's own domain. With an operator it targets the
        // key principal (agent state stays with the agent); without one the
        // key IS the principal, so Mine and Self coincide (M1 semantics).
        MemoryScope::SelfScope => (
            MemoryScopeType::Principal,
            actor
                .self_principal_id
                .clone()
                .unwrap_or_else(|| actor.principal_id.clone()),
        ),
    })
}

/// Origin defaulting (design §4.2): never validated, only defaulted.
/// Team and dept memories carry no origin at all.
fn resolve_origin(
    actor: &MemoryActor,
    scope: MemoryScope,
    kind: MemoryKind,
    origin: Option<&str>,
) -> Option<String> {
    if matches!(scope, MemoryScope::Team | MemoryScope::Dept) {
        return None;
    }
    match origin {
        Some("") => None,
        Some(ws) => Some(ws.to_string()),
        None => match kind {
            MemoryKind::Preference => None,
            _ => Some(actor.workspace_id.clone()),
        },
    }
}

fn context_filter(actor: &MemoryActor) -> MemoryScopeFilter {
    if actor.team_only {
        return MemoryScopeFilter::Scope {
            scope_type: MemoryScopeType::Workspace,
            scope_id: actor.workspace_id.clone(),
        };
    }
    MemoryScopeFilter::Context {
        workspace_id: actor.workspace_id.clone(),
        principal_id: actor.principal_id.clone(),
        dept_id: actor.dept_id.clone(),
    }
}

/// Cross-domain dedup priority (§1.6): the widest audience wins — the
/// shared copy is the authoritative citation, private drafts yield.
fn scope_width(t: MemoryScopeType) -> u8 {
    match t {
        MemoryScopeType::Workspace => 3,
        MemoryScopeType::Dept => 2,
        MemoryScopeType::Principal => 1,
    }
}

/// The caller's writable domains: the current workspace's team domain,
/// their own personal domain, and their department when known.
/// Update/delete WHERE clauses are built from this — sharing the discipline
/// that no memory write escapes its scopes.
fn allowed_scopes(actor: &MemoryActor) -> Vec<(MemoryScopeType, String)> {
    if actor.team_only {
        return vec![(MemoryScopeType::Workspace, actor.workspace_id.clone())];
    }
    let mut v = vec![
        (MemoryScopeType::Workspace, actor.workspace_id.clone()),
        (MemoryScopeType::Principal, actor.principal_id.clone()),
    ];
    if let Some(sp) = &actor.self_principal_id {
        if sp != &actor.principal_id {
            v.push((MemoryScopeType::Principal, sp.clone()));
        }
    }
    if let Some(d) = &actor.dept_id {
        v.push((MemoryScopeType::Dept, d.clone()));
    }
    v
}

/// Tab → single-domain filter. Mirrors resolve_scope's rules (team_only
/// degradation, dept needs a department), except the mine tab uses the
/// origin-restricted Personal filter: the browse page shows what context
/// retrieval can surface in THIS workspace (portable + pinned-here), not
/// the full cross-project personal domain.
fn browse_filter(actor: &MemoryActor, tab: MemoryScope) -> Result<MemoryScopeFilter> {
    if matches!(tab, MemoryScope::SelfScope) {
        return Err(VedaError::InvalidInput(
            "self is not a browse tab — use team, dept, or mine".into(),
        ));
    }
    if matches!(tab, MemoryScope::Mine) && !actor.team_only {
        return Ok(MemoryScopeFilter::Personal {
            principal_id: actor.principal_id.clone(),
            workspace_id: actor.workspace_id.clone(),
        });
    }
    let (scope_type, scope_id) = resolve_scope(actor, tab)?;
    Ok(MemoryScopeFilter::Scope {
        scope_type,
        scope_id,
    })
}

fn embedding_row(memory: &Memory, vector: Vec<f32>) -> MemoryWithEmbedding {
    MemoryWithEmbedding {
        id: memory.id,
        scope_type: memory.scope_type,
        scope_id: memory.scope_id.clone(),
        origin_workspace_id: memory.origin_workspace_id.clone().unwrap_or_default(),
        content: memory.content.clone(),
        vector,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use veda_types::{MemoryCandidate, Principal, PrincipalKind, PrincipalSource};

    fn mem(id: i64, scope_type: MemoryScopeType, scope_id: &str, topic: Option<&str>) -> Memory {
        Memory {
            id,
            scope_type,
            scope_id: scope_id.into(),
            origin_workspace_id: None,
            topic: topic.map(Into::into),
            kind: MemoryKind::Fact,
            content: format!("memory {id}"),
            content_hash: format!("{id:064}"),
            source_ref: None,
            expires_at: None,
            last_used_at: None,
            created_by: "p1".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            updated_by: "p1".into(),
        }
    }

    #[derive(Default)]
    struct MockState {
        rows: Vec<Memory>,
        inserted: Vec<NewMemory>,
        touched: Vec<i64>,
        vectors_upserted: Vec<MemoryWithEmbedding>,
        vectors_deleted: Vec<i64>,
        candidates: Vec<MemoryCandidate>,
        /// Filters passed into list_memories/topic_counts (browse assertions).
        list_filters: Vec<MemoryScopeFilter>,
        /// Allowed-domain sets passed into delete_memory.
        deleted_allowed: Vec<Vec<(MemoryScopeType, String)>>,
        fail_vector_upsert: bool,
        duplicate_of: Option<i64>,
        /// Principal returned by get_principal_by_identity (identity cache).
        cached_principal: Option<Principal>,
        /// Profiles passed into ensure_principal_for_identity.
        ensured_profiles: Vec<Option<veda_types::PersonProfile>>,
    }

    #[derive(Default)]
    struct Mock(Mutex<MockState>);

    #[async_trait]
    impl MemoryStore for Mock {
        async fn insert_memory(&self, m: &NewMemory) -> Result<MemoryInsert> {
            let mut s = self.0.lock().unwrap();
            s.inserted.push(m.clone());
            if let Some(id) = s.duplicate_of {
                let row = s.rows.iter().find(|r| r.id == id).unwrap().clone();
                return Ok(MemoryInsert::Duplicate(row));
            }
            let row = Memory {
                id: 100 + s.inserted.len() as i64,
                scope_type: m.scope_type,
                scope_id: m.scope_id.clone(),
                origin_workspace_id: m.origin_workspace_id.clone(),
                topic: m.topic.clone(),
                kind: m.kind,
                content: m.content.clone(),
                content_hash: m.content_hash.clone(),
                source_ref: m.source_ref.clone(),
                expires_at: m.expires_at,
                last_used_at: None,
                created_by: m.created_by.clone(),
                created_at: Utc::now(),
                updated_by: m.created_by.clone(),
                updated_at: Utc::now(),
            };
            s.rows.push(row.clone());
            Ok(MemoryInsert::Inserted(row))
        }

        async fn update_memory(
            &self,
            _id: i64,
            _allowed: &[(MemoryScopeType, String)],
            _patch: &MemoryPatch,
            _updated_by: &str,
        ) -> Result<Memory> {
            unimplemented!()
        }

        async fn delete_memory(
            &self,
            _id: i64,
            allowed: &[(MemoryScopeType, String)],
        ) -> Result<bool> {
            self.0.lock().unwrap().deleted_allowed.push(allowed.to_vec());
            Ok(true)
        }

        async fn get_memories_by_ids(
            &self,
            ids: &[i64],
            _filter: &MemoryScopeFilter,
        ) -> Result<Vec<Memory>> {
            let s = self.0.lock().unwrap();
            Ok(s.rows
                .iter()
                .filter(|r| ids.contains(&r.id))
                .cloned()
                .collect())
        }

        async fn touch_memories(&self, ids: &[i64]) -> Result<()> {
            self.0.lock().unwrap().touched.extend_from_slice(ids);
            Ok(())
        }

        async fn list_memories(
            &self,
            filter: &MemoryScopeFilter,
            _topic: Option<&str>,
            _kind: Option<MemoryKind>,
            _order: MemoryListOrder,
            _page: u32,
            _size: u32,
        ) -> Result<(Vec<Memory>, i64)> {
            let mut s = self.0.lock().unwrap();
            s.list_filters.push(filter.clone());
            let n = s.rows.len() as i64;
            Ok((s.rows.clone(), n))
        }

        async fn topic_counts(
            &self,
            filter: &MemoryScopeFilter,
        ) -> Result<Vec<(Option<String>, i64)>> {
            self.0.lock().unwrap().list_filters.push(filter.clone());
            Ok(vec![])
        }

        async fn get_principal_by_identity(
            &self,
            _source: PrincipalSource,
            _external_id: &str,
        ) -> Result<Option<Principal>> {
            Ok(self.0.lock().unwrap().cached_principal.clone())
        }

        async fn ensure_principal_for_identity(
            &self,
            _source: PrincipalSource,
            external_id: &str,
            kind: PrincipalKind,
            profile: Option<&veda_types::PersonProfile>,
        ) -> Result<Principal> {
            let mut s = self.0.lock().unwrap();
            s.ensured_profiles.push(profile.cloned());
            Ok(Principal {
                id: format!("P-{external_id}"),
                kind,
                emp_no: profile.map(|p| p.emp_no.clone()),
                display_name: profile.and_then(|p| p.display_name.clone()),
                dept_id: profile.and_then(|p| p.dept_id.clone()),
                dept_name: profile.and_then(|p| p.dept_name.clone()),
                profile_synced_at: profile.map(|_| Utc::now()),
                created_at: Utc::now(),
            })
        }
    }

    #[async_trait]
    impl MemoryVectorStore for Mock {
        async fn init_memory_collection(&self, _dim: u32) -> Result<()> {
            Ok(())
        }
        async fn upsert_memory_vectors(&self, items: &[MemoryWithEmbedding]) -> Result<()> {
            let mut s = self.0.lock().unwrap();
            if s.fail_vector_upsert {
                return Err(VedaError::Storage("milvus down".into()));
            }
            s.vectors_upserted.extend(items.iter().cloned());
            Ok(())
        }
        async fn delete_memory_vectors(&self, ids: &[i64]) -> Result<()> {
            self.0.lock().unwrap().vectors_deleted.extend_from_slice(ids);
            Ok(())
        }
        async fn search_memory_candidates(
            &self,
            _query_text: &str,
            _vector: &[f32],
            _filter: &MemoryScopeFilter,
            _limit: usize,
        ) -> Result<Vec<MemoryCandidate>> {
            Ok(self.0.lock().unwrap().candidates.clone())
        }
    }

    struct MockEmbed;
    #[async_trait]
    impl EmbeddingService for MockEmbed {
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![0.5, 0.5]).collect())
        }
        fn dimension(&self) -> usize {
            2
        }
    }

    fn service(mock: Arc<Mock>) -> MemoryService {
        MemoryService::new(mock.clone(), mock.clone(), Arc::new(MockEmbed), None)
    }

    fn actor() -> MemoryActor {
        MemoryActor::new("W1".into(), "P1".into())
    }

    fn save_input(scope: MemoryScope, kind: MemoryKind) -> SaveMemoryInput {
        SaveMemoryInput {
            content: "tests need --test-threads=1".into(),
            kind,
            scope,
            topic: None,
            origin: None,
            source_ref: None,
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn save_resolves_scope_and_origin_defaults() {
        let mock = Arc::new(Mock::default());
        let svc = service(mock.clone());

        // team: workspace domain, no origin ever
        svc.save(&actor(), save_input(MemoryScope::Team, MemoryKind::Fact))
            .await
            .unwrap();
        // mine + fact: pinned to current workspace by default
        let mut inp = save_input(MemoryScope::Mine, MemoryKind::Fact);
        inp.content = "fact two".into();
        svc.save(&actor(), inp).await.unwrap();
        // mine + preference: portable by default
        let mut inp = save_input(MemoryScope::Mine, MemoryKind::Preference);
        inp.content = "pref".into();
        svc.save(&actor(), inp).await.unwrap();
        // explicit "" forces portable even for a fact
        let mut inp = save_input(MemoryScope::Mine, MemoryKind::Fact);
        inp.content = "portable fact".into();
        inp.origin = Some("".into());
        svc.save(&actor(), inp).await.unwrap();

        let s = mock.0.lock().unwrap();
        let i = &s.inserted;
        assert_eq!(
            (i[0].scope_type, i[0].scope_id.as_str(), i[0].origin_workspace_id.as_deref()),
            (MemoryScopeType::Workspace, "W1", None)
        );
        assert_eq!(
            (i[1].scope_type, i[1].scope_id.as_str(), i[1].origin_workspace_id.as_deref()),
            (MemoryScopeType::Principal, "P1", Some("W1"))
        );
        assert_eq!(i[2].origin_workspace_id, None, "preference defaults portable");
        assert_eq!(i[3].origin_workspace_id, None, "explicit empty forces portable");
        assert_eq!(s.vectors_upserted.len(), 4, "every save writes the index");
        assert_eq!(
            s.vectors_upserted[1].origin_workspace_id, "W1",
            "index row carries origin as string"
        );
    }

    #[tokio::test]
    async fn save_inherits_topic_only_above_threshold() {
        let mock = Arc::new(Mock::default());
        {
            let mut s = mock.0.lock().unwrap();
            s.rows.push(mem(7, MemoryScopeType::Workspace, "W1", Some("testing")));
            s.candidates = vec![MemoryCandidate { id: 7, score: 0.9 }];
        }
        let svc = service(mock.clone());
        let out = svc
            .save(&actor(), save_input(MemoryScope::Team, MemoryKind::Fact))
            .await
            .unwrap();
        assert_eq!(out.memory.topic.as_deref(), Some("testing"));
        assert_eq!(out.neighbors.len(), 1);

        // Below threshold: no inheritance.
        {
            let mut s = mock.0.lock().unwrap();
            s.candidates = vec![MemoryCandidate { id: 7, score: 0.3 }];
        }
        let mut inp = save_input(MemoryScope::Team, MemoryKind::Fact);
        inp.content = "unrelated fact".into();
        let out = svc.save(&actor(), inp).await.unwrap();
        assert_eq!(out.memory.topic, None);
    }

    #[tokio::test]
    async fn save_survives_sync_vector_write_failure() {
        // Durability comes from the store-transactional MemorySync task;
        // the service's synchronous Milvus write is latency-only, so its
        // failure must not fail the save.
        let mock = Arc::new(Mock::default());
        mock.0.lock().unwrap().fail_vector_upsert = true;
        let svc = service(mock.clone());
        let out = svc
            .save(&actor(), save_input(MemoryScope::Team, MemoryKind::Fact))
            .await
            .expect("save must succeed despite sync vector failure");
        assert!(!out.duplicate);
        assert!(mock.0.lock().unwrap().vectors_upserted.is_empty());
    }

    #[tokio::test]
    async fn duplicate_save_heals_vector_and_flags() {
        let mock = Arc::new(Mock::default());
        {
            let mut s = mock.0.lock().unwrap();
            s.rows.push(mem(42, MemoryScopeType::Workspace, "W1", None));
            s.duplicate_of = Some(42);
        }
        let svc = service(mock.clone());
        let out = svc
            .save(&actor(), save_input(MemoryScope::Team, MemoryKind::Fact))
            .await
            .unwrap();
        assert!(out.duplicate);
        assert_eq!(out.memory.id, 42);
        let s = mock.0.lock().unwrap();
        assert_eq!(
            s.vectors_upserted.iter().filter(|v| v.id == 42).count(),
            1,
            "duplicate save still upserts the vector (heal)"
        );
    }

    #[tokio::test]
    async fn retrieve_rechecks_orders_and_touches() {
        let mock = Arc::new(Mock::default());
        {
            let mut s = mock.0.lock().unwrap();
            s.rows.push(mem(1, MemoryScopeType::Workspace, "W1", None));
            s.rows.push(mem(2, MemoryScopeType::Workspace, "W1", None));
            // id 3 is a stale candidate: in Milvus, already gone from MySQL.
            s.candidates = vec![
                MemoryCandidate { id: 3, score: 0.99 },
                MemoryCandidate { id: 2, score: 0.8 },
                MemoryCandidate { id: 1, score: 0.7 },
            ];
        }
        let svc = service(mock.clone());
        let hits = svc.context(&actor(), "threads", 10).await.unwrap();
        let ids: Vec<i64> = hits.iter().map(|h| h.memory.id).collect();
        assert_eq!(ids, vec![2, 1], "stale candidate dropped, similarity order kept");
        let s = mock.0.lock().unwrap();
        assert_eq!(s.touched, vec![2, 1], "hits are touched");
    }

    #[tokio::test]
    async fn delete_removes_vector() {
        let mock = Arc::new(Mock::default());
        let svc = service(mock.clone());
        svc.delete(&actor(), 9).await.unwrap();
        assert_eq!(mock.0.lock().unwrap().vectors_deleted, vec![9]);
    }

    fn person(emp: &str, dept: Option<&str>) -> veda_types::PersonProfile {
        veda_types::PersonProfile {
            emp_no: emp.into(),
            display_name: Some("张三".into()),
            dept_id: dept.map(Into::into),
            dept_name: dept.map(|_| "基础架构".into()),
        }
    }

    struct MockDirectory(std::result::Result<Option<veda_types::PersonProfile>, String>);
    #[async_trait]
    impl crate::store::PersonDirectory for MockDirectory {
        async fn lookup(
            &self,
            _source: PrincipalSource,
            _external_id: &str,
        ) -> Result<Option<veda_types::PersonProfile>> {
            match &self.0 {
                Ok(p) => Ok(p.clone()),
                Err(e) => Err(VedaError::Internal(e.clone())),
            }
        }
    }

    fn service_with_dir(
        mock: Arc<Mock>,
        dir: MockDirectory,
    ) -> MemoryService {
        MemoryService::new(mock.clone(), mock.clone(), Arc::new(MockEmbed), Some(Arc::new(dir)))
    }

    #[tokio::test]
    async fn dept_save_requires_department() {
        let mock = Arc::new(Mock::default());
        let svc = service(mock.clone());
        // Actor without a dept: dept-scope save is a client error.
        let err = svc
            .save(&actor(), save_input(MemoryScope::Dept, MemoryKind::Fact))
            .await
            .unwrap_err();
        assert!(matches!(err, VedaError::InvalidInput(_)), "{err:?}");

        // With a dept: lands in the dept domain, no origin.
        let with_dept = MemoryActor {
            dept_id: Some("D9".into()),
            ..actor()
        };
        svc.save(&with_dept, save_input(MemoryScope::Dept, MemoryKind::Fact))
            .await
            .unwrap();
        let s = mock.0.lock().unwrap();
        let last = s.inserted.last().unwrap();
        assert_eq!(
            (last.scope_type, last.scope_id.as_str(), last.origin_workspace_id.as_deref()),
            (MemoryScopeType::Dept, "D9", None)
        );
    }

    #[tokio::test]
    async fn retrieve_dedups_cross_domain_widest_wins() {
        let mock = Arc::new(Mock::default());
        {
            let mut s = mock.0.lock().unwrap();
            // Same fact living in personal AND team domains (same hash).
            let mut personal = mem(1, MemoryScopeType::Principal, "P1", None);
            personal.content_hash = "h".repeat(64);
            let mut team = mem(2, MemoryScopeType::Workspace, "W1", None);
            team.content_hash = "h".repeat(64);
            let other = mem(3, MemoryScopeType::Workspace, "W1", None);
            s.rows.extend([personal, team, other]);
            // Personal ranks higher, team lower — widest must still win.
            s.candidates = vec![
                MemoryCandidate { id: 1, score: 0.9 },
                MemoryCandidate { id: 2, score: 0.8 },
                MemoryCandidate { id: 3, score: 0.7 },
            ];
        }
        let svc = service(mock.clone());
        let hits = svc.context(&actor(), "q", 10).await.unwrap();
        let got: Vec<(i64, MemoryScopeType)> =
            hits.iter().map(|h| (h.memory.id, h.memory.scope_type)).collect();
        assert_eq!(
            got,
            vec![(2, MemoryScopeType::Workspace), (3, MemoryScopeType::Workspace)],
            "duplicate collapsed to the team copy, position kept"
        );
    }

    #[tokio::test]
    async fn operator_new_identity_with_directory_down_degrades() {
        let mock = Arc::new(Mock::default());
        let svc = service_with_dir(mock.clone(), MockDirectory(Err("boom".into())));
        let got = svc
            .resolve_operator_actor("W1", PrincipalSource::Wecom, "u-new")
            .await
            .unwrap();
        assert!(got.is_none(), "unknown identity + directory down must degrade");
        assert!(
            mock.0.lock().unwrap().ensured_profiles.is_empty(),
            "no half-identity principal may be created"
        );
    }

    #[tokio::test]
    async fn operator_known_identity_survives_directory_down() {
        let mock = Arc::new(Mock::default());
        mock.0.lock().unwrap().cached_principal = Some(Principal {
            id: "P-old".into(),
            kind: PrincipalKind::Human,
            emp_no: Some("0001".into()),
            display_name: None,
            dept_id: Some("D9".into()),
            dept_name: None,
            profile_synced_at: Some(Utc::now() - chrono::Duration::hours(48)), // stale
            created_at: Utc::now(),
        });
        let svc = service_with_dir(mock.clone(), MockDirectory(Err("boom".into())));
        let got = svc
            .resolve_operator_actor("W1", PrincipalSource::Wecom, "u1")
            .await
            .unwrap()
            .expect("cached identity keeps working");
        assert_eq!(got.principal_id, "P-old");
        assert_eq!(got.dept_id.as_deref(), Some("D9"), "stale cache still grants dept");
    }

    #[tokio::test]
    async fn operator_fresh_cache_skips_directory() {
        let mock = Arc::new(Mock::default());
        mock.0.lock().unwrap().cached_principal = Some(Principal {
            id: "P-fresh".into(),
            kind: PrincipalKind::Human,
            emp_no: Some("0001".into()),
            display_name: None,
            dept_id: Some("D9".into()),
            dept_name: None,
            profile_synced_at: Some(Utc::now()),
            created_at: Utc::now(),
        });
        // Directory would error if called — fresh cache must not call it.
        let svc = service_with_dir(mock.clone(), MockDirectory(Err("must not be called".into())));
        let got = svc
            .resolve_operator_actor("W1", PrincipalSource::Wecom, "u1")
            .await
            .unwrap()
            .expect("fresh cache resolves");
        assert_eq!(got.principal_id, "P-fresh");
    }

    #[tokio::test]
    async fn operator_directory_lookup_populates_profile() {
        let mock = Arc::new(Mock::default());
        let svc = service_with_dir(
            mock.clone(),
            MockDirectory(Ok(Some(person("0042", Some("D7"))))),
        );
        let got = svc
            .resolve_operator_actor("W1", PrincipalSource::Emp, "0042")
            .await
            .unwrap()
            .expect("directory-backed resolve");
        assert_eq!(got.dept_id.as_deref(), Some("D7"));
        let profiles = mock.0.lock().unwrap().ensured_profiles.clone();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].as_ref().unwrap().emp_no, "0042");
    }

    #[tokio::test]
    async fn operator_without_directory_resolves_per_entrance() {
        let mock = Arc::new(Mock::default());
        let svc = service(mock.clone()); // directory = None
        let got = svc
            .resolve_operator_actor("W1", PrincipalSource::Wecom, "u9")
            .await
            .unwrap()
            .expect("identity-only mode resolves");
        assert_eq!(got.principal_id, "P-u9");
        assert!(got.dept_id.is_none(), "no directory, no dept domain");
    }

    #[tokio::test]
    async fn self_scope_targets_key_principal_when_operator_present() {
        let mock = Arc::new(Mock::default());
        let svc = service(mock.clone());
        let a = MemoryActor {
            principal_id: "P-human".into(),
            self_principal_id: Some("P-key".into()),
            ..actor()
        };
        let mut inp = save_input(MemoryScope::SelfScope, MemoryKind::Fact);
        inp.content = "agent state".into();
        svc.save(&a, inp).await.unwrap();
        let mut inp = save_input(MemoryScope::Mine, MemoryKind::Fact);
        inp.content = "human note".into();
        svc.save(&a, inp).await.unwrap();
        let s = mock.0.lock().unwrap();
        assert_eq!(s.inserted[0].scope_id, "P-key", "self follows the agent");
        assert_eq!(s.inserted[1].scope_id, "P-human", "mine follows the human");
    }

    #[tokio::test]
    async fn team_only_actor_rejects_private_scopes_and_reads_team() {
        let mock = Arc::new(Mock::default());
        let svc = service(mock.clone());
        let a = MemoryActor {
            team_only: true,
            ..actor()
        };
        for scope in [MemoryScope::Mine, MemoryScope::SelfScope, MemoryScope::Dept] {
            let err = svc.save(&a, save_input(scope, MemoryKind::Fact)).await.unwrap_err();
            assert!(matches!(err, VedaError::InvalidInput(_)), "{scope:?}: {err:?}");
        }
        // Team writes still work, and delete's allowed domains shrink to team.
        svc.save(&a, save_input(MemoryScope::Team, MemoryKind::Fact)).await.unwrap();
        assert_eq!(
            allowed_scopes(&a),
            vec![(MemoryScopeType::Workspace, "W1".to_string())],
            "no personal/dept write surface while degraded"
        );
        assert!(
            matches!(context_filter(&a), MemoryScopeFilter::Scope { scope_type: MemoryScopeType::Workspace, .. }),
            "context collapses to the team domain"
        );
    }

    #[tokio::test]
    async fn browse_tabs_resolve_to_single_domain_filters() {
        let mock = Arc::new(Mock::default());
        let svc = service(mock.clone());
        let mut a = actor();
        a.dept_id = Some("D1".into());

        svc.list(&a, MemoryScope::Mine, None, None, 1, 20).await.unwrap();
        svc.list(&a, MemoryScope::Team, None, None, 1, 20).await.unwrap();
        svc.topics(&a, MemoryScope::Dept).await.unwrap();

        let filters = mock.0.lock().unwrap().list_filters.clone();
        assert!(
            matches!(&filters[0], MemoryScopeFilter::Personal { principal_id, workspace_id }
                if principal_id == "P1" && workspace_id == "W1"),
            "mine tab must be origin-restricted, got {:?}",
            filters[0]
        );
        assert!(matches!(&filters[1], MemoryScopeFilter::Scope { scope_type: MemoryScopeType::Workspace, scope_id } if scope_id == "W1"));
        assert!(matches!(&filters[2], MemoryScopeFilter::Scope { scope_type: MemoryScopeType::Dept, scope_id } if scope_id == "D1"));
    }

    #[tokio::test]
    async fn browse_rejects_self_tab_missing_dept_and_degraded_private() {
        let mock = Arc::new(Mock::default());
        let svc = service(mock.clone());
        let err = svc.list(&actor(), MemoryScope::SelfScope, None, None, 1, 20).await.unwrap_err();
        assert!(matches!(err, VedaError::InvalidInput(_)));
        // No department on the actor → dept tab errors instead of silently empty.
        let err = svc.topics(&actor(), MemoryScope::Dept).await.unwrap_err();
        assert!(matches!(err, VedaError::InvalidInput(_)));
        // Degraded operator: private tabs reject, team still lists.
        let degraded = MemoryActor {
            team_only: true,
            ..actor()
        };
        let err = svc.list(&degraded, MemoryScope::Mine, None, None, 1, 20).await.unwrap_err();
        assert!(matches!(err, VedaError::InvalidInput(_)));
        svc.list(&degraded, MemoryScope::Team, None, None, 1, 20).await.unwrap();
    }

    #[tokio::test]
    async fn admin_delete_scopes_to_team_domain_only() {
        let mock = Arc::new(Mock::default());
        let svc = service(mock.clone());
        svc.admin_delete_team("W1", 42).await.unwrap();
        let s = mock.0.lock().unwrap();
        assert_eq!(
            s.deleted_allowed[0],
            vec![(MemoryScopeType::Workspace, "W1".to_string())],
            "admin cleanup must not reach personal/dept domains"
        );
        assert_eq!(s.vectors_deleted, vec![42], "vector goes with the row");
    }

    #[tokio::test]
    async fn operator_dept_dropped_when_directory_forgets_person() {
        let mock = Arc::new(Mock::default());
        mock.0.lock().unwrap().cached_principal = Some(Principal {
            id: "P-left".into(),
            kind: PrincipalKind::Human,
            emp_no: Some("0009".into()),
            display_name: None,
            dept_id: Some("D9".into()),
            dept_name: Some("旧部门".into()),
            profile_synced_at: Some(Utc::now() - chrono::Duration::hours(48)), // stale
            created_at: Utc::now(),
        });
        // Directory answers authoritatively: no such person anymore.
        let svc = service_with_dir(mock.clone(), MockDirectory(Ok(None)));
        let got = svc
            .resolve_operator_actor("W1", PrincipalSource::Wecom, "u-left")
            .await
            .unwrap()
            .expect("known identity keeps resolving");
        assert_eq!(got.principal_id, "P-left", "personal notes keep working");
        assert!(got.dept_id.is_none(), "dept authorization must not outlive the TTL");
    }
}
