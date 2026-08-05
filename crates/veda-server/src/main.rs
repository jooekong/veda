use veda_server::{config, obs, reconciler, routes, state, tunnel_bots, worker};

use std::sync::Arc;

use axum::http::{header, HeaderValue, Method};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;
use veda_core::service::answer::{AnswerParams, AnswerService};
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

    let config_path = match parse_args(std::env::args().skip(1))? {
        Cli::Help => {
            eprintln!("Usage: veda-server [config.toml]");
            return Ok(());
        }
        Cli::Version => {
            // stdout, exact `veda-server <version>`: the deploy runbook
            // asserts on this string to prove which build it just swapped in.
            println!("veda-server {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Cli::Run { config_path } => config_path,
    };
    let cfg = ServerConfig::load(&config_path)?;
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

    let embedding = Arc::new(EmbeddingProvider::new_tuned(
        &cfg.embedding.api_url,
        &cfg.embedding.api_key,
        &cfg.embedding.model,
        Some(cfg.embedding.dimension),
        cfg.embedding.batch_size,
        cfg.embedding.max_concurrency,
    )?);
    // Worker indexing embeds at LOW gate priority: idle it may saturate
    // every permit, but interactive callers get the next freed one.
    let embedding_bg = embedding.background();
    // Vector data plane (db-kind workspace) gets its own L1 cache wrap.
    // fs path uses the raw `embedding` to avoid double-caching with the
    // existing search/collection services.
    let vector_embedding: Arc<dyn veda_core::store::EmbeddingService> =
        Arc::new(veda_pipeline::embedding::EmbeddingCache::new(
            embedding.clone(),
            &cfg.embedding.model,
        ));

    milvus.init_collections(cfg.embedding.dimension).await?;

    let fs_service = Arc::new(FsService::new(mysql.clone()));
    let search_service = SearchService::new(mysql.clone(), milvus.clone(), embedding.clone());
    let collection_service =
        CollectionService::new(mysql.clone(), milvus.clone(), embedding.clone());
    let vector_service = veda_core::service::vector::VectorService::new(
        milvus.clone(),
        vector_embedding.clone(),
        mysql.clone(),
    );
    let workspace_service = veda_core::service::workspace::WorkspaceService::new(
        mysql.clone(),
        milvus.clone(),
        cfg.embedding.dimension,
    );

    let sql_engine = veda_sql::VedaSqlEngine::new(
        mysql.clone(),
        milvus.clone(),
        mysql.clone(),
        milvus.clone(),
        embedding.clone(),
        fs_service.clone(),
    );

    // On-demand MySQL↔Milvus reconciler. No background loop — driven by
    // POST /admin/v1/reconcile/{ws}. An operator runs it attended, and the
    // in-pass get_file/has_pending_event re-checks guard the read-skew race
    // within a single pass.
    let reconciler = Arc::new(reconciler::Reconciler::new(
        mysql.clone(),
        mysql.clone(),
        milvus.clone(),
        mysql.clone(),
    ));

    let llm: Option<Arc<dyn LlmService>> = match &cfg.llm {
        Some(llm_cfg) => {
            let provider = LlmProvider::new(
                &llm_cfg.api_url,
                &llm_cfg.api_key,
                &llm_cfg.model,
                llm_cfg.summary_disable_thinking,
            )?
            .with_summary_fallback(llm_cfg.summary_fallback_model.clone());
            info!(
                model = %llm_cfg.model,
                summary_disable_thinking = llm_cfg.summary_disable_thinking,
                summary_fallback_model = llm_cfg.summary_fallback_model.as_deref().unwrap_or("-"),
                "LLM summary service enabled"
            );
            Some(Arc::new(provider))
        }
        None => {
            info!("LLM config not set, summary generation disabled");
            None
        }
    };

    // Agentic RAG answer service (LLM drives search/read_file via tool
    // calls). Present only when [llm] is configured; a `None` here is the
    // source of the 501 the `/v1/answer` route returns. Round/token knobs
    // come from [llm], timeout/retry from AnswerParams defaults.
    let answer_service: Option<Arc<AnswerService>> = match (&cfg.llm, &llm) {
        (Some(llm_cfg), Some(llm)) => {
            let params = AnswerParams {
                max_output_tokens: llm_cfg.answer_max_output_tokens,
                max_tool_rounds: llm_cfg.answer_max_tool_rounds,
                ..Default::default()
            };
            let tools = Arc::new(veda_core::service::answer::LiveTools::new(
                search_service.clone(),
                fs_service.clone(),
            ));
            Some(Arc::new(AnswerService::new(tools, llm.clone(), params)))
        }
        _ => None,
    };
    let answer_concurrency = cfg.llm.as_ref().map(|c| c.answer_concurrency).unwrap_or(2);

    // Shared WeCom bot table (owner: veda-tunnel). Small dedicated pool on
    // the same MySQL — see veda_server::tunnel_bots for the topology.
    let tunnel_bots =
        Arc::new(tunnel_bots::TunnelBotStore::connect(&cfg.mysql.database_url).await?);

    let app_state = Arc::new(AppState {
        fs_service,
        search_service,
        collection_service,
        vector_service,
        workspace_service,
        auth_store: mysql.clone(),
        meta_store: mysql.clone(),
        vector_store: milvus.clone(),
        reconciler,
        vector_workspace_store: milvus.clone(),
        vector_embedding,
        embedding_dim: cfg.embedding.dimension,
        sql_engine,
        metrics: metrics.clone(),
        metrics_token: cfg.metrics_token.clone(),
        admin_token: cfg.admin_token.clone(),
        summary_enabled: cfg.llm.is_some(),
        answer_service,
        answer_concurrency,
        tunnel_bots,
        draining: std::sync::atomic::AtomicBool::new(false),
    });

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

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
            embedding_bg.clone(),
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

    // Outbox depth sampler: emits veda_outbox_depth{status} every 30s. Pairs
    // with veda_outbox_dead_total (emitted at the two death sites in
    // claim()/fail()) so ops can alert on dead-letter backlog
    // (status="dead") and queue growth (status="pending"). Always emits the
    // three actionable statuses so a status that drops back to zero (e.g.
    // dead-letter drained) reports 0 instead of holding its last gauge value.
    let outbox_metrics = mysql.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            match outbox_metrics.outbox_status_counts().await {
                Ok(counts) => {
                    let m: std::collections::HashMap<String, i64> = counts.into_iter().collect();
                    for status in ["pending", "processing", "dead"] {
                        let n = m.get(status).copied().unwrap_or(0);
                        ::metrics::gauge!("veda_outbox_depth", "status" => status).set(n as f64);
                    }
                }
                Err(e) => tracing::warn!(err = %e, "outbox depth sample failed"),
            }
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

    // OTLP metrics exporter: periodically push Prometheus metrics to the company
    // Monitor Collector. Off by default (gray rollout via [otlp] enabled). Never
    // affects the main service — errors only warn.
    let otlp_handle = if cfg.otlp.enabled {
        match obs::otlp::OtlpExporter::from_config(&cfg.otlp) {
            Some(exporter) => {
                let metrics_handle = metrics.clone();
                let rx = shutdown_rx.clone();
                let interval = cfg.otlp.interval_secs;
                info!(
                    interval_secs = interval,
                    endpoint = %cfg.otlp.endpoint,
                    "OTLP metrics exporter enabled"
                );
                Some(tokio::spawn(async move {
                    exporter.run(metrics_handle, rx, interval).await;
                }))
            }
            None => None, // from_config already warned about why it's disabled
        }
    } else {
        info!("OTLP metrics exporter disabled");
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

    let drain_secs = cfg.drain_secs;
    let drain_state = app_state.clone();
    let app = routes::build_router(app_state)
        // Innermost: turn a handler panic (e.g. inside DataFusion) into a 500
        // instead of a reset connection, so `track_http` still records it.
        .layer(CatchPanicLayer::new())
        .layer(axum::middleware::from_fn(obs::track_http))
        .layer(TraceLayer::new_for_http())
        .layer(cors);

    // Socket activation: when systemd (or systemfd for local testing) passes
    // a listener via LISTEN_FDS, inherit it instead of binding. The socket
    // stays open in systemd across restarts, so a deploy never refuses
    // connections — they queue in the kernel backlog until the new process
    // accepts. `cfg.listen` is ignored in that case (the .socket unit owns
    // the address). Falls back to a plain bind for dev and non-systemd runs.
    let listener = match listenfd::ListenFd::from_env().take_tcp_listener(0)? {
        Some(inherited) => {
            inherited.set_nonblocking(true)?;
            let addr = inherited.local_addr()?;
            info!(%addr, "server listening on inherited socket (socket activation)");
            TcpListener::from_std(inherited)?
        }
        None => {
            let l = TcpListener::bind(&cfg.listen).await?;
            info!(addr = %cfg.listen, "server listening");
            l
        }
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            // systemd stop sends SIGTERM — ctrl_c() alone (SIGINT) never
            // fires under it, so production deploys would hard-kill the
            // worker mid-batch without ever reaching this path.
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            info!("shutdown signal received");
            // Stop background tasks (worker/retention/OTLP) right away so the
            // worker doesn't claim a fresh batch during the drain window and
            // stretch total stop time past drain_secs + current batch.
            let _ = shutdown_tx.send(true);
            if drain_secs > 0 {
                // Drain window: keep serving while /v1/ready reports 503 so
                // the LB health check pulls this node before the listener
                // closes. A second signal cuts the wait short.
                drain_state
                    .draining
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                info!(drain_secs, "draining: /v1/ready now 503, still serving");
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(drain_secs)) => {}
                    _ = tokio::signal::ctrl_c() => info!("second signal — drain wait skipped"),
                    _ = term.recv() => info!("second signal — drain wait skipped"),
                }
            }
        })
        .await?;

    if let Some(handle) = worker_handle {
        let _ = handle.await;
    }
    if let Some(handle) = retention_handle {
        let _ = handle.await;
    }
    if let Some(handle) = otlp_handle {
        let _ = handle.await;
    }

    Ok(())
}

/// What the CLI resolved to. Minimal parsing without pulling clap into the
/// binary: one positional config path, plus `--help` and `--version`.
#[derive(Debug, PartialEq)]
enum Cli {
    Run { config_path: String },
    Help,
    Version,
}

/// Pure so the contract is unit testable — an unknown flag MUST stay a hard
/// error (a typo'd flag silently taken as the config path would start the
/// server against the wrong backends).
fn parse_args(args: impl IntoIterator<Item = String>) -> anyhow::Result<Cli> {
    let mut config_path = "config/server.toml".to_string();
    for arg in args {
        match arg.as_str() {
            "--help" | "-h" => return Ok(Cli::Help),
            "--version" | "-V" => return Ok(Cli::Version),
            other if !other.starts_with("--") => config_path = other.to_string(),
            other => anyhow::bail!("unknown flag: {other}"),
        }
    }
    Ok(Cli::Run { config_path })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> anyhow::Result<Cli> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn version_flag_is_recognized() {
        assert_eq!(parse(&["--version"]).unwrap(), Cli::Version);
        assert_eq!(parse(&["-V"]).unwrap(), Cli::Version);
    }

    #[test]
    fn unknown_flag_is_still_an_error() {
        let err = parse(&["--versionn"]).unwrap_err();
        assert!(err.to_string().contains("unknown flag"), "{err}");
    }

    #[test]
    fn positional_is_the_config_path() {
        assert_eq!(
            parse(&["/etc/veda/server.toml"]).unwrap(),
            Cli::Run { config_path: "/etc/veda/server.toml".to_string() }
        );
        assert_eq!(
            parse(&[]).unwrap(),
            Cli::Run { config_path: "config/server.toml".to_string() }
        );
        assert_eq!(parse(&["--help"]).unwrap(), Cli::Help);
    }
}

