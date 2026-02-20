mod support;

use anyhow::Result;
use loki_mcp::{
    config::{Config, SavedQuery},
    tools::ToolRouter,
};
use serde_json::json;
use support::loki::{LokiTestHarness, skip_reason_for_real_loki_tests};

#[tokio::test]
async fn tool_router_query_tools_execute_against_real_loki() -> Result<()> {
    if let Some(reason) = skip_reason_for_real_loki_tests() {
        eprintln!("skipping `tool_router_query_tools_execute_against_real_loki`: {reason}");
        return Ok(());
    }

    let harness = LokiTestHarness::start().await?;
    harness.seed_example_logs().await?;

    let router = ToolRouter::new(router_config(harness.base_url(), harness.run_id(), false))?;

    let query_selector = harness.scoped_selector();

    let logs_response = router
        .call(
            "loki_query_logs",
            json!({
                "query": query_selector,
                "response_mode": "raw",
            }),
        )
        .await?;
    assert_eq!(logs_response["data"]["mode"], "raw");
    assert!(logs_response["data"]["total_lines"].as_u64().unwrap_or(0) > 0);

    let build_response = router
        .call(
            "loki_build_query",
            json!({
                "labels": {
                    "test_run_id": harness.run_id(),
                },
                "response_mode": "truncated",
            }),
        )
        .await?;
    assert!(
        build_response["query"]
            .as_str()
            .unwrap_or("")
            .contains("test_run_id")
    );
    assert_eq!(build_response["data"]["mode"], "truncated");

    let saved_response = router
        .call(
            "loki_run_saved_query",
            json!({
                "name": "seeded_logs",
                "response_mode": "summary",
            }),
        )
        .await?;
    assert_eq!(saved_response["name"], "seeded_logs");
    assert_eq!(saved_response["data"]["mode"], "summary");
    assert!(saved_response["data"]["total_lines"].as_u64().unwrap_or(0) > 0);

    Ok(())
}

#[tokio::test]
async fn tool_router_uses_cache_when_loki_becomes_unavailable() -> Result<()> {
    if let Some(reason) = skip_reason_for_real_loki_tests() {
        eprintln!("skipping `tool_router_uses_cache_when_loki_becomes_unavailable`: {reason}");
        return Ok(());
    }

    let mut harness = LokiTestHarness::start().await?;
    harness.seed_example_logs().await?;

    let mut config = router_config(harness.base_url(), harness.run_id(), true);
    config.cache.skip_if_range_shorter_than = "1s".to_string();
    config.cache.ttl = "5m".to_string();

    let router = ToolRouter::new(config)?;
    let params = json!({
        "query": harness.scoped_selector(),
        "response_mode": "raw",
    });

    let first = router.call("loki_query_logs", params.clone()).await?;
    harness.stop()?;
    let second = router.call("loki_query_logs", params).await?;

    assert_eq!(first, second);
    Ok(())
}

#[tokio::test]
async fn tool_router_guardrails_reject_over_threshold_query() -> Result<()> {
    if let Some(reason) = skip_reason_for_real_loki_tests() {
        eprintln!("skipping `tool_router_guardrails_reject_over_threshold_query`: {reason}");
        return Ok(());
    }

    let harness = LokiTestHarness::start().await?;
    harness.seed_example_logs().await?;

    let mut config = router_config(harness.base_url(), harness.run_id(), false);
    config.cache.enabled = false;
    config.guardrails.max_bytes_scanned = "1B".to_string();
    config.guardrails.max_streams = 0;
    config.guardrails.skip_stats_if_streams_below = 0;
    config.guardrails.skip_stats_if_range_shorter_than = "0s".to_string();

    let router = ToolRouter::new(config)?;
    let error = router
        .call(
            "loki_query_logs",
            json!({
                "query": harness.scoped_selector(),
                "response_mode": "raw",
            }),
        )
        .await
        .expect_err("guardrail should reject or fail closed");

    assert!(error.to_string().contains("guardrail"));
    Ok(())
}

fn router_config(base_url: &str, run_id: &str, disable_guardrails: bool) -> Config {
    let mut config = Config::default();
    config.loki.url = base_url.to_string();
    config.loki.timeout = "30s".to_string();
    config.saved_queries = vec![SavedQuery {
        name: "seeded_logs".to_string(),
        description: "Logs seeded by integration test harness".to_string(),
        query: format!("{{test_run_id=\"{run_id}\"}}"),
        range: "30m".to_string(),
    }];

    if disable_guardrails {
        config.guardrails.max_bytes_scanned = "0B".to_string();
        config.guardrails.max_streams = 0;
    }

    config
}
