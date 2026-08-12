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
use crate::store::{EmbeddingService, MemoryStore, MemoryVectorStore};

/// Memories are one-liners; anything longer belongs in a document.
const MAX_CONTENT_CHARS: usize = 4096;
const MAX_TOPIC_CHARS: usize = 128;
/// Neighbors returned by save (guides the agent toward update-vs-new).
const NEIGHBOR_LIMIT: usize = 3;
/// Candidate over-fetch: the MySQL recheck drops deleted/expired rows, so
/// under-fetching would return fewer than `limit` live hits.
const OVERFETCH: usize = 2;
/// A topicless save inherits the top neighbor's topic only above this
/// cosine score — below it the memory is genuinely new, leave topic unset.
const TOPIC_INHERIT_MIN_SCORE: f32 = 0.75;
/// Evidence pointers are references, not payloads.
const MAX_SOURCE_REF_BYTES: usize = 4096;

/// Server-resolved identities for one call. `principal_id` comes from the
/// request identity (M1: the wk_ key), never from client input — that rule
/// is what makes the personal domain private.
#[derive(Debug, Clone)]
pub struct MemoryActor {
    pub workspace_id: String,
    pub principal_id: String,
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
}

impl MemoryService {
    pub fn new(
        store: Arc<dyn MemoryStore>,
        vector: Arc<dyn MemoryVectorStore>,
        embedding: Arc<dyn EmbeddingService>,
    ) -> Self {
        Self {
            store,
            vector,
            embedding,
        }
    }

    /// M1 identity resolution: the wk_ key is the person (one key per human
    /// or per agent). First sighting lazily creates the principal.
    pub async fn resolve_key_actor(&self, workspace_id: &str, key_id: &str) -> Result<MemoryActor> {
        let principal = self
            .store
            .ensure_principal(PrincipalSource::Key, key_id, PrincipalKind::Human, None)
            .await?;
        Ok(MemoryActor {
            workspace_id: workspace_id.to_string(),
            principal_id: principal.id,
        })
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

        let (scope_type, scope_id) = resolve_scope(actor, input.scope);
        let origin = resolve_origin(actor, input.scope, input.kind, input.origin.as_deref());

        let vector = self.embed_one(content).await?;
        let domain = MemoryScopeFilter::Scope {
            scope_type,
            scope_id: scope_id.clone(),
        };
        let neighbors = self
            .lookup_candidates(&vector, &domain, NEIGHBOR_LIMIT)
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
                let (scope_type, scope_id) = resolve_scope(actor, s);
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
        };
        let memory = self
            .store
            .update_memory(id, &allowed_scopes(actor), &patch, &actor.principal_id)
            .await?;

        if patch.content.is_some() {
            // Sync re-embed for immediate ranking freshness; the store
            // committed a MemorySync task with the row change, so failures
            // here cost latency, not convergence.
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
        let deleted = self.store.delete_memory(id, &allowed_scopes(actor)).await?;
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
            .lookup_candidates(&vector, filter, limit)
            .await?;
        let ids: Vec<i64> = hits.iter().map(|h| h.memory.id).collect();
        if !ids.is_empty() {
            if let Err(e) = self.store.touch_memories(&ids).await {
                warn!(err = %e, "touch_memories failed");
            }
        }
        Ok(hits)
    }

    /// Candidates from Milvus (over-fetched), rechecked against MySQL under
    /// the same filter, returned in candidate (similarity) order.
    async fn lookup_candidates(
        &self,
        vector: &[f32],
        filter: &MemoryScopeFilter,
        limit: usize,
    ) -> Result<Vec<MemoryHit>> {
        let candidates = self
            .vector
            .search_memory_candidates(vector, filter, limit * OVERFETCH)
            .await?;
        if candidates.is_empty() {
            return Ok(vec![]);
        }
        let ids: Vec<i64> = candidates.iter().map(|c| c.id).collect();
        let rows = self.store.get_memories_by_ids(&ids, filter).await?;
        let by_id: std::collections::HashMap<i64, Memory> =
            rows.into_iter().map(|m| (m.id, m)).collect();
        let mut out = Vec::with_capacity(limit);
        for c in candidates {
            if let Some(m) = by_id.get(&c.id) {
                out.push(MemoryHit {
                    memory: m.clone(),
                    score: c.score,
                });
                if out.len() >= limit {
                    break;
                }
            }
        }
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

fn resolve_scope(actor: &MemoryActor, scope: MemoryScope) -> (MemoryScopeType, String) {
    match scope {
        MemoryScope::Team => (MemoryScopeType::Workspace, actor.workspace_id.clone()),
        // M1 identity is key-only, so the operator and the agent are the
        // same principal; Mine and Self diverge once richer identity
        // sources land (M2 gateway, M3 wecom).
        MemoryScope::Mine | MemoryScope::SelfScope => {
            (MemoryScopeType::Principal, actor.principal_id.clone())
        }
    }
}

/// Origin defaulting (design §4.2): never validated, only defaulted.
/// Team memories carry no origin at all.
fn resolve_origin(
    actor: &MemoryActor,
    scope: MemoryScope,
    kind: MemoryKind,
    origin: Option<&str>,
) -> Option<String> {
    if matches!(scope, MemoryScope::Team) {
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
    MemoryScopeFilter::Context {
        workspace_id: actor.workspace_id.clone(),
        principal_id: actor.principal_id.clone(),
    }
}

/// The caller's writable domains: the current workspace's team domain and
/// their own personal domain. Update/delete WHERE clauses are built from
/// this — sharing the discipline that no memory write escapes its scopes.
fn allowed_scopes(actor: &MemoryActor) -> Vec<(MemoryScopeType, String)> {
    vec![
        (MemoryScopeType::Workspace, actor.workspace_id.clone()),
        (MemoryScopeType::Principal, actor.principal_id.clone()),
    ]
}

fn embedding_row(memory: &Memory, vector: Vec<f32>) -> MemoryWithEmbedding {
    MemoryWithEmbedding {
        id: memory.id,
        scope_type: memory.scope_type,
        scope_id: memory.scope_id.clone(),
        origin_workspace_id: memory.origin_workspace_id.clone().unwrap_or_default(),
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
        fail_vector_upsert: bool,
        duplicate_of: Option<i64>,
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
            _allowed: &[(MemoryScopeType, String)],
        ) -> Result<bool> {
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

        async fn ensure_principal(
            &self,
            _source: PrincipalSource,
            _external_id: &str,
            _kind: PrincipalKind,
            _display_name: Option<&str>,
        ) -> Result<Principal> {
            unimplemented!()
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
        MemoryService::new(mock.clone(), mock.clone(), Arc::new(MockEmbed))
    }

    fn actor() -> MemoryActor {
        MemoryActor {
            workspace_id: "W1".into(),
            principal_id: "P1".into(),
        }
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
}
