use std::sync::Arc;
use veda_core::service::collection::CollectionService;
use veda_core::service::fs::FsService;
use veda_core::service::search::SearchService;
use veda_core::store::{AuthStore, MetadataStore, VectorStore};
use veda_sql::VedaSqlEngine;
use veda_store::MilvusStore;

use crate::obs::MetricsHandle;

pub struct AppState {
    pub fs_service: Arc<FsService>,
    pub search_service: SearchService,
    pub collection_service: CollectionService,
    pub auth_store: Arc<dyn AuthStore>,
    pub meta_store: Arc<dyn MetadataStore>,
    pub vector_store: Arc<dyn VectorStore>,
    /// Concrete `MilvusStore` ref used by db-kind workspace provisioning
    /// (see routes/account.rs::create_workspace). `vector_store` is the same
    /// instance via trait object; this field exposes inherent methods
    /// (`create_vector_collection`, `drop_collection`).
    pub milvus: Arc<MilvusStore>,
    /// Embedding dim from `config.embedding.dimension`. Stamped into Milvus
    /// vector field on db workspace collection creation.
    pub embedding_dim: u32,
    pub sql_engine: VedaSqlEngine,
    pub jwt_secret: String,
    pub metrics: MetricsHandle,
    /// Bearer token required to read `/v1/metrics`. `None` disables the
    /// endpoint entirely (returns 404). See `ServerConfig::metrics_token`.
    pub metrics_token: Option<String>,
    /// Whether [llm] is configured. When false, summary generation is
    /// permanently disabled, and `GET /v1/summary/...` returns 501 Not
    /// Implemented instead of the misleading 202 "pending".
    pub summary_enabled: bool,
}
