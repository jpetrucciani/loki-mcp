use std::{collections::BTreeMap, fmt::Display, net::SocketAddr, path::PathBuf, str::FromStr};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize, de::Deserializer};

use crate::time::parse_std_duration;

#[derive(Debug, Clone, Parser)]
#[command(author, version, about)]
pub struct Cli {
    #[arg(long, env = "LOKI_MCP_CONFIG", default_value = "config.toml")]
    pub config: PathBuf,

    #[arg(long)]
    pub listen: Option<String>,
    #[arg(long)]
    pub timezone: Option<String>,
    #[arg(long)]
    pub log_level: Option<String>,
    #[arg(long)]
    pub identity_header: Option<String>,

    #[arg(long)]
    pub loki_url: Option<String>,
    #[arg(long)]
    pub loki_tenant_id: Option<String>,
    #[arg(long)]
    pub loki_auth_type: Option<String>,
    #[arg(long)]
    pub loki_username: Option<String>,
    #[arg(long)]
    pub loki_password: Option<String>,
    #[arg(long)]
    pub loki_token: Option<String>,
    #[arg(long)]
    pub loki_ca_cert: Option<String>,
    #[arg(long)]
    pub loki_timeout: Option<String>,

    #[arg(long)]
    pub cache_enabled: Option<bool>,
    #[arg(long)]
    pub cache_ttl: Option<String>,
    #[arg(long)]
    pub cache_skip_if_range_shorter_than: Option<String>,
    #[arg(long)]
    pub cache_max_entries: Option<u64>,

    #[arg(long)]
    pub guardrails_max_bytes_scanned: Option<String>,
    #[arg(long)]
    pub guardrails_max_streams: Option<u64>,
    #[arg(long)]
    pub guardrails_skip_stats_if_streams_below: Option<u64>,
    #[arg(long)]
    pub guardrails_skip_stats_if_range_shorter_than: Option<String>,

    #[arg(long)]
    pub rate_limit_enabled: Option<bool>,
    #[arg(long)]
    pub rate_limit_rps: Option<f64>,
    #[arg(long)]
    pub rate_limit_burst: Option<u32>,

    #[arg(long)]
    pub metrics_prefix: Option<String>,

    #[arg(long)]
    pub recent_actions_enabled: Option<bool>,
    #[arg(long)]
    pub recent_actions_max_entries: Option<u64>,
    #[arg(long)]
    pub recent_actions_ttl: Option<String>,
    #[arg(long)]
    pub recent_actions_store_query_text: Option<bool>,
    #[arg(long)]
    pub recent_actions_store_error_text: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    pub server: ServerConfig,
    pub loki: LokiConfig,
    pub cache: CacheConfig,
    pub guardrails: GuardrailsConfig,
    pub rate_limit: RateLimitConfig,
    pub metrics: MetricsConfig,
    pub recent_actions: RecentActionsConfig,
    #[serde(default)]
    pub labels: Vec<SchemaField>,
    #[serde(default)]
    pub structured_metadata: Vec<SchemaField>,
    #[serde(default)]
    pub saved_queries: Vec<SavedQuery>,
}

impl Config {
    fn normalize(&mut self) {
        self.server.listen = self.server.listen.trim().to_string();
        self.server.timezone = self.server.timezone.trim().to_string();
        self.server.log_level = self.server.log_level.trim().to_string();
        normalize_optional_string(&mut self.server.identity_header);

        self.loki.url = self.loki.url.trim().to_string();
        self.loki.auth_type = self.loki.auth_type.trim().to_ascii_lowercase();
        self.loki.timeout = self.loki.timeout.trim().to_string();
        normalize_optional_string(&mut self.loki.tenant_id);
        normalize_optional_string(&mut self.loki.username);
        normalize_optional_string(&mut self.loki.password);
        normalize_optional_string(&mut self.loki.token);
        normalize_optional_string(&mut self.loki.ca_cert);

        self.cache.ttl = self.cache.ttl.trim().to_string();
        self.cache.skip_if_range_shorter_than =
            self.cache.skip_if_range_shorter_than.trim().to_string();

        self.guardrails.max_bytes_scanned = self.guardrails.max_bytes_scanned.trim().to_string();
        self.guardrails.skip_stats_if_range_shorter_than = self
            .guardrails
            .skip_stats_if_range_shorter_than
            .trim()
            .to_string();

        self.metrics.prefix = self.metrics.prefix.trim().to_string();
        self.recent_actions.ttl = self.recent_actions.ttl.trim().to_string();
    }

    fn validate(&self) -> Result<()> {
        ensure_non_empty("server.listen", &self.server.listen)?;
        self.server.listen.parse::<SocketAddr>().with_context(|| {
            format!(
                "server.listen must be host:port, got {}",
                self.server.listen
            )
        })?;

        ensure_non_empty("server.timezone", &self.server.timezone)?;
        self.server
            .timezone
            .parse::<chrono_tz::Tz>()
            .with_context(|| format!("invalid server.timezone: {}", self.server.timezone))?;

        ensure_non_empty("server.log_level", &self.server.log_level)?;

        ensure_non_empty("loki.url", &self.loki.url)?;
        reqwest::Url::parse(&self.loki.url)
            .with_context(|| format!("invalid loki.url: {}", self.loki.url))?;

        match self.loki.auth_type.as_str() {
            "none" => {}
            "basic" => {
                if self.loki.username.is_none() {
                    bail!("loki.username is required when loki.auth_type=basic");
                }
                if self.loki.password.is_none() {
                    bail!("loki.password is required when loki.auth_type=basic");
                }
            }
            "bearer" => {
                if self.loki.token.is_none() {
                    bail!("loki.token is required when loki.auth_type=bearer");
                }
            }
            other => {
                bail!("unsupported loki.auth_type: {other}. expected one of none/basic/bearer");
            }
        }

        parse_std_duration(&self.loki.timeout)
            .with_context(|| format!("invalid loki.timeout: {}", self.loki.timeout))?;
        parse_std_duration(&self.cache.ttl)
            .with_context(|| format!("invalid cache.ttl: {}", self.cache.ttl))?;
        parse_std_duration(&self.cache.skip_if_range_shorter_than).with_context(|| {
            format!(
                "invalid cache.skip_if_range_shorter_than: {}",
                self.cache.skip_if_range_shorter_than
            )
        })?;
        parse_std_duration(&self.guardrails.skip_stats_if_range_shorter_than).with_context(
            || {
                format!(
                    "invalid guardrails.skip_stats_if_range_shorter_than: {}",
                    self.guardrails.skip_stats_if_range_shorter_than
                )
            },
        )?;

        parse_byte_size(&self.guardrails.max_bytes_scanned).with_context(|| {
            format!(
                "invalid guardrails.max_bytes_scanned: {}",
                self.guardrails.max_bytes_scanned
            )
        })?;

        if self.cache.max_entries == 0 {
            bail!("cache.max_entries must be greater than zero");
        }

        if self.rate_limit.enabled {
            if self.rate_limit.rps <= 0.0 {
                bail!("rate_limit.rps must be > 0 when rate limiting is enabled");
            }
            if self.rate_limit.burst == 0 {
                bail!("rate_limit.burst must be > 0 when rate limiting is enabled");
            }
        }

        ensure_non_empty("metrics.prefix", &self.metrics.prefix)?;
        parse_std_duration(&self.recent_actions.ttl)
            .with_context(|| format!("invalid recent_actions.ttl: {}", self.recent_actions.ttl))?;
        if self.recent_actions.max_entries == 0 {
            bail!("recent_actions.max_entries must be greater than zero");
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub listen: String,
    pub timezone: String,
    pub log_level: String,
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub identity_header: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".to_string(),
            timezone: "America/New_York".to_string(),
            log_level: "info".to_string(),
            identity_header: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LokiConfig {
    pub url: String,
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub tenant_id: Option<String>,
    pub auth_type: String,
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub username: Option<String>,
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub password: Option<String>,
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub token: Option<String>,
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub ca_cert: Option<String>,
    pub timeout: String,
}

impl Default for LokiConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:3100".to_string(),
            tenant_id: None,
            auth_type: "none".to_string(),
            username: None,
            password: None,
            token: None,
            ca_cert: None,
            timeout: "30s".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    pub enabled: bool,
    pub ttl: String,
    pub skip_if_range_shorter_than: String,
    pub max_entries: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl: "60s".to_string(),
            skip_if_range_shorter_than: "60s".to_string(),
            max_entries: 1000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardrailsConfig {
    pub max_bytes_scanned: String,
    pub max_streams: u64,
    pub skip_stats_if_streams_below: u64,
    pub skip_stats_if_range_shorter_than: String,
}

impl Default for GuardrailsConfig {
    fn default() -> Self {
        Self {
            max_bytes_scanned: "500MB".to_string(),
            max_streams: 5000,
            skip_stats_if_streams_below: 50,
            skip_stats_if_range_shorter_than: "15m".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitConfig {
    pub enabled: bool,
    pub rps: f64,
    pub burst: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            rps: 10.0,
            burst: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsConfig {
    pub prefix: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            prefix: "loki_mcp".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentActionsConfig {
    pub enabled: bool,
    pub max_entries: u64,
    pub ttl: String,
    pub store_query_text: bool,
    pub store_error_text: bool,
}

impl Default for RecentActionsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_entries: 500,
            ttl: "30m".to_string(),
            store_query_text: false,
            store_error_text: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaField {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub common_values: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedQuery {
    pub name: String,
    pub description: String,
    pub query: String,
    pub range: String,
}

#[derive(Debug, Clone, Serialize, Default)]
struct ConfigOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<ServerOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    loki: Option<LokiOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache: Option<CacheOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    guardrails: Option<GuardrailsOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rate_limit: Option<RateLimitOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics: Option<MetricsOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recent_actions: Option<RecentActionsOverrides>,
}

impl ConfigOverrides {
    fn from_cli(cli: &Cli) -> Self {
        let server = ServerOverrides {
            listen: normalized(cli.listen.clone()),
            timezone: normalized(cli.timezone.clone()),
            log_level: normalized(cli.log_level.clone()),
            identity_header: normalized(cli.identity_header.clone()),
        };

        let loki = LokiOverrides {
            url: normalized(cli.loki_url.clone()),
            tenant_id: normalized(cli.loki_tenant_id.clone()),
            auth_type: normalized(cli.loki_auth_type.clone()),
            username: normalized(cli.loki_username.clone()),
            password: normalized(cli.loki_password.clone()),
            token: normalized(cli.loki_token.clone()),
            ca_cert: normalized(cli.loki_ca_cert.clone()),
            timeout: normalized(cli.loki_timeout.clone()),
        };

        let cache = CacheOverrides {
            enabled: cli.cache_enabled,
            ttl: normalized(cli.cache_ttl.clone()),
            skip_if_range_shorter_than: normalized(cli.cache_skip_if_range_shorter_than.clone()),
            max_entries: cli.cache_max_entries,
        };

        let guardrails = GuardrailsOverrides {
            max_bytes_scanned: normalized(cli.guardrails_max_bytes_scanned.clone()),
            max_streams: cli.guardrails_max_streams,
            skip_stats_if_streams_below: cli.guardrails_skip_stats_if_streams_below,
            skip_stats_if_range_shorter_than: normalized(
                cli.guardrails_skip_stats_if_range_shorter_than.clone(),
            ),
        };

        let rate_limit = RateLimitOverrides {
            enabled: cli.rate_limit_enabled,
            rps: cli.rate_limit_rps,
            burst: cli.rate_limit_burst,
        };

        let metrics = MetricsOverrides {
            prefix: normalized(cli.metrics_prefix.clone()),
        };

        let recent_actions = RecentActionsOverrides {
            enabled: cli.recent_actions_enabled,
            max_entries: cli.recent_actions_max_entries,
            ttl: normalized(cli.recent_actions_ttl.clone()),
            store_query_text: cli.recent_actions_store_query_text,
            store_error_text: cli.recent_actions_store_error_text,
        };

        Self {
            server: option_if_not_empty(server),
            loki: option_if_not_empty(loki),
            cache: option_if_not_empty(cache),
            guardrails: option_if_not_empty(guardrails),
            rate_limit: option_if_not_empty(rate_limit),
            metrics: option_if_not_empty(metrics),
            recent_actions: option_if_not_empty(recent_actions),
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
struct ServerOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    listen: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timezone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    log_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity_header: Option<String>,
}

impl IsEmpty for ServerOverrides {
    fn is_empty(&self) -> bool {
        self.listen.is_none()
            && self.timezone.is_none()
            && self.log_level.is_none()
            && self.identity_header.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Default)]
struct LokiOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tenant_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ca_cert: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<String>,
}

impl IsEmpty for LokiOverrides {
    fn is_empty(&self) -> bool {
        self.url.is_none()
            && self.tenant_id.is_none()
            && self.auth_type.is_none()
            && self.username.is_none()
            && self.password.is_none()
            && self.token.is_none()
            && self.ca_cert.is_none()
            && self.timeout.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Default)]
struct CacheOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skip_if_range_shorter_than: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_entries: Option<u64>,
}

impl IsEmpty for CacheOverrides {
    fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.ttl.is_none()
            && self.skip_if_range_shorter_than.is_none()
            && self.max_entries.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Default)]
struct GuardrailsOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_bytes_scanned: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_streams: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skip_stats_if_streams_below: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skip_stats_if_range_shorter_than: Option<String>,
}

impl IsEmpty for GuardrailsOverrides {
    fn is_empty(&self) -> bool {
        self.max_bytes_scanned.is_none()
            && self.max_streams.is_none()
            && self.skip_stats_if_streams_below.is_none()
            && self.skip_stats_if_range_shorter_than.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Default)]
struct RateLimitOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    burst: Option<u32>,
}

impl IsEmpty for RateLimitOverrides {
    fn is_empty(&self) -> bool {
        self.enabled.is_none() && self.rps.is_none() && self.burst.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Default)]
struct MetricsOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    prefix: Option<String>,
}

impl IsEmpty for MetricsOverrides {
    fn is_empty(&self) -> bool {
        self.prefix.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Default)]
struct RecentActionsOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_entries: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    store_query_text: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    store_error_text: Option<bool>,
}

impl IsEmpty for RecentActionsOverrides {
    fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.max_entries.is_none()
            && self.ttl.is_none()
            && self.store_query_text.is_none()
            && self.store_error_text.is_none()
    }
}

trait IsEmpty {
    fn is_empty(&self) -> bool;
}

fn option_if_not_empty<T: IsEmpty>(value: T) -> Option<T> {
    if value.is_empty() { None } else { Some(value) }
}

fn normalized(value: Option<String>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

fn normalize_optional_string(value: &mut Option<String>) {
    if let Some(inner) = value {
        let trimmed = inner.trim().to_string();
        if trimmed.is_empty() {
            *value = None;
        } else {
            *inner = trimmed;
        }
    }
}

fn empty_string_as_none<'de, D>(deserializer: D) -> std::result::Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(value.and_then(|raw| {
        let trimmed = raw.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }))
}

pub fn load(cli: &Cli) -> Result<Config> {
    let config_provider = if cli.config.exists() {
        Toml::file(&cli.config)
    } else {
        Toml::string("")
    };

    let flat_env_overrides = flat_env_overrides()?;
    let cli_overrides = ConfigOverrides::from_cli(cli);

    let mut config: Config = Figment::from(Serialized::defaults(Config::default()))
        .merge(config_provider)
        .merge(Env::prefixed("LOKI_MCP_").split("__"))
        .merge(Serialized::defaults(flat_env_overrides))
        .merge(Serialized::defaults(cli_overrides))
        .extract()
        .context("failed to load configuration")?;

    config.normalize();
    config.validate()?;

    Ok(config)
}

fn flat_env_overrides() -> Result<ConfigOverrides> {
    let vars = std::env::vars().collect::<BTreeMap<String, String>>();
    flat_env_overrides_from_map(&vars)
}

fn flat_env_overrides_from_map(vars: &BTreeMap<String, String>) -> Result<ConfigOverrides> {
    let server = ServerOverrides {
        listen: env_string(vars, "LOKI_MCP_LISTEN"),
        timezone: env_string(vars, "LOKI_MCP_TIMEZONE"),
        log_level: env_string(vars, "LOKI_MCP_LOG_LEVEL"),
        identity_header: env_string(vars, "LOKI_MCP_IDENTITY_HEADER"),
    };

    let loki = LokiOverrides {
        url: env_string(vars, "LOKI_MCP_LOKI_URL"),
        tenant_id: env_string(vars, "LOKI_MCP_LOKI_TENANT_ID"),
        auth_type: env_string(vars, "LOKI_MCP_LOKI_AUTH_TYPE"),
        username: env_string(vars, "LOKI_MCP_LOKI_USERNAME"),
        password: env_string(vars, "LOKI_MCP_LOKI_PASSWORD"),
        token: env_string(vars, "LOKI_MCP_LOKI_TOKEN"),
        ca_cert: env_string(vars, "LOKI_MCP_LOKI_CA_CERT"),
        timeout: env_string(vars, "LOKI_MCP_LOKI_TIMEOUT"),
    };

    let cache = CacheOverrides {
        enabled: env_parse(vars, "LOKI_MCP_CACHE_ENABLED")?,
        ttl: env_string(vars, "LOKI_MCP_CACHE_TTL"),
        skip_if_range_shorter_than: env_string(vars, "LOKI_MCP_CACHE_SKIP_IF_RANGE_SHORTER_THAN"),
        max_entries: env_parse(vars, "LOKI_MCP_CACHE_MAX_ENTRIES")?,
    };

    let guardrails = GuardrailsOverrides {
        max_bytes_scanned: env_string(vars, "LOKI_MCP_GUARDRAILS_MAX_BYTES_SCANNED"),
        max_streams: env_parse(vars, "LOKI_MCP_GUARDRAILS_MAX_STREAMS")?,
        skip_stats_if_streams_below: env_parse(
            vars,
            "LOKI_MCP_GUARDRAILS_SKIP_STATS_IF_STREAMS_BELOW",
        )?,
        skip_stats_if_range_shorter_than: env_string(
            vars,
            "LOKI_MCP_GUARDRAILS_SKIP_STATS_IF_RANGE_SHORTER_THAN",
        ),
    };

    let rate_limit = RateLimitOverrides {
        enabled: env_parse(vars, "LOKI_MCP_RATE_LIMIT_ENABLED")?,
        rps: env_parse(vars, "LOKI_MCP_RATE_LIMIT_RPS")?,
        burst: env_parse(vars, "LOKI_MCP_RATE_LIMIT_BURST")?,
    };

    let metrics = MetricsOverrides {
        prefix: env_string(vars, "LOKI_MCP_METRICS_PREFIX"),
    };

    let recent_actions = RecentActionsOverrides {
        enabled: env_parse(vars, "LOKI_MCP_RECENT_ACTIONS_ENABLED")?,
        max_entries: env_parse(vars, "LOKI_MCP_RECENT_ACTIONS_MAX_ENTRIES")?,
        ttl: env_string(vars, "LOKI_MCP_RECENT_ACTIONS_TTL"),
        store_query_text: env_parse(vars, "LOKI_MCP_RECENT_ACTIONS_STORE_QUERY_TEXT")?,
        store_error_text: env_parse(vars, "LOKI_MCP_RECENT_ACTIONS_STORE_ERROR_TEXT")?,
    };

    Ok(ConfigOverrides {
        server: option_if_not_empty(server),
        loki: option_if_not_empty(loki),
        cache: option_if_not_empty(cache),
        guardrails: option_if_not_empty(guardrails),
        rate_limit: option_if_not_empty(rate_limit),
        metrics: option_if_not_empty(metrics),
        recent_actions: option_if_not_empty(recent_actions),
    })
}

fn env_string(vars: &BTreeMap<String, String>, key: &str) -> Option<String> {
    vars.get(key).and_then(|value| {
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

fn env_parse<T>(vars: &BTreeMap<String, String>, key: &str) -> Result<Option<T>>
where
    T: FromStr,
    T::Err: Display,
{
    let Some(raw) = vars.get(key) else {
        return Ok(None);
    };

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let parsed = trimmed
        .parse::<T>()
        .map_err(|err| anyhow!("invalid value for {key}: {trimmed} ({err})"))?;

    Ok(Some(parsed))
}

fn ensure_non_empty(key: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{key} must not be empty");
    }

    Ok(())
}

fn parse_byte_size(input: &str) -> Result<u64> {
    let compact = input
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect::<String>();
    if compact.is_empty() {
        bail!("size must not be empty");
    }

    let split_index = compact
        .char_indices()
        .find_map(|(index, character)| {
            if character.is_ascii_digit() {
                None
            } else {
                Some(index)
            }
        })
        .unwrap_or(compact.len());

    let value_text = &compact[..split_index];
    let unit_text = compact[split_index..].to_ascii_uppercase();

    if value_text.is_empty() {
        bail!("size is missing numeric value");
    }

    let value = value_text
        .parse::<u64>()
        .with_context(|| format!("invalid numeric size value: {value_text}"))?;

    let multiplier: u64 = match unit_text.as_str() {
        "" | "B" => 1,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        "KIB" => 1_024,
        "MIB" => 1_048_576,
        "GIB" => 1_073_741_824,
        "TIB" => 1_099_511_627_776,
        _ => bail!("unsupported byte size unit: {unit_text}"),
    };

    value
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("byte size is too large"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::config::{Config, ConfigOverrides, flat_env_overrides_from_map, parse_byte_size};

    #[test]
    fn default_config_has_expected_values() {
        let config = Config::default();
        assert_eq!(config.server.listen, "0.0.0.0:8080");
        assert_eq!(config.server.log_level, "info");
        assert_eq!(config.loki.timeout, "30s");
    }

    #[test]
    fn parses_byte_sizes() {
        assert_eq!(parse_byte_size("500MB").expect("valid"), 500_000_000);
        assert_eq!(parse_byte_size("1GiB").expect("valid"), 1_073_741_824);
        assert_eq!(parse_byte_size("0").expect("valid"), 0);
    }

    #[test]
    fn flat_env_aliases_map_to_nested_fields() {
        let vars = BTreeMap::from([
            ("LOKI_MCP_LISTEN".to_string(), "127.0.0.1:9090".to_string()),
            (
                "LOKI_MCP_LOKI_URL".to_string(),
                "https://loki.example:3100".to_string(),
            ),
            ("LOKI_MCP_RATE_LIMIT_RPS".to_string(), "25.5".to_string()),
        ]);

        let overrides: ConfigOverrides =
            flat_env_overrides_from_map(&vars).expect("overrides should parse");
        let serialized = serde_json::to_value(overrides).expect("serializable");

        assert_eq!(serialized["server"]["listen"], "127.0.0.1:9090");
        assert_eq!(serialized["loki"]["url"], "https://loki.example:3100");
        assert_eq!(serialized["rate_limit"]["rps"], 25.5);
    }

    #[test]
    fn validation_rejects_invalid_auth_combinations() {
        let mut config = Config::default();
        config.loki.auth_type = "basic".to_string();
        config.loki.username = None;
        config.loki.password = None;

        let error = config.validate().expect_err("invalid auth should fail");
        assert!(
            error
                .to_string()
                .contains("loki.username is required when loki.auth_type=basic")
        );
    }
}
