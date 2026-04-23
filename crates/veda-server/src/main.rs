mod auth;
mod config;
mod error;
mod routes;
mod state;
mod worker;

use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::watch;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;
use veda_core::service::collection::CollectionService;
use veda_core::service::fs::FsService;
use veda_core::service::search::SearchService;
use veda_core::store::VectorStore;
use veda_pipeline::embedding::EmbeddingProvider;
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

    let mysql = Arc::new(MysqlStore::new(&cfg.mysql.database_url).await?);
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
        sql_engine,
        jwt_secret: cfg.jwt_secret.clone(),
    });

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let worker_handle = if cfg.worker.enabled {
        let w = Worker::new(
            mysql.clone(),
            mysql.clone(),
            milvus.clone(),
            embedding.clone(),
            cfg.worker.batch_size,
            cfg.worker.poll_interval_secs,
        );
        Some(tokio::spawn(async move {
            w.run(shutdown_rx).await;
        }))
    } else {
        None
    };

    let app = routes::build_router(app_state)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive());

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
