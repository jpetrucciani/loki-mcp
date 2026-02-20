#![allow(dead_code)]

pub mod analysis;
pub mod discovery;
pub mod query;
pub mod utility;

use std::{collections::BTreeMap, time::Duration as StdDuration};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};

use crate::{
    cache::QueryCache,
    config::Config,
    guardrails::{self, GuardrailDecision},
    loki::{client::LokiClient, types::LokiQueryStats},
    metrics::MetricsRegistry,
    time::{parse_std_duration, parse_time_reference, resolve_time_range},
};

#[derive(Clone)]
pub struct ToolRouter {
    config: Config,
    loki_client: LokiClient,
    timezone: Tz,
    metrics: Option<MetricsRegistry>,
    cache: Option<QueryCache>,
    cache_skip_if_range_shorter_than: StdDuration,
    guardrails: GuardrailSettings,
}

#[derive(Clone, Copy)]
struct GuardrailSettings {
    max_bytes_scanned: Option<u64>,
    max_streams: Option<u64>,
    skip_stats_if_streams_below: u64,
    skip_stats_if_range_shorter_than: StdDuration,
}

impl GuardrailSettings {
    fn enabled(self) -> bool {
        self.max_bytes_scanned.is_some() || self.max_streams.is_some()
    }
}

impl ToolRouter {
    pub fn new(config: Config) -> Result<Self> {
        Self::new_with_metrics(config, None)
    }

    pub fn new_with_metrics(config: Config, metrics: Option<MetricsRegistry>) -> Result<Self> {
        let timezone = config
            .server
            .timezone
            .parse::<Tz>()
            .with_context(|| format!("invalid timezone: {}", config.server.timezone))?;
        let loki_client = LokiClient::new(&config.loki)?;
        let cache = if config.cache.enabled {
            let ttl = parse_std_duration(&config.cache.ttl)
                .with_context(|| format!("invalid cache.ttl: {}", config.cache.ttl))?;
            Some(QueryCache::new(config.cache.max_entries, ttl))
        } else {
            None
        };
        let cache_skip_if_range_shorter_than =
            parse_std_duration(&config.cache.skip_if_range_shorter_than).with_context(|| {
                format!(
                    "invalid cache.skip_if_range_shorter_than: {}",
                    config.cache.skip_if_range_shorter_than
                )
            })?;

        let max_bytes = guardrails::parse_byte_size(&config.guardrails.max_bytes_scanned)
            .with_context(|| {
                format!(
                    "invalid guardrails.max_bytes_scanned: {}",
                    config.guardrails.max_bytes_scanned
                )
            })?;
        let max_bytes_scanned = if max_bytes == 0 {
            None
        } else {
            Some(max_bytes)
        };
        let max_streams = if config.guardrails.max_streams == 0 {
            None
        } else {
            Some(config.guardrails.max_streams)
        };
        let skip_stats_if_streams_below = config.guardrails.skip_stats_if_streams_below;
        let skip_stats_if_range_shorter_than = parse_std_duration(
            &config.guardrails.skip_stats_if_range_shorter_than,
        )
        .with_context(|| {
            format!(
                "invalid guardrails.skip_stats_if_range_shorter_than: {}",
                config.guardrails.skip_stats_if_range_shorter_than
            )
        })?;

        Ok(Self {
            config,
            loki_client,
            timezone,
            metrics,
            cache,
            cache_skip_if_range_shorter_than,
            guardrails: GuardrailSettings {
                max_bytes_scanned,
                max_streams,
                skip_stats_if_streams_below,
                skip_stats_if_range_shorter_than,
            },
        })
    }

    pub async fn call(&self, tool_name: &str, params: Value) -> Result<Value> {
        let normalized_params = normalize_params(params);
        let should_use_cache = self.should_use_cache(tool_name, &normalized_params);

        if should_use_cache
            && let Some(cached) = self.try_cache_get(tool_name, &normalized_params).await?
        {
            if let Some(metrics) = self.metrics.as_ref() {
                metrics.inc_tool_cache_hit(tool_name);
            }
            return Ok(cached);
        }
        if should_use_cache && let Some(metrics) = self.metrics.as_ref() {
            metrics.inc_tool_cache_miss(tool_name);
        }

        if let Err(error) = self.enforce_guardrails(tool_name, &normalized_params).await {
            if let Some(metrics) = self.metrics.as_ref()
                && is_guardrail_error(&error)
            {
                metrics.inc_tool_guardrail_rejection(tool_name);
            }
            return Err(error);
        }

        let response = self.dispatch(tool_name, normalized_params.clone()).await?;

        if should_use_cache {
            self.try_cache_put(tool_name, &normalized_params, &response)
                .await?;
        }

        Ok(response)
    }

    async fn dispatch(&self, tool_name: &str, params: Value) -> Result<Value> {
        match tool_name {
            "loki_describe_schema" => Ok(discovery::describe_schema(&self.config)),
            "loki_list_labels" => {
                let input: StartEndParams = parse_params(params)?;
                discovery::list_labels(
                    &self.loki_client,
                    self.timezone,
                    input.start.as_deref(),
                    input.end.as_deref(),
                )
                .await
            }
            "loki_label_values" => {
                let input: LabelValuesParams = parse_params(params)?;
                discovery::label_values(
                    &self.loki_client,
                    self.timezone,
                    &input.label,
                    input.start.as_deref(),
                    input.end.as_deref(),
                    input.query.as_deref(),
                )
                .await
            }
            "loki_series" => {
                let input: SeriesParams = parse_params(params)?;
                discovery::series(
                    &self.loki_client,
                    self.timezone,
                    &input.r#match,
                    input.start.as_deref(),
                    input.end.as_deref(),
                )
                .await
            }
            "loki_query_logs" => {
                let input: query::QueryLogsInput = parse_params(params)?;
                query::query_logs(&self.loki_client, self.timezone, input).await
            }
            "loki_query_metrics" => {
                let input: query::QueryMetricsInput = parse_params(params)?;
                query::query_metrics(&self.loki_client, self.timezone, input).await
            }
            "loki_build_query" => {
                let input: query::BuildQueryInput = parse_params(params)?;
                query::build_query(&self.loki_client, self.timezone, input).await
            }
            "loki_tail" => {
                let input: query::TailInput = parse_params(params)?;
                query::tail(&self.loki_client, self.timezone, input).await
            }
            "loki_run_saved_query" => {
                let input: query::RunSavedQueryInput = parse_params(params)?;
                query::run_saved_query(&self.loki_client, &self.config, self.timezone, input).await
            }
            "loki_query_stats" => {
                let input: analysis::QueryStatsInput = parse_params(params)?;
                analysis::query_stats(&self.loki_client, self.timezone, input).await
            }
            "loki_detect_patterns" => {
                let input: analysis::DetectPatternsInput = parse_params(params)?;
                analysis::detect_patterns(&self.loki_client, self.timezone, input).await
            }
            "loki_compare_ranges" => {
                let input: analysis::CompareRangesInput = parse_params(params)?;
                analysis::compare_ranges(&self.loki_client, self.timezone, input).await
            }
            "loki_explain_query" => {
                let input: ExplainQueryParams = parse_params(params)?;
                utility::explain_query(&input.query)
            }
            "loki_suggest_metric_rule" => {
                let input: SuggestMetricRuleParams = parse_params(params)?;
                utility::suggest_metric_rule(
                    &input.query,
                    &input.metric_name,
                    input.description.as_deref(),
                    input.rule_type.as_deref(),
                    input.alert_threshold,
                    input.alert_for.as_deref(),
                )
            }
            "loki_check_health" => utility::check_health(&self.loki_client).await,
            _ => bail!("unknown tool: {tool_name}"),
        }
    }

    fn should_use_cache(&self, tool_name: &str, params: &Value) -> bool {
        if self.cache.is_none() || !is_cacheable_tool(tool_name) {
            return false;
        }

        let Ok(range_duration) = self.cache_range_duration(tool_name, params) else {
            // Best-effort only. Let the tool-specific param validation happen in dispatch.
            return true;
        };

        if let Some(duration) = range_duration {
            duration >= self.cache_skip_if_range_shorter_than
        } else {
            true
        }
    }

    async fn try_cache_get(&self, tool_name: &str, params: &Value) -> Result<Option<Value>> {
        let Some(cache) = self.cache.as_ref() else {
            return Ok(None);
        };

        let key = cache_key(tool_name, params)?;
        Ok(cache.get(&key).await)
    }

    async fn try_cache_put(&self, tool_name: &str, params: &Value, value: &Value) -> Result<()> {
        let Some(cache) = self.cache.as_ref() else {
            return Ok(());
        };

        let key = cache_key(tool_name, params)?;
        cache.insert(key, value.clone()).await;
        Ok(())
    }

    async fn enforce_guardrails(&self, tool_name: &str, params: &Value) -> Result<()> {
        if !self.guardrails.enabled() || !is_guardrailed_tool(tool_name) {
            return Ok(());
        }

        let guardrail_queries = self.guardrail_queries(tool_name, params)?;
        for guardrail_query in guardrail_queries {
            for (start, end) in &guardrail_query.ranges {
                let range_duration = duration_between(*start, *end)?;
                if range_duration < self.guardrails.skip_stats_if_range_shorter_than {
                    continue;
                }

                let mut stats = self
                    .loki_client
                    .query_stats(&guardrail_query.query, Some(*start), Some(*end))
                    .await
                    .with_context(|| {
                        format!(
                            "guardrail pre-check failed for query. narrow the query or use a shorter range (start={start}, end={end})"
                        )
                    })?;
                if needs_runtime_stats_fallback(&stats) {
                    let runtime_stats = self
                        .loki_client
                        .query_runtime_stats(&guardrail_query.query, Some(*start), Some(*end))
                        .await
                        .with_context(|| {
                            format!(
                                "guardrail pre-check failed for query. narrow the query or use a shorter range (start={start}, end={end})"
                            )
                        })?;
                    stats = merge_stats(stats, runtime_stats);
                }

                let estimated_streams = stats.streams.ok_or_else(|| {
                    anyhow::anyhow!(
                        "guardrail pre-check failed: Loki stats response is missing stream estimates. narrow the query or use a shorter range"
                    )
                })?;

                if estimated_streams < self.guardrails.skip_stats_if_streams_below {
                    continue;
                }

                let estimated_bytes = stats.bytes_processed.ok_or_else(|| {
                    anyhow::anyhow!(
                        "guardrail pre-check failed: Loki stats response is missing byte estimates. narrow the query or use a shorter range"
                    )
                })?;

                match guardrails::evaluate(
                    estimated_bytes,
                    estimated_streams,
                    self.guardrails.max_bytes_scanned,
                    self.guardrails.max_streams,
                ) {
                    GuardrailDecision::Allow => {}
                    GuardrailDecision::RejectBytes => {
                        let limit = self.guardrails.max_bytes_scanned.unwrap_or_default();
                        bail!(
                            "query rejected by guardrail: estimated bytes scanned ({estimated_bytes}) exceeds configured limit ({limit}). narrow labels or shorten the time range"
                        );
                    }
                    GuardrailDecision::RejectStreams => {
                        let limit = self.guardrails.max_streams.unwrap_or_default();
                        bail!(
                            "query rejected by guardrail: estimated streams ({estimated_streams}) exceeds configured limit ({limit}). add narrower label selectors or shorten the time range"
                        );
                    }
                }
            }
        }

        Ok(())
    }

    fn guardrail_queries(&self, tool_name: &str, params: &Value) -> Result<Vec<GuardrailQuery>> {
        match tool_name {
            "loki_query_logs" => {
                let input: query::QueryLogsInput = parse_params(params.clone())?;
                let range = resolve_time_range(
                    input.start.as_deref(),
                    input.end.as_deref(),
                    self.timezone,
                    Utc::now(),
                )?;
                Ok(vec![GuardrailQuery {
                    query: input.query,
                    ranges: vec![range],
                }])
            }
            "loki_query_metrics" => {
                let input: query::QueryMetricsInput = parse_params(params.clone())?;
                let range = resolve_time_range(
                    input.start.as_deref(),
                    input.end.as_deref(),
                    self.timezone,
                    Utc::now(),
                )?;
                Ok(vec![GuardrailQuery {
                    query: input.query,
                    ranges: vec![range],
                }])
            }
            "loki_build_query" => {
                let input: query::BuildQueryInput = parse_params(params.clone())?;
                let mut built_query = query::build_query_string(&input)?;
                if let Some(aggregation) = input.aggregation.as_deref() {
                    query::validate_aggregation(aggregation)?;
                    let aggregation_range = input.aggregation_range.as_deref().unwrap_or("5m");
                    built_query = format!("{aggregation}({built_query}[{aggregation_range}])");
                }

                let range = resolve_time_range(
                    input.start.as_deref(),
                    input.end.as_deref(),
                    self.timezone,
                    Utc::now(),
                )?;

                Ok(vec![GuardrailQuery {
                    query: built_query,
                    ranges: vec![range],
                }])
            }
            "loki_tail" => {
                let input: query::TailInput = parse_params(params.clone())?;
                if input.labels.is_empty() {
                    bail!("tail labels must not be empty");
                }
                let selector = query::selector_from_labels(&input.labels);
                let range = resolve_time_range(None, None, self.timezone, Utc::now())?;

                Ok(vec![GuardrailQuery {
                    query: selector,
                    ranges: vec![range],
                }])
            }
            "loki_run_saved_query" => {
                let input: query::RunSavedQueryInput = parse_params(params.clone())?;
                let Some(saved_query) = self
                    .config
                    .saved_queries
                    .iter()
                    .find(|saved_query| saved_query.name == input.name)
                else {
                    bail!("saved query not found: {}", input.name);
                };

                let range = resolve_time_range(
                    Some(
                        input
                            .override_range
                            .as_deref()
                            .unwrap_or(saved_query.range.as_str()),
                    ),
                    None,
                    self.timezone,
                    Utc::now(),
                )?;

                Ok(vec![GuardrailQuery {
                    query: saved_query.query.clone(),
                    ranges: vec![range],
                }])
            }
            "loki_detect_patterns" => {
                let input: analysis::DetectPatternsInput = parse_params(params.clone())?;
                let range = resolve_time_range(
                    input.start.as_deref(),
                    input.end.as_deref(),
                    self.timezone,
                    Utc::now(),
                )?;
                Ok(vec![GuardrailQuery {
                    query: input.query,
                    ranges: vec![range],
                }])
            }
            "loki_compare_ranges" => {
                let input: analysis::CompareRangesInput = parse_params(params.clone())?;
                let now = Utc::now();
                let baseline_start =
                    parse_time_reference(&input.baseline_start, self.timezone, now)?;
                let baseline_end = parse_time_reference(&input.baseline_end, self.timezone, now)?;
                ensure_ordered_range(baseline_start, baseline_end)?;

                let compare_start = parse_time_reference(&input.compare_start, self.timezone, now)?;
                let compare_end = parse_time_reference(&input.compare_end, self.timezone, now)?;
                ensure_ordered_range(compare_start, compare_end)?;

                Ok(vec![GuardrailQuery {
                    query: input.query,
                    ranges: vec![(baseline_start, baseline_end), (compare_start, compare_end)],
                }])
            }
            _ => Ok(Vec::new()),
        }
    }

    fn cache_range_duration(&self, tool_name: &str, params: &Value) -> Result<Option<StdDuration>> {
        match tool_name {
            "loki_query_logs" => {
                let input: query::QueryLogsInput = parse_params(params.clone())?;
                range_duration_from_bounds(
                    input.start.as_deref(),
                    input.end.as_deref(),
                    self.timezone,
                )
                .map(Some)
            }
            "loki_query_metrics" => {
                let input: query::QueryMetricsInput = parse_params(params.clone())?;
                range_duration_from_bounds(
                    input.start.as_deref(),
                    input.end.as_deref(),
                    self.timezone,
                )
                .map(Some)
            }
            "loki_build_query" => {
                let input: query::BuildQueryInput = parse_params(params.clone())?;
                range_duration_from_bounds(
                    input.start.as_deref(),
                    input.end.as_deref(),
                    self.timezone,
                )
                .map(Some)
            }
            "loki_tail" => range_duration_from_bounds(None, None, self.timezone).map(Some),
            "loki_run_saved_query" => {
                let input: query::RunSavedQueryInput = parse_params(params.clone())?;
                let Some(saved_query) = self
                    .config
                    .saved_queries
                    .iter()
                    .find(|saved_query| saved_query.name == input.name)
                else {
                    bail!("saved query not found: {}", input.name);
                };

                let (start, end) = resolve_time_range(
                    Some(
                        input
                            .override_range
                            .as_deref()
                            .unwrap_or(saved_query.range.as_str()),
                    ),
                    None,
                    self.timezone,
                    Utc::now(),
                )?;
                duration_between(start, end).map(Some)
            }
            "loki_query_stats" => {
                let input: analysis::QueryStatsInput = parse_params(params.clone())?;
                range_duration_from_bounds(
                    input.start.as_deref(),
                    input.end.as_deref(),
                    self.timezone,
                )
                .map(Some)
            }
            "loki_detect_patterns" => {
                let input: analysis::DetectPatternsInput = parse_params(params.clone())?;
                range_duration_from_bounds(
                    input.start.as_deref(),
                    input.end.as_deref(),
                    self.timezone,
                )
                .map(Some)
            }
            "loki_compare_ranges" => {
                let input: analysis::CompareRangesInput = parse_params(params.clone())?;
                let now = Utc::now();
                let baseline_start =
                    parse_time_reference(&input.baseline_start, self.timezone, now)?;
                let baseline_end = parse_time_reference(&input.baseline_end, self.timezone, now)?;
                ensure_ordered_range(baseline_start, baseline_end)?;
                let compare_start = parse_time_reference(&input.compare_start, self.timezone, now)?;
                let compare_end = parse_time_reference(&input.compare_end, self.timezone, now)?;
                ensure_ordered_range(compare_start, compare_end)?;

                let baseline_duration = duration_between(baseline_start, baseline_end)?;
                let compare_duration = duration_between(compare_start, compare_end)?;
                Ok(Some(baseline_duration.min(compare_duration)))
            }
            "loki_list_labels" => {
                let input: StartEndParams = parse_params(params.clone())?;
                optional_discovery_range(
                    input.start.as_deref(),
                    input.end.as_deref(),
                    self.timezone,
                )
            }
            "loki_label_values" => {
                let input: LabelValuesParams = parse_params(params.clone())?;
                optional_discovery_range(
                    input.start.as_deref(),
                    input.end.as_deref(),
                    self.timezone,
                )
            }
            "loki_series" => {
                let input: SeriesParams = parse_params(params.clone())?;
                optional_discovery_range(
                    input.start.as_deref(),
                    input.end.as_deref(),
                    self.timezone,
                )
            }
            _ => Ok(None),
        }
    }
}

fn parse_params<T: DeserializeOwned>(params: Value) -> Result<T> {
    serde_json::from_value(normalize_params(params)).context("invalid tool parameters")
}

fn normalize_params(params: Value) -> Value {
    if params.is_null() { json!({}) } else { params }
}

fn is_cacheable_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "loki_list_labels"
            | "loki_label_values"
            | "loki_series"
            | "loki_query_logs"
            | "loki_query_metrics"
            | "loki_build_query"
            | "loki_tail"
            | "loki_run_saved_query"
            | "loki_query_stats"
            | "loki_detect_patterns"
            | "loki_compare_ranges"
    )
}

fn is_guardrailed_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "loki_query_logs"
            | "loki_query_metrics"
            | "loki_build_query"
            | "loki_tail"
            | "loki_run_saved_query"
            | "loki_detect_patterns"
            | "loki_compare_ranges"
    )
}

fn is_guardrail_error(error: &anyhow::Error) -> bool {
    error.to_string().to_ascii_lowercase().contains("guardrail")
}

fn needs_runtime_stats_fallback(stats: &LokiQueryStats) -> bool {
    stats.bytes_processed.unwrap_or_default() == 0 && stats.streams.unwrap_or_default() == 0
}

fn merge_stats(primary: LokiQueryStats, fallback: LokiQueryStats) -> LokiQueryStats {
    let primary_bytes = primary.bytes_processed.unwrap_or_default();
    let primary_streams = primary.streams.unwrap_or_default();
    let primary_chunks = primary.chunks.unwrap_or_default();
    let primary_entries = primary.entries.unwrap_or_default();

    LokiQueryStats {
        bytes_processed: if primary_bytes > 0 {
            Some(primary_bytes)
        } else {
            fallback.bytes_processed
        },
        streams: if primary_streams > 0 {
            Some(primary_streams)
        } else {
            fallback.streams
        },
        chunks: if primary_chunks > 0 {
            Some(primary_chunks)
        } else {
            fallback.chunks
        },
        entries: if primary_entries > 0 {
            Some(primary_entries)
        } else {
            fallback.entries
        },
        raw: if primary.raw.is_null() {
            fallback.raw
        } else {
            primary.raw
        },
    }
}

fn cache_key(tool_name: &str, params: &Value) -> Result<String> {
    let canonical = canonicalize_json(params);
    let serialized =
        serde_json::to_string(&canonical).context("failed to serialize cache key params")?;
    Ok(format!("{tool_name}:{serialized}"))
}

fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut sorted = BTreeMap::<String, Value>::new();
            for (key, inner) in object {
                sorted.insert(key.clone(), canonicalize_json(inner));
            }

            let mut normalized_object = Map::new();
            for (key, inner) in sorted {
                normalized_object.insert(key, inner);
            }

            Value::Object(normalized_object)
        }
        Value::Array(array) => Value::Array(array.iter().map(canonicalize_json).collect()),
        other => other.clone(),
    }
}

fn range_duration_from_bounds(
    start: Option<&str>,
    end: Option<&str>,
    timezone: Tz,
) -> Result<StdDuration> {
    let (start, end) = resolve_time_range(start, end, timezone, Utc::now())?;
    duration_between(start, end)
}

fn optional_discovery_range(
    start: Option<&str>,
    end: Option<&str>,
    timezone: Tz,
) -> Result<Option<StdDuration>> {
    if start.is_none() && end.is_none() {
        return Ok(None);
    }

    range_duration_from_bounds(start, end, timezone).map(Some)
}

fn duration_between(start: DateTime<Utc>, end: DateTime<Utc>) -> Result<StdDuration> {
    end.signed_duration_since(start)
        .to_std()
        .context("time range must not be negative")
}

fn ensure_ordered_range(start: DateTime<Utc>, end: DateTime<Utc>) -> Result<()> {
    if start > end {
        bail!("start time must be less than or equal to end time");
    }

    Ok(())
}

struct GuardrailQuery {
    query: String,
    ranges: Vec<(DateTime<Utc>, DateTime<Utc>)>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct StartEndParams {
    start: Option<String>,
    end: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct LabelValuesParams {
    label: String,
    start: Option<String>,
    end: Option<String>,
    query: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SeriesParams {
    r#match: Vec<String>,
    start: Option<String>,
    end: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ExplainQueryParams {
    query: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SuggestMetricRuleParams {
    query: String,
    metric_name: String,
    description: Option<String>,
    rule_type: Option<String>,
    alert_threshold: Option<f64>,
    alert_for: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{
        config::Config,
        tools::{ToolRouter, cache_key},
    };

    #[tokio::test]
    async fn describe_schema_tool_returns_configured_schema() {
        let router = ToolRouter::new(Config::default()).expect("router should build");
        let response = router
            .call("loki_describe_schema", json!({}))
            .await
            .expect("tool should execute");

        assert!(response.get("labels").is_some());
        assert!(response.get("structured_metadata").is_some());
    }

    #[test]
    fn cache_key_is_stable_for_equivalent_json_objects() {
        let first = json!({
            "b": 2,
            "a": {
                "z": 1,
                "m": 3
            }
        });
        let second = json!({
            "a": {
                "m": 3,
                "z": 1
            },
            "b": 2
        });

        let first_key = cache_key("loki_query_logs", &first).expect("cache key");
        let second_key = cache_key("loki_query_logs", &second).expect("cache key");
        assert_eq!(first_key, second_key);
    }
}
