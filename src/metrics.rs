use anyhow::{Context, Result};
use prometheus::{IntCounter, IntCounterVec, Opts, Registry, TextEncoder};

#[derive(Clone)]
pub struct MetricsRegistry {
    registry: Registry,
    http_requests_total: IntCounter,
    tool_calls_total: IntCounterVec,
    tool_cache_total: IntCounterVec,
    tool_guardrail_rejections_total: IntCounterVec,
    tool_rate_limited_total: IntCounterVec,
    readiness_cache_total: IntCounterVec,
}

impl MetricsRegistry {
    pub fn new(prefix: &str) -> Result<Self> {
        let registry = Registry::new();
        let http_requests_total = IntCounter::with_opts(Opts::new(
            format!("{prefix}_http_requests_total"),
            "Total HTTP requests handled by loki-mcp",
        ))
        .context("failed to create http_requests_total metric")?;

        let tool_calls_total = IntCounterVec::new(
            Opts::new(
                format!("{prefix}_tool_calls_total"),
                "Total MCP tool calls partitioned by tool and outcome",
            ),
            &["tool", "outcome"],
        )
        .context("failed to create tool_calls_total metric")?;

        let tool_cache_total = IntCounterVec::new(
            Opts::new(
                format!("{prefix}_tool_cache_total"),
                "Total cache lookups partitioned by tool and result",
            ),
            &["tool", "result"],
        )
        .context("failed to create tool_cache_total metric")?;

        let tool_guardrail_rejections_total = IntCounterVec::new(
            Opts::new(
                format!("{prefix}_tool_guardrail_rejections_total"),
                "Total MCP tool guardrail rejections partitioned by tool",
            ),
            &["tool"],
        )
        .context("failed to create tool_guardrail_rejections_total metric")?;

        let tool_rate_limited_total = IntCounterVec::new(
            Opts::new(
                format!("{prefix}_tool_rate_limited_total"),
                "Total MCP tool calls rejected by rate limiting partitioned by tool",
            ),
            &["tool"],
        )
        .context("failed to create tool_rate_limited_total metric")?;

        let readiness_cache_total = IntCounterVec::new(
            Opts::new(
                format!("{prefix}_readiness_cache_total"),
                "Total readiness cache lookups partitioned by result",
            ),
            &["result"],
        )
        .context("failed to create readiness_cache_total metric")?;

        registry
            .register(Box::new(http_requests_total.clone()))
            .context("failed to register http_requests_total metric")?;
        registry
            .register(Box::new(tool_calls_total.clone()))
            .context("failed to register tool_calls_total metric")?;
        registry
            .register(Box::new(tool_cache_total.clone()))
            .context("failed to register tool_cache_total metric")?;
        registry
            .register(Box::new(tool_guardrail_rejections_total.clone()))
            .context("failed to register tool_guardrail_rejections_total metric")?;
        registry
            .register(Box::new(tool_rate_limited_total.clone()))
            .context("failed to register tool_rate_limited_total metric")?;
        registry
            .register(Box::new(readiness_cache_total.clone()))
            .context("failed to register readiness_cache_total metric")?;

        Ok(Self {
            registry,
            http_requests_total,
            tool_calls_total,
            tool_cache_total,
            tool_guardrail_rejections_total,
            tool_rate_limited_total,
            readiness_cache_total,
        })
    }

    pub fn inc_http_requests(&self) {
        self.http_requests_total.inc();
    }

    pub fn inc_tool_call(&self, tool: &str, outcome: &str) {
        self.tool_calls_total
            .with_label_values(&[tool, outcome])
            .inc();
    }

    pub fn inc_tool_cache_hit(&self, tool: &str) {
        self.tool_cache_total
            .with_label_values(&[tool, "hit"])
            .inc();
    }

    pub fn inc_tool_cache_miss(&self, tool: &str) {
        self.tool_cache_total
            .with_label_values(&[tool, "miss"])
            .inc();
    }

    pub fn inc_tool_guardrail_rejection(&self, tool: &str) {
        self.tool_guardrail_rejections_total
            .with_label_values(&[tool])
            .inc();
    }

    pub fn inc_tool_rate_limited(&self, tool: &str) {
        self.tool_rate_limited_total
            .with_label_values(&[tool])
            .inc();
    }

    pub fn inc_readiness_cache_hit(&self) {
        self.readiness_cache_total.with_label_values(&["hit"]).inc();
    }

    pub fn inc_readiness_cache_miss(&self) {
        self.readiness_cache_total
            .with_label_values(&["miss"])
            .inc();
    }

    pub fn render(&self) -> Result<String> {
        let metric_families = self.registry.gather();
        let mut body = String::new();
        let encoder = TextEncoder::new();
        encoder
            .encode_utf8(&metric_families, &mut body)
            .context("failed encoding prometheus metrics")?;

        Ok(body)
    }
}
