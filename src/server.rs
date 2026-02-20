use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration as StdDuration, Instant},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Extension, Query, Request, State},
    http::{HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use rmcp::transport::{
    StreamableHttpServerConfig,
    streamable_http_server::{session::local::LocalSessionManager, tower::StreamableHttpService},
};
use serde::Deserialize;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{Instrument, info, warn};

use crate::{
    config::Config, loki::client::LokiClient, mcp::LokiMcpServer, metrics::MetricsRegistry,
    recent_actions::RecentActionsStore, time::parse_std_duration,
};

const READINESS_CACHE_TTL: StdDuration = StdDuration::from_secs(3);
static REQUEST_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct RequestId(String);

#[derive(Clone)]
struct CachedReadiness {
    observed_at: Instant,
    status: StatusCode,
    body: serde_json::Value,
}

#[derive(Clone)]
struct AppState {
    metrics: MetricsRegistry,
    loki_client: LokiClient,
    readiness_cache: Arc<RwLock<Option<CachedReadiness>>>,
    recent_actions: Option<RecentActionsStore>,
}

pub async fn run(config: Config) -> Result<()> {
    init_tracing(&config.server.log_level);
    let recent_actions = build_recent_actions_store(&config)?;

    let state = AppState {
        metrics: MetricsRegistry::new(&config.metrics.prefix)?,
        loki_client: LokiClient::new(&config.loki)?,
        readiness_cache: Arc::new(RwLock::new(None)),
        recent_actions: recent_actions.clone(),
    };

    let mcp_server = LokiMcpServer::new(config.clone(), state.metrics.clone(), recent_actions)?;
    let mcp_service: StreamableHttpService<LokiMcpServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(mcp_server.clone()),
            Default::default(),
            StreamableHttpServerConfig {
                stateful_mode: true,
                sse_keep_alive: None,
                ..Default::default()
            },
        );

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .route("/debug/recent-actions", get(recent_actions_endpoint))
        .nest_service("/mcp", mcp_service)
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(
            state,
            request_context_middleware,
        ));

    let address: SocketAddr = config
        .server
        .listen
        .parse()
        .with_context(|| format!("invalid listen address: {}", config.server.listen))?;

    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("failed to bind to {address}"))?;

    info!(%address, "loki-mcp server started");

    axum::serve(listener, app)
        .await
        .context("server exited unexpectedly")
}

fn init_tracing(log_level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_new(log_level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_level(true)
        .try_init();
}

async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    let _ = state;
    (StatusCode::OK, "ok")
}

async fn readyz(
    State(state): State<AppState>,
    request_id: Option<Extension<RequestId>>,
) -> impl IntoResponse {
    if let Some(cached) = read_cached_readiness(&state).await {
        state.metrics.inc_readiness_cache_hit();
        return (cached.status, Json(cached.body)).into_response();
    }

    state.metrics.inc_readiness_cache_miss();
    let resolved = match state.loki_client.check_health().await {
        Ok(health) if health.healthy => CachedReadiness {
            observed_at: Instant::now(),
            status: StatusCode::OK,
            body: json!({"status": "ready"}),
        },
        Ok(health) => CachedReadiness {
            observed_at: Instant::now(),
            status: StatusCode::SERVICE_UNAVAILABLE,
            body: json!({
                "status": "not_ready",
                "message": health.message,
            }),
        },
        Err(error) => {
            let request_id = request_id
                .map(|Extension(value)| value.0)
                .unwrap_or_else(|| "unknown".to_string());
            warn!(request_id = %request_id, error = %error, "readiness check failed");
            CachedReadiness {
                observed_at: Instant::now(),
                status: StatusCode::SERVICE_UNAVAILABLE,
                body: json!({
                    "status": "not_ready",
                    "message": error.to_string(),
                }),
            }
        }
    };

    write_cached_readiness(&state, resolved.clone()).await;
    (resolved.status, Json(resolved.body)).into_response()
}

async fn metrics(
    State(state): State<AppState>,
    request_id: Option<Extension<RequestId>>,
) -> impl IntoResponse {
    match state.metrics.render() {
        Ok(body) => (StatusCode::OK, body).into_response(),
        Err(error) => {
            let request_id = request_id
                .map(|Extension(value)| value.0)
                .unwrap_or_else(|| "unknown".to_string());
            warn!(
                request_id = %request_id,
                error = %error,
                "failed to render metrics"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "failed to render metrics"})),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
struct RecentActionsQuery {
    limit: Option<usize>,
}

async fn recent_actions_endpoint(
    State(state): State<AppState>,
    Query(query): Query<RecentActionsQuery>,
) -> impl IntoResponse {
    let Some(recent_actions) = state.recent_actions.as_ref() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "recent actions tracking is disabled"})),
        )
            .into_response();
    };

    let limit = query.limit.unwrap_or(100).clamp(1, 1000);
    let actions = recent_actions.list(limit).await;
    (
        StatusCode::OK,
        Json(json!({
            "count": actions.len(),
            "actions": actions,
        })),
    )
        .into_response()
}

async fn request_context_middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    state.metrics.inc_http_requests();

    let request_id = next_request_id();
    request
        .extensions_mut()
        .insert(RequestId(request_id.clone()));
    if let Ok(header_value) = HeaderValue::from_str(&request_id) {
        request.headers_mut().insert("x-request-id", header_value);
    }

    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let span = tracing::info_span!("http_request", request_id = %request_id, method = %method, path = %path);
    let mut response = next.run(request).instrument(span).await;

    if let Ok(header_value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", header_value);
    }

    response
}

fn next_request_id() -> String {
    let id = REQUEST_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("req-{id}")
}

fn build_recent_actions_store(config: &Config) -> Result<Option<RecentActionsStore>> {
    if !config.recent_actions.enabled {
        return Ok(None);
    }

    let ttl = parse_std_duration(&config.recent_actions.ttl)
        .with_context(|| format!("invalid recent_actions.ttl: {}", config.recent_actions.ttl))?;
    Ok(Some(RecentActionsStore::new(
        config.recent_actions.max_entries,
        ttl,
        config.recent_actions.store_query_text,
        config.recent_actions.store_error_text,
    )))
}

async fn read_cached_readiness(state: &AppState) -> Option<CachedReadiness> {
    let cache = state.readiness_cache.read().await;
    let cached = cache.as_ref()?.clone();
    if cached.observed_at.elapsed() > READINESS_CACHE_TTL {
        return None;
    }

    Some(cached)
}

async fn write_cached_readiness(state: &AppState, readiness: CachedReadiness) {
    let mut cache = state.readiness_cache.write().await;
    *cache = Some(readiness);
}
