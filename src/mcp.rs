use std::{
    collections::BTreeMap,
    future::{self, Future},
    hash::{Hash, Hasher},
    net::SocketAddr,
    time::Instant,
};

use anyhow::{Context, Result};
use axum::http::request::Parts;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::tool::schema_for_type,
    model::{
        CallToolRequestParams, CallToolResult, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerInfo, Tool, ToolAnnotations,
    },
    service::RequestContext,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    config::Config,
    metrics::MetricsRegistry,
    rate_limit::ToolRateLimiter,
    recent_actions::{ActionOutcome, RecentActionInput, RecentActionsStore},
    tools::ToolRouter,
};

#[derive(Clone)]
pub struct LokiMcpServer {
    tool_router: ToolRouter,
    tools: Vec<Tool>,
    metrics: MetricsRegistry,
    rate_limiter: Option<ToolRateLimiter>,
    identity_header: Option<String>,
    tenant_id: Option<String>,
    recent_actions: Option<RecentActionsStore>,
}

impl LokiMcpServer {
    pub fn new(
        config: Config,
        metrics: MetricsRegistry,
        recent_actions: Option<RecentActionsStore>,
    ) -> Result<Self> {
        let rate_limiter = if config.rate_limit.enabled {
            ToolRateLimiter::new(config.rate_limit.rps, config.rate_limit.burst)
        } else {
            None
        };
        let identity_header = config.server.identity_header.clone();
        let tenant_id = config.loki.tenant_id.clone();
        let tool_router = ToolRouter::new_with_metrics(config, Some(metrics.clone()))
            .context("failed to create tool router")?;
        let tools = build_tools();

        Ok(Self {
            tool_router,
            tools,
            metrics,
            rate_limiter,
            identity_header,
            tenant_id,
            recent_actions,
        })
    }

    fn resolve_identity(&self, context: &RequestContext<RoleServer>) -> String {
        let Some(parts) = context.extensions.get::<Parts>() else {
            return "unknown".to_string();
        };

        if let Some(identity_header) = self.identity_header.as_deref()
            && let Some(identity) = header_value(parts, identity_header)
        {
            return identity;
        }

        if let Some(forwarded_for) = header_value(parts, "x-forwarded-for")
            && let Some(first_hop) = forwarded_for
                .split(',')
                .next()
                .map(str::trim)
                .filter(|value| !value.is_empty())
        {
            return first_hop.to_string();
        }

        if let Some(remote_address) = parts.extensions.get::<SocketAddr>() {
            return remote_address.ip().to_string();
        }

        "unknown".to_string()
    }

    fn resolve_request_id(&self, context: &RequestContext<RoleServer>) -> Option<String> {
        let parts = context.extensions.get::<Parts>()?;
        header_value(parts, "x-request-id")
    }

    async fn record_action(&self, input: RecentActionInput) {
        if let Some(recent_actions) = self.recent_actions.as_ref() {
            recent_actions.record(input).await;
        }
    }
}

impl ServerHandler for LokiMcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            instructions: Some(
                "Query Grafana Loki. Start with loki_describe_schema, then use query tools."
                    .to_string(),
            ),
            ..Default::default()
        };
        info.server_info.description = Some(
            "MCP server for querying Grafana Loki with environment-aware schemas.".to_string(),
        );
        info
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        future::ready(Ok(ListToolsResult::with_all_items(self.tools.clone())))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|tool| tool.name == name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let started = Instant::now();
        let tool_name = request.name.into_owned();
        let identity = self.resolve_identity(&context);
        let request_id = self.resolve_request_id(&context);
        let identity_hash = hash_string(&identity);
        let arguments_map = request.arguments.unwrap_or_default();
        let query_text = extract_query_text(&arguments_map);

        if self.get_tool(&tool_name).is_none() {
            self.metrics.inc_tool_call(&tool_name, "invalid_tool");
            self.record_action(RecentActionInput {
                request_id: request_id.clone(),
                tool: tool_name.clone(),
                outcome: ActionOutcome::InvalidTool,
                duration_ms: elapsed_millis(started),
                identity_hash: identity_hash.clone(),
                tenant_id: self.tenant_id.clone(),
                query: query_text.clone(),
                error_class: Some("invalid_tool".to_string()),
                error: Some(format!("unknown tool: {tool_name}")),
            })
            .await;
            return Err(McpError::invalid_params(
                format!("unknown tool: {tool_name}"),
                None,
            ));
        }

        if let Some(rate_limiter) = self.rate_limiter.as_ref()
            && let Err(error_message) =
                rate_limiter.check(&tool_name, &identity, self.tenant_id.as_deref())
        {
            self.metrics.inc_tool_rate_limited(&tool_name);
            self.metrics.inc_tool_call(&tool_name, "rate_limited");
            self.record_action(RecentActionInput {
                request_id: request_id.clone(),
                tool: tool_name.clone(),
                outcome: ActionOutcome::RateLimited,
                duration_ms: elapsed_millis(started),
                identity_hash: identity_hash.clone(),
                tenant_id: self.tenant_id.clone(),
                query: query_text.clone(),
                error_class: Some("rate_limited".to_string()),
                error: Some(error_message.clone()),
            })
            .await;
            return Ok(CallToolResult::structured_error(json!({
                "error": error_message,
                "tool": tool_name,
                "identity": identity,
            })));
        }

        let arguments = Value::Object(arguments_map);
        match self.tool_router.call(&tool_name, arguments).await {
            Ok(value) => {
                self.metrics.inc_tool_call(&tool_name, "success");
                self.record_action(RecentActionInput {
                    request_id: request_id.clone(),
                    tool: tool_name.clone(),
                    outcome: ActionOutcome::Success,
                    duration_ms: elapsed_millis(started),
                    identity_hash: identity_hash.clone(),
                    tenant_id: self.tenant_id.clone(),
                    query: query_text.clone(),
                    error_class: None,
                    error: None,
                })
                .await;
                Ok(CallToolResult::structured(value))
            }
            Err(error) => {
                let message = error.to_string();
                let (outcome, error_class) = classify_error(&message);
                self.metrics.inc_tool_call(&tool_name, "error");
                self.record_action(RecentActionInput {
                    request_id: request_id.clone(),
                    tool: tool_name.clone(),
                    outcome,
                    duration_ms: elapsed_millis(started),
                    identity_hash: identity_hash.clone(),
                    tenant_id: self.tenant_id.clone(),
                    query: query_text.clone(),
                    error_class: Some(error_class),
                    error: Some(message.clone()),
                })
                .await;
                Ok(CallToolResult::structured_error(json!({
                    "error": message,
                    "tool": tool_name,
                })))
            }
        }
    }
}

fn build_tools() -> Vec<Tool> {
    vec![
        readonly_tool::<NoParams>(
            "loki_describe_schema",
            "Return configured label, structured metadata, and saved-query schema briefing.",
        ),
        readonly_tool::<ListLabelsParams>(
            "loki_list_labels",
            "List label names known to Loki, optionally scoped to a time range.",
        ),
        readonly_tool::<LabelValuesParams>(
            "loki_label_values",
            "List values for a label, optionally scoped by time and query selector.",
        ),
        readonly_tool::<SeriesParams>(
            "loki_series",
            "List matching series (unique label sets) for one or more LogQL matchers.",
        ),
        readonly_tool::<QueryLogsParams>(
            "loki_query_logs",
            "Run a LogQL log query with optional time range and result controls.",
        ),
        readonly_tool::<QueryMetricsParams>(
            "loki_query_metrics",
            "Run a LogQL metric query and return numeric series data.",
        ),
        readonly_tool::<BuildQueryParams>(
            "loki_build_query",
            "Build LogQL from structured filters, then execute and return results.",
        ),
        readonly_tool::<TailParams>(
            "loki_tail",
            "Fetch the latest log lines for a required label set.",
        ),
        readonly_tool::<RunSavedQueryParams>(
            "loki_run_saved_query",
            "Run a configured saved query by name with optional range override.",
        ),
        readonly_tool::<QueryStatsParams>(
            "loki_query_stats",
            "Return Loki index query statistics for cost estimation.",
        ),
        readonly_tool::<DetectPatternsParams>(
            "loki_detect_patterns",
            "Detect recurring patterns from logs matching a query in a time range.",
        ),
        readonly_tool::<CompareRangesParams>(
            "loki_compare_ranges",
            "Compare line volumes for a query across two explicit ranges.",
        ),
        readonly_tool::<ExplainQueryParams>(
            "loki_explain_query",
            "Explain key parts of a LogQL query (selector, stages, aggregation).",
        ),
        readonly_tool::<SuggestMetricRuleParams>(
            "loki_suggest_metric_rule",
            "Generate a recording or alerting rule from a LogQL query.",
        ),
        readonly_tool::<NoParams>(
            "loki_check_health",
            "Check Loki readiness/build/ring health status through the configured endpoint.",
        ),
    ]
}

fn readonly_tool<T>(name: &'static str, description: &'static str) -> Tool
where
    T: JsonSchema + std::any::Any,
{
    Tool::new(name, description, schema_for_type::<T>())
        .annotate(ToolAnnotations::new().read_only(true).idempotent(true))
}

fn header_value(parts: &Parts, name: &str) -> Option<String> {
    parts
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn hash_string(value: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn extract_query_text(arguments: &Map<String, Value>) -> Option<String> {
    arguments
        .get("query")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn classify_error(message: &str) -> (ActionOutcome, String) {
    let normalized = message.to_ascii_lowercase();
    if normalized.contains("guardrail") {
        return (ActionOutcome::GuardrailReject, "guardrail".to_string());
    }

    (ActionOutcome::Error, "tool_error".to_string())
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoParams {}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListLabelsParams {
    start: Option<String>,
    end: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct LabelValuesParams {
    label: String,
    start: Option<String>,
    end: Option<String>,
    query: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SeriesParams {
    #[serde(rename = "match")]
    r#match: Vec<String>,
    start: Option<String>,
    end: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct QueryLogsParams {
    query: String,
    start: Option<String>,
    end: Option<String>,
    limit: Option<u32>,
    direction: Option<String>,
    response_mode: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct QueryMetricsParams {
    query: String,
    start: Option<String>,
    end: Option<String>,
    step: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct BuildQueryParams {
    labels: Option<BTreeMap<String, String>>,
    structured_metadata: Option<BTreeMap<String, String>>,
    line_filter: Option<String>,
    line_filter_regex: Option<String>,
    exclude: Option<String>,
    json_fields: Option<BTreeMap<String, String>>,
    aggregation: Option<String>,
    aggregation_range: Option<String>,
    start: Option<String>,
    end: Option<String>,
    limit: Option<u32>,
    response_mode: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TailParams {
    labels: BTreeMap<String, String>,
    lines: Option<u32>,
    response_mode: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RunSavedQueryParams {
    name: String,
    override_range: Option<String>,
    response_mode: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct QueryStatsParams {
    query: String,
    start: Option<String>,
    end: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DetectPatternsParams {
    query: String,
    start: Option<String>,
    end: Option<String>,
    step: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CompareRangesParams {
    query: String,
    baseline_start: String,
    baseline_end: String,
    compare_start: String,
    compare_end: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExplainQueryParams {
    query: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
    use std::collections::BTreeSet;

    use crate::mcp::build_tools;

    #[test]
    fn registers_all_spec_tools_with_unique_names() {
        let tools = build_tools();
        assert_eq!(tools.len(), 15);

        let unique_count = tools
            .iter()
            .map(|tool| tool.name.to_string())
            .collect::<BTreeSet<String>>()
            .len();

        assert_eq!(unique_count, 15);
    }
}
