use std::sync::Arc;
use veda_core::service::collection::CollectionService;
use veda_core::service::fs::FsService;
use veda_core::service::search::SearchService;
use veda_core::store::{AuthStore, MetadataStore, VectorStore};
use veda_sql::VedaSqlEngine;

pub struct AppState {
    pub fs_service: Arc<FsService>,
    pub search_service: SearchService,
    pub collection_service: CollectionService,
    pub auth_store: Arc<dyn AuthStore>,
    pub meta_store: Arc<dyn MetadataStore>,
    pub vector_store: Arc<dyn VectorStore>,
    pub sql_engine: VedaSqlEngine,
    pub jwt_secret: String,
}
