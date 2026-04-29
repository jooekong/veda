use veda_server::{auth, config, routes, state, worker};

use std::sync::Arc;

use axum::http::{header, HeaderValue, Method};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;
use veda_core::service::collection::CollectionService;
use veda_core::service::fs::FsService;
use veda_core::service::search::SearchService;
use veda_core::store::{LlmService, VectorStore};
use veda_pipeline::embedding::EmbeddingProvider;
use veda_pipeline::llm::LlmProvider;
use veda_store::{MilvusStore, MysqlStore};

use config::ServerConfig;
use state::AppState;
use worker::Worker;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/server.toml".into());
    let cfg = ServerConfig::load(&config_path)?;
    auth::validate_jwt_secret(&cfg.jwt_secret)?;
    info!(listen = %cfg.listen, "starting veda-server");

    let mysql = Arc::new(
        MysqlStore::with_max_connections(&cfg.mysql.database_url, cfg.mysql.max_connections)
            .await?,
    );
    mysql.migrate().await?;

    let milvus = Arc::new(MilvusStore::new(
        &cfg.milvus.url,
        cfg.milvus.token.clone(),
        cfg.milvus.db.clone(),
    ));

    let embedding = Arc::new(EmbeddingProvider::new(
        &cfg.embedding.api_url,
        &cfg.embedding.api_key,
        &cfg.embedding.model,
        Some(cfg.embedding.dimension),
    )?);

    milvus.init_collections(cfg.embedding.dimension).await?;

    let fs_service = Arc::new(FsService::new(mysql.clone()));
    let search_service = SearchService::new(mysql.clone(), milvus.clone(), embedding.clone());
    let collection_service =
        CollectionService::new(mysql.clone(), milvus.clone(), embedding.clone());

    let sql_engine = veda_sql::VedaSqlEngine::new(
        mysql.clone(),
        milvus.clone(),
        mysql.clone(),
        milvus.clone(),
        embedding.clone(),
        fs_service.clone(),
    );

    let app_state = Arc::new(AppState {
        fs_service,
        search_service,
        collection_service,
        auth_store: mysql.clone(),
        meta_store: mysql.clone(),
        vector_store: milvus.clone(),
        sql_engine,
        jwt_secret: cfg.jwt_secret.clone(),
    });

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let llm: Option<Arc<dyn LlmService>> = match &cfg.llm {
        Some(llm_cfg) => {
            let provider = LlmProvider::new(&llm_cfg.api_url, &llm_cfg.api_key, &llm_cfg.model)?;
            info!(model = %llm_cfg.model, "LLM summary service enabled");
            Some(Arc::new(provider))
        }
        None => {
            info!("LLM config not set, summary generation disabled");
            None
        }
    };

    let worker_handle = if cfg.worker.enabled {
        let max_overview_tokens = cfg
            .llm
            .as_ref()
            .map(|c| c.max_summary_tokens)
            .unwrap_or(2048);
        let w = Worker::new(
            mysql.clone(),
            mysql.clone(),
            milvus.clone(),
            embedding.clone(),
            llm.clone(),
            cfg.worker.batch_size,
            cfg.worker.poll_interval_secs,
            max_overview_tokens,
        );
        Some(tokio::spawn(async move {
            w.run(shutdown_rx).await;
        }))
    } else {
        None
    };

    let cors = if cfg.allowed_origins.is_empty() {
        CorsLayer::permissive()
    } else {
        let origins: Vec<HeaderValue> = cfg
            .allowed_origins
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect();
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::DELETE,
                Method::HEAD,
            ])
            .allow_headers([
                header::AUTHORIZATION,
                header::CONTENT_TYPE,
                header::IF_MATCH,
                header::IF_NONE_MATCH,
                header::RANGE,
            ])
    };

    let app = routes::build_router(app_state)
        .layer(TraceLayer::new_for_http())
        .layer(cors);

    let listener = TcpListener::bind(&cfg.listen).await?;
    info!(addr = %cfg.listen, "server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            info!("shutdown signal received");
            let _ = shutdown_tx.send(true);
        })
        .await?;

    if let Some(handle) = worker_handle {
        let _ = handle.await;
    }

    Ok(())
}
