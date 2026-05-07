use veda_server::{auth, config, obs, reconciler, routes, state, worker};

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
use veda_core::store::{AuthStore, LlmService, MetadataStore, TaskQueue, VectorStore};
use veda_types::types::{OutboxEvent, OutboxEventType, OutboxStatus};
use veda_pipeline::embedding::EmbeddingProvider;
use veda_pipeline::llm::LlmProvider;
use veda_store::{MilvusStore, MysqlStore, PoolConfig};

use config::ServerConfig;
use state::AppState;
use worker::Worker;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Default to INFO so first-run logs are useful out of the box;
    // RUST_LOG (e.g. "info,veda=debug") still wins for ops tuning.
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // Install the global metrics recorder before any module fires a
    // `metrics::*!` macro. Subsequent install attempts panic, so this
    // must happen exactly once and early.
    let metrics = obs::install();

    // Minimal CLI parsing without bringing in clap for the binary.
    // Positional config path + `--skip-migrate`. Order-insensitive.
    let mut config_path = "config/server.toml".to_string();
    let mut skip_migrate = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--skip-migrate" => skip_migrate = true,
            "--help" | "-h" => {
                eprintln!(
                    "Usage: veda-server [config.toml] [--skip-migrate]\n\
                     \n\
                     --skip-migrate  Don't run schema migrations on startup.\n\
                                     Run `veda-migrate` separately before this.\n\
                     \n\
                     Multi-replica deploys should always pass --skip-migrate\n\
                     to avoid CREATE TABLE races between replicas, and rely\n\
                     on a one-off migration job."
                );
                return Ok(());
            }
            other if !other.starts_with("--") => config_path = other.to_string(),
            other => anyhow::bail!("unknown flag: {other}"),
        }
    }
    let cfg = ServerConfig::load(&config_path)?;
    auth::validate_jwt_secret(&cfg.jwt_secret)?;
    info!(listen = %cfg.listen, skip_migrate, "starting veda-server");

    let mysql = Arc::new(
        MysqlStore::with_pool_config(
            &cfg.mysql.database_url,
            PoolConfig {
                max_connections: cfg.mysql.max_connections,
                min_connections: cfg.mysql.min_connections,
                acquire_timeout_secs: cfg.mysql.acquire_timeout_secs,
                idle_timeout_secs: cfg.mysql.idle_timeout_secs,
                max_lifetime_secs: cfg.mysql.max_lifetime_secs,
            },
        )
        .await?,
    );
    if !skip_migrate {
        info!("running schema migrations");
        mysql.migrate().await?;
    } else {
        info!("--skip-migrate set, skipping schema migrations");
    }

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
        cfg.embedding.batch_size,
    )?);

    let chunks_rebuilt = milvus.init_collections(cfg.embedding.dimension).await?;
    if chunks_rebuilt {
        // Schema migration just dropped the chunk collection; the worker
        // wouldn't otherwise repopulate it because it relies on outbox
        // events. Walk every workspace's dentries and enqueue a forced
        // ChunkSync for each file so the new BM25 sparse_vector field
        // gets populated for everything that existed pre-migration.
        reembed_all_files_for_migration(&mysql).await?;
    }

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
        metrics: metrics.clone(),
        metrics_token: cfg.metrics_token.clone(),
        summary_enabled: cfg.llm.is_some(),
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
        let rx = shutdown_rx.clone();
        Some(tokio::spawn(async move {
            w.run(rx).await;
        }))
    } else {
        None
    };

    // Pool stats sampler: emits veda_mysql_pool_{connections,idle} every 10s.
    // Lives for the duration of the server (no shutdown handle — gets dropped
    // when the runtime exits).
    let pool_metrics = mysql.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let s = pool_metrics.pool_stats();
            ::metrics::gauge!("veda_mysql_pool_connections").set(s.size as f64);
            ::metrics::gauge!("veda_mysql_pool_idle").set(s.idle as f64);
        }
    });

    let retention_handle = if cfg.retention.enabled {
        let interval = std::time::Duration::from_secs(cfg.retention.interval_secs.max(60));
        let days = cfg.retention.events_retention_days.max(1);
        let svc = app_state.fs_service.clone();
        let mut rx = shutdown_rx.clone();
        info!(
            interval_secs = cfg.retention.interval_secs,
            events_retention_days = days,
            "fs_events retention sweep enabled"
        );
        Some(tokio::spawn(async move {
            // Drift the first sweep by `interval` so a fresh boot doesn't
            // immediately delete; gives ops time to ctrl-c if config is wrong.
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // first tick fires immediately — discard
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = rx.changed() => {
                        if *rx.borrow() { return; }
                    }
                }
                let cutoff = chrono::Utc::now() - chrono::Duration::days(days);
                match svc.prune_events_older_than(cutoff).await {
                    Ok(n) if n > 0 => info!(deleted = n, cutoff = %cutoff, "fs_events retention swept"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(err = %e, "fs_events retention sweep failed"),
                }
            }
        }))
    } else {
        info!("fs_events retention sweep disabled");
        None
    };

    let reconciler_handle = if cfg.reconciler.enabled {
        let r = reconciler::Reconciler::new(
            mysql.clone(),
            mysql.clone(),
            milvus.clone(),
            mysql.clone(),
            cfg.reconciler.interval_secs,
        );
        let rx = shutdown_rx.clone();
        info!(
            interval_secs = cfg.reconciler.interval_secs,
            "reconciler enabled"
        );
        Some(tokio::spawn(async move {
            r.run(rx).await;
        }))
    } else {
        info!("reconciler disabled");
        None
    };

    let cors = if !cfg.allowed_origins.is_empty() {
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
    } else if cfg.dev_mode {
        tracing::warn!("dev_mode=true: CORS is permissive — do NOT use in production");
        CorsLayer::permissive()
    } else {
        // Default-deny: empty allowed_origins + dev_mode=false means
        // cross-origin browser requests are blocked. Same-origin still works.
        // Configure `allowed_origins` to whitelist trusted frontends.
        info!("allowed_origins empty: cross-origin requests will be denied");
        CorsLayer::new()
    };

    let app = routes::build_router(app_state)
        .layer(axum::middleware::from_fn(obs::track_http))
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
    if let Some(handle) = reconciler_handle {
        let _ = handle.await;
    }
    if let Some(handle) = retention_handle {
        let _ = handle.await;
    }

    Ok(())
}

/// Walks every active workspace, collects every file dentry, and enqueues a
/// forced ChunkSync into the outbox for each one. Used after a chunk-collection
/// schema migration so the worker repopulates the new fields (e.g. BM25
/// sparse_vector) for files that already existed pre-migration. Idempotent —
/// `enqueue` is `INSERT IGNORE` style on the outbox table; if the same file
/// is queued twice, the second is a no-op.
async fn reembed_all_files_for_migration(mysql: &Arc<MysqlStore>) -> anyhow::Result<()> {
    let auth: &dyn AuthStore = mysql.as_ref();
    let meta: &dyn MetadataStore = mysql.as_ref();
    let queue: &dyn TaskQueue = mysql.as_ref();

    let workspace_ids = auth.list_active_workspace_ids().await?;
    info!(
        workspaces = workspace_ids.len(),
        "schema migration: re-enqueueing ChunkSync for every file in every workspace"
    );

    let now = chrono::Utc::now();
    let mut total = 0usize;
    const PAGE_SIZE: usize = 1000;
    for ws in &workspace_ids {
        // Stream paginated listing instead of capped — a workspace with
        // > 50k files would otherwise crash the server on startup with
        // QuotaExceeded.
        let mut after: Option<String> = None;
        loop {
            let dentries = meta
                .list_dentries_under_page(ws, "/", after.as_deref(), PAGE_SIZE)
                .await?;
            if dentries.is_empty() {
                break;
            }
            let last_path = dentries.last().map(|d| d.path.clone());
            for d in dentries {
                let Some(file_id) = d.file_id.clone() else {
                    continue;
                };
                // has_pending_event keeps this idempotent if the function
                // is somehow re-entered (e.g. server restart mid-migration).
                if queue
                    .has_pending_event(OutboxEventType::ChunkSync, ws, "file_id", &file_id)
                    .await?
                {
                    continue;
                }
                let event = OutboxEvent {
                    id: 0,
                    workspace_id: ws.clone(),
                    event_type: OutboxEventType::ChunkSync,
                    payload: serde_json::json!({
                        "file_id": file_id,
                        "force_reembed": true,
                    }),
                    status: OutboxStatus::Pending,
                    retry_count: 0,
                    max_retries: 3,
                    available_at: now,
                    lease_until: None,
                    created_at: now,
                };
                queue.enqueue(&event).await?;
                total += 1;
            }
            after = last_path;
        }
    }
    info!(
        files_enqueued = total,
        "schema migration: ChunkSync events enqueued; worker will rebuild chunks"
    );
    Ok(())
}
