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
    http::{HeaderName, HeaderValue, Method, StatusCode},
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
const CORS_ALLOW_METHODS: &str = "GET, POST, OPTIONS";
const CORS_EXPOSE_HEADERS: &str = "x-request-id, mcp-session-id";
const CORS_MAX_AGE_SECONDS: &str = "600";
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
    server_auth: Option<ServerAuth>,
    server_cors: Option<ServerCors>,
}

#[derive(Clone)]
struct ServerAuth {
    header_name: HeaderName,
    token: String,
}

impl ServerAuth {
    fn from_config(config: &Config) -> Result<Option<Self>> {
        let Some(token) = config
            .server
            .auth_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(None);
        };

        let header_name = config
            .server
            .auth_header
            .trim()
            .parse::<HeaderName>()
            .with_context(|| {
                format!(
                    "server.auth_header must be a valid HTTP header name when server.auth_token is set, got {}",
                    config.server.auth_header
                )
            })?;

        Ok(Some(Self {
            header_name,
            token: token.to_string(),
        }))
    }
}

#[derive(Clone)]
struct ServerCors {
    allow_any_origin: bool,
    allowed_origins: Vec<String>,
}

#[derive(Clone)]
struct CorsResponseHeaders {
    allow_origin: HeaderValue,
    allow_headers: Option<HeaderValue>,
}

impl ServerCors {
    fn from_config(config: &Config) -> Option<Self> {
        if config.server.cors_allowed_origins.is_empty() {
            return None;
        }

        let allow_any_origin = config
            .server
            .cors_allowed_origins
            .iter()
            .any(|origin| origin == "*");
        let allowed_origins = if allow_any_origin {
            Vec::new()
        } else {
            config.server.cors_allowed_origins.clone()
        };

        Some(Self {
            allow_any_origin,
            allowed_origins,
        })
    }

    fn headers_for_request(&self, request: &Request) -> Option<CorsResponseHeaders> {
        let origin = request.headers().get("origin")?;
        let allow_origin = if self.allow_any_origin {
            HeaderValue::from_static("*")
        } else {
            let origin_text = origin.to_str().ok()?;
            if !self
                .allowed_origins
                .iter()
                .any(|allowed_origin| allowed_origin == origin_text)
            {
                return None;
            }
            origin.clone()
        };

        let allow_headers = request
            .headers()
            .get("access-control-request-headers")
            .cloned();

        Some(CorsResponseHeaders {
            allow_origin,
            allow_headers,
        })
    }
}

pub async fn run(config: Config) -> Result<()> {
    init_tracing(&config.server.log_level);
    let recent_actions = build_recent_actions_store(&config)?;
    let server_auth = ServerAuth::from_config(&config)?;
    let server_cors = ServerCors::from_config(&config);

    let state = AppState {
        metrics: MetricsRegistry::new(&config.metrics.prefix)?,
        loki_client: LokiClient::new(&config.loki)?,
        readiness_cache: Arc::new(RwLock::new(None)),
        recent_actions: recent_actions.clone(),
        server_auth,
        server_cors,
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
    if let Ok(header_value) = HeaderValue::from_str(request_id.as_str()) {
        request.headers_mut().insert("x-request-id", header_value);
    }

    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let cors_headers = state
        .server_cors
        .as_ref()
        .and_then(|server_cors| server_cors.headers_for_request(&request));
    let span = tracing::info_span!("http_request", request_id = %request_id, method = %method, path = %path);
    async move {
        if path_allows_cors(&path) && method == Method::OPTIONS {
            return cors_preflight_response(cors_headers.as_ref(), &request_id);
        }

        if path_requires_auth(&path)
            && !request_has_valid_auth(&request, state.server_auth.as_ref())
        {
            warn!("unauthorized request rejected");
            let mut response = unauthorized_response(&request_id);
            if path_allows_cors(&path) {
                insert_cors_headers(&mut response, cors_headers.as_ref());
            }
            return response;
        }

        let mut response = next.run(request).await;

        insert_request_id_header(&mut response, &request_id);
        if path_allows_cors(&path) {
            insert_cors_headers(&mut response, cors_headers.as_ref());
        }

        response
    }
    .instrument(span)
    .await
}

fn insert_request_id_header(response: &mut Response, request_id: &str) {
    if let Ok(header_value) = HeaderValue::from_str(request_id) {
        response.headers_mut().insert("x-request-id", header_value);
    }
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

fn path_requires_auth(path: &str) -> bool {
    path == "/mcp" || path.starts_with("/mcp/") || path == "/debug" || path.starts_with("/debug/")
}

fn path_allows_cors(path: &str) -> bool {
    path_requires_auth(path)
}

fn request_has_valid_auth(request: &Request, server_auth: Option<&ServerAuth>) -> bool {
    let Some(server_auth) = server_auth else {
        return true;
    };

    let Some(header_value) = request.headers().get(&server_auth.header_name) else {
        return false;
    };

    let Ok(provided_token) = header_value.to_str() else {
        return false;
    };

    constant_time_eq(provided_token.as_bytes(), server_auth.token.as_bytes())
}

fn unauthorized_response(request_id: &str) -> Response {
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "unauthorized"})),
    )
        .into_response();
    insert_request_id_header(&mut response, request_id);
    response
}

fn cors_preflight_response(
    cors_headers: Option<&CorsResponseHeaders>,
    request_id: &str,
) -> Response {
    let mut response = if cors_headers.is_some() {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "cors origin is not allowed"})),
        )
            .into_response()
    };
    insert_request_id_header(&mut response, request_id);
    insert_cors_headers(&mut response, cors_headers);
    response
}

fn insert_cors_headers(response: &mut Response, cors_headers: Option<&CorsResponseHeaders>) {
    let Some(cors_headers) = cors_headers else {
        return;
    };

    response.headers_mut().insert(
        HeaderName::from_static("access-control-allow-origin"),
        cors_headers.allow_origin.clone(),
    );
    response.headers_mut().insert(
        HeaderName::from_static("access-control-allow-methods"),
        HeaderValue::from_static(CORS_ALLOW_METHODS),
    );
    response.headers_mut().insert(
        HeaderName::from_static("access-control-expose-headers"),
        HeaderValue::from_static(CORS_EXPOSE_HEADERS),
    );
    response.headers_mut().insert(
        HeaderName::from_static("access-control-max-age"),
        HeaderValue::from_static(CORS_MAX_AGE_SECONDS),
    );
    if let Some(allow_headers) = cors_headers.allow_headers.as_ref() {
        response.headers_mut().insert(
            HeaderName::from_static("access-control-allow-headers"),
            allow_headers.clone(),
        );
    }
    response.headers_mut().append(
        HeaderName::from_static("vary"),
        HeaderValue::from_static("origin"),
    );
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let max_len = left.len().max(right.len());
    let mut diff = left.len() ^ right.len();

    for index in 0..max_len {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        diff |= usize::from(left_byte ^ right_byte);
    }

    diff == 0
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{HeaderName, HeaderValue, Method, Request, StatusCode},
    };

    use crate::{
        config::Config,
        server::{CorsResponseHeaders, ServerAuth, ServerCors},
    };

    #[test]
    fn server_auth_is_disabled_without_token() {
        let config = Config::default();

        let server_auth = ServerAuth::from_config(&config).expect("server auth should build");

        assert!(server_auth.is_none());
    }

    #[test]
    fn path_auth_scope_only_covers_mcp_and_debug() {
        assert!(crate::server::path_requires_auth("/mcp"));
        assert!(crate::server::path_requires_auth("/mcp/session"));
        assert!(crate::server::path_requires_auth("/debug"));
        assert!(crate::server::path_requires_auth("/debug/recent-actions"));
        assert!(!crate::server::path_requires_auth("/healthz"));
        assert!(!crate::server::path_requires_auth("/readyz"));
        assert!(!crate::server::path_requires_auth("/metrics"));
        assert!(!crate::server::path_requires_auth("/debugger"));
    }

    #[test]
    fn request_auth_accepts_configured_header_value() {
        let server_auth = ServerAuth {
            header_name: HeaderName::from_static("x-loki-mcp-token"),
            token: "secret".to_string(),
        };
        let request = Request::builder()
            .header("x-loki-mcp-token", "secret")
            .body(Body::empty())
            .expect("request should build");

        assert!(crate::server::request_has_valid_auth(
            &request,
            Some(&server_auth)
        ));
    }

    #[test]
    fn request_auth_rejects_missing_or_wrong_header_value() {
        let server_auth = ServerAuth {
            header_name: HeaderName::from_static("x-loki-mcp-token"),
            token: "secret".to_string(),
        };
        let missing = Request::builder()
            .body(Body::empty())
            .expect("request should build");
        let wrong = Request::builder()
            .header("x-loki-mcp-token", "other")
            .body(Body::empty())
            .expect("request should build");

        assert!(!crate::server::request_has_valid_auth(
            &missing,
            Some(&server_auth)
        ));
        assert!(!crate::server::request_has_valid_auth(
            &wrong,
            Some(&server_auth)
        ));
    }

    #[test]
    fn token_comparison_requires_exact_match() {
        assert!(crate::server::constant_time_eq(b"secret", b"secret"));
        assert!(!crate::server::constant_time_eq(b"secret", b"Secret"));
        assert!(!crate::server::constant_time_eq(b"secret", b"secret "));
        assert!(!crate::server::constant_time_eq(b"secret", b"secret2"));
    }

    #[test]
    fn server_cors_is_disabled_without_allowed_origins() {
        let config = Config::default();

        let server_cors = ServerCors::from_config(&config);

        assert!(server_cors.is_none());
    }

    #[test]
    fn server_cors_allows_exact_configured_origin() {
        let mut config = Config::default();
        config.server.cors_allowed_origins = vec!["http://localhost:6274".to_string()];
        let server_cors = ServerCors::from_config(&config).expect("cors should be enabled");
        let request = Request::builder()
            .header("origin", "http://localhost:6274")
            .body(Body::empty())
            .expect("request should build");

        let headers = server_cors
            .headers_for_request(&request)
            .expect("origin should be allowed");

        assert_eq!(
            headers.allow_origin,
            HeaderValue::from_static("http://localhost:6274")
        );
    }

    #[test]
    fn server_cors_rejects_unconfigured_origin() {
        let mut config = Config::default();
        config.server.cors_allowed_origins = vec!["http://localhost:6274".to_string()];
        let server_cors = ServerCors::from_config(&config).expect("cors should be enabled");
        let request = Request::builder()
            .header("origin", "http://localhost:9999")
            .body(Body::empty())
            .expect("request should build");

        assert!(server_cors.headers_for_request(&request).is_none());
    }

    #[test]
    fn server_cors_wildcard_allows_any_origin() {
        let mut config = Config::default();
        config.server.cors_allowed_origins = vec!["*".to_string()];
        let server_cors = ServerCors::from_config(&config).expect("cors should be enabled");
        let request = Request::builder()
            .header("origin", "http://localhost:6274")
            .body(Body::empty())
            .expect("request should build");

        let headers = server_cors
            .headers_for_request(&request)
            .expect("origin should be allowed");

        assert_eq!(headers.allow_origin, HeaderValue::from_static("*"));
    }

    #[test]
    fn cors_preflight_response_includes_requested_headers() {
        let cors_headers = CorsResponseHeaders {
            allow_origin: HeaderValue::from_static("http://localhost:6274"),
            allow_headers: Some(HeaderValue::from_static(
                "content-type,x-loki-mcp-token,mcp-session-id",
            )),
        };

        let response = crate::server::cors_preflight_response(Some(&cors_headers), "req-test");

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .expect("allow origin header"),
            "http://localhost:6274"
        );
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-headers")
                .expect("allow headers header"),
            "content-type,x-loki-mcp-token,mcp-session-id"
        );
        assert_eq!(
            response.headers().get("x-request-id").expect("request id"),
            "req-test"
        );
    }

    #[test]
    fn cors_preflight_response_rejects_disallowed_origins() {
        let response = crate::server::cors_preflight_response(None, "req-test");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
        assert_eq!(
            response.headers().get("x-request-id").expect("request id"),
            "req-test"
        );
    }

    #[test]
    fn cors_applies_to_same_paths_as_server_auth() {
        assert!(crate::server::path_allows_cors("/mcp"));
        assert!(crate::server::path_allows_cors("/debug/recent-actions"));
        assert!(!crate::server::path_allows_cors("/metrics"));
    }

    #[test]
    fn cors_preflight_path_uses_options_method() {
        let request = Request::builder()
            .method(Method::OPTIONS)
            .uri("/mcp")
            .header("origin", "http://localhost:6274")
            .header("access-control-request-method", "POST")
            .body(Body::empty())
            .expect("request should build");

        assert_eq!(request.method(), Method::OPTIONS);
        assert!(crate::server::path_allows_cors(request.uri().path()));
    }
}
