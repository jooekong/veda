use std::sync::Arc;
use veda_core::service::collection::CollectionService;
use veda_core::service::fs::FsService;
use veda_core::service::search::SearchService;
use veda_core::store::{AuthStore, EmbeddingService, MetadataStore, VectorStore, VectorWorkspaceStore};
use veda_sql::VedaSqlEngine;

use crate::obs::MetricsHandle;

pub struct AppState {
    pub fs_service: Arc<FsService>,
    pub search_service: SearchService,
    pub collection_service: CollectionService,
    pub auth_store: Arc<dyn AuthStore>,
    pub meta_store: Arc<dyn MetadataStore>,
    pub vector_store: Arc<dyn VectorStore>,
    /// On-demand MySQL↔Milvus reconciler (no background loop). Driven by
    /// `POST /admin/v1/reconcile/{ws}`. See `crate::reconciler`.
    pub reconciler: Arc<crate::reconciler::Reconciler>,
    /// db-kind workspace collection lifecycle (create/drop). Separate from
    /// `vector_store` (fs-side chunk/summary ops); both currently happen to
    /// be the same MilvusStore instance, but the trait split lets Stage 4+
    /// stub the vector path independently of fs.
    pub vector_workspace_store: Arc<dyn VectorWorkspaceStore>,
    /// L1-cached embedding provider used by the vectors data plane (Stage 4).
    /// Wraps the raw HTTP EmbeddingProvider with moka cache; cache hits skip
    /// upstream, misses are batched into one upstream call per `embed()`.
    /// Separate from the fs-side embedding (Stage 3.1 cache is vector-only).
    pub vector_embedding: Arc<dyn EmbeddingService>,
    /// Embedding dim from `config.embedding.dimension`. Stamped into Milvus
    /// vector field on db workspace collection creation.
    pub embedding_dim: u32,
    pub sql_engine: VedaSqlEngine,
    pub metrics: MetricsHandle,
    /// Bearer token required to read `/v1/metrics`. `None` disables the
    /// endpoint entirely (returns 404). See `ServerConfig::metrics_token`.
    pub metrics_token: Option<String>,
    /// Bearer token gating the read-only admin surface (`/admin/v1/*`).
    /// `None` disables it entirely — every admin route 404s, so an
    /// unconfigured node exposes no cross-tenant data. See
    /// `ServerConfig::admin_token`.
    pub admin_token: Option<String>,
    /// Whether [llm] is configured. When false, summary generation is
    /// permanently disabled, and `GET /v1/summary/...` returns 501 Not
    /// Implemented instead of the misleading 202 "pending".
    pub summary_enabled: bool,
    /// Flipped by the SIGTERM handler at the start of the drain window
    /// (`ServerConfig::drain_secs`). While set, `/v1/ready` reports 503
    /// "draining" so the LB pulls this node, but the listener keeps
    /// serving until the window elapses. `/healthz` is unaffected.
    pub draining: std::sync::atomic::AtomicBool,
}
