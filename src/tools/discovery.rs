#![allow(dead_code)]

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde_json::{Value, json};

use crate::{config::Config, loki::client::LokiClient, time::parse_time_reference};

type OptionalRange = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

pub fn describe_schema(config: &Config) -> Value {
    json!({
        "labels": config.labels,
        "structured_metadata": config.structured_metadata,
        "saved_queries": config.saved_queries,
        "notes": {
            "label_selector_syntax": "{label=\"value\"}",
            "structured_metadata_filter_syntax": "{label=\"value\"} | field=\"value\"",
        }
    })
}

pub async fn list_labels(
    client: &LokiClient,
    timezone: Tz,
    start: Option<&str>,
    end: Option<&str>,
) -> Result<Value> {
    let (start_time, end_time) = parse_optional_range(start, end, timezone)?;
    let labels = client.labels(start_time, end_time).await?;

    Ok(json!({ "labels": labels }))
}

pub async fn label_values(
    client: &LokiClient,
    timezone: Tz,
    label: &str,
    start: Option<&str>,
    end: Option<&str>,
    query: Option<&str>,
) -> Result<Value> {
    let (start_time, end_time) = parse_optional_range(start, end, timezone)?;
    let values = client
        .label_values(label, start_time, end_time, query)
        .await?;

    Ok(json!({
        "label": label,
        "values": values,
    }))
}

pub async fn series(
    client: &LokiClient,
    timezone: Tz,
    matches: &[String],
    start: Option<&str>,
    end: Option<&str>,
) -> Result<Value> {
    let (start_time, end_time) = parse_optional_range(start, end, timezone)?;
    let series = client.series(matches, start_time, end_time).await?;

    Ok(json!({ "series": series }))
}

fn parse_optional_range(
    start: Option<&str>,
    end: Option<&str>,
    timezone: Tz,
) -> Result<OptionalRange> {
    let now = Utc::now();

    let end_time = end
        .map(|value| parse_time_reference(value, timezone, now))
        .transpose()?;

    let anchor = end_time.unwrap_or(now);
    let start_time = start
        .map(|value| parse_time_reference(value, timezone, anchor))
        .transpose()?;

    if let (Some(start_time), Some(end_time)) = (start_time, end_time)
        && start_time > end_time
    {
        bail!("start time must be less than or equal to end time");
    }

    Ok((start_time, end_time))
}
