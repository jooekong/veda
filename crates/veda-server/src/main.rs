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
use veda_core::store::{LlmService, TaskQueue, VectorStore};
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
    // Single positional config path; demo phase, no flags.
    let mut config_path = "config/server.toml".to_string();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--help" | "-h" => {
                eprintln!("Usage: veda-server [config.toml]");
                return Ok(());
            }
            other if !other.starts_with("--") => config_path = other.to_string(),
            other => anyhow::bail!("unknown flag: {other}"),
        }
    }
    let cfg = ServerConfig::load(&config_path)?;
    auth::validate_jwt_secret(&cfg.jwt_secret)?;
    info!(listen = %cfg.listen, "starting veda-server");

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
    info!("running schema bootstrap (CREATE TABLE IF NOT EXISTS)");
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
        cfg.embedding.batch_size,
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
        let events_days = cfg.retention.events_retention_days.max(1);
        let outbox_days = cfg.retention.outbox_retention_days.max(1);
        let svc = app_state.fs_service.clone();
        let outbox = mysql.clone();
        let mut rx = shutdown_rx.clone();
        info!(
            interval_secs = cfg.retention.interval_secs,
            events_retention_days = events_days,
            outbox_retention_days = outbox_days,
            "retention sweep enabled (fs_events + outbox)"
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
                let now = chrono::Utc::now();
                let events_cutoff = now - chrono::Duration::days(events_days);
                match svc.prune_events_older_than(events_cutoff).await {
                    Ok(n) => {
                        ::metrics::counter!("veda_fs_events_retention_swept_total").increment(n);
                        if n > 0 {
                            info!(deleted = n, cutoff = %events_cutoff, "fs_events retention swept");
                        }
                    }
                    Err(e) => tracing::warn!(err = %e, "fs_events retention sweep failed"),
                }
                let outbox_cutoff = now - chrono::Duration::days(outbox_days);
                match outbox.prune_outbox_older_than(outbox_cutoff).await {
                    Ok(n) => {
                        ::metrics::counter!("veda_outbox_retention_swept_total").increment(n);
                        if n > 0 {
                            info!(deleted = n, cutoff = %outbox_cutoff, "outbox retention swept");
                        }
                    }
                    Err(e) => tracing::warn!(err = %e, "outbox retention sweep failed"),
                }
            }
        }))
    } else {
        info!("retention sweep disabled (fs_events + outbox)");
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

