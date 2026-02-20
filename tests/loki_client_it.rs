mod support;

use anyhow::Result;
use chrono::{Duration, Utc};
use loki_mcp::{config::LokiConfig, loki::client::LokiClient};
use serde_json::Value;
use support::loki::LokiTestHarness;

#[tokio::test]
async fn loki_client_endpoints_work_against_real_loki() -> Result<()> {
    let harness = LokiTestHarness::start().await?;
    harness.seed_example_logs().await?;

    let config = LokiConfig {
        url: harness.base_url().to_string(),
        timeout: "30s".to_string(),
        ..Default::default()
    };
    let client = LokiClient::new(&config)?;

    let end = Utc::now();
    let start = end - Duration::minutes(30);
    let selector = harness.scoped_selector();

    let labels = client.labels(Some(start), Some(end)).await?;
    assert!(labels.iter().any(|label| label == "test_run_id"));

    let label_values = client
        .label_values("test_run_id", Some(start), Some(end), Some(&selector))
        .await?;
    assert!(label_values.iter().any(|value| value == harness.run_id()));

    let series = client
        .series(std::slice::from_ref(&selector), Some(start), Some(end))
        .await?;
    assert!(!series.is_empty());

    let logs = client
        .query_logs(
            &selector,
            Some(start),
            Some(end),
            Some(100),
            Some("backward"),
        )
        .await?;
    let log_result = logs
        .get("result")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(!log_result.is_empty());

    let metric_query = format!("count_over_time({selector}[1m])");
    let metrics = client
        .query_metrics(&metric_query, Some(start), Some(end), Some("30s"))
        .await?;
    let metric_result = metrics
        .get("result")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(!metric_result.is_empty());

    let stats = client
        .query_stats(&selector, Some(start), Some(end))
        .await?;
    assert!(!stats.raw.is_null());

    Ok(())
}
