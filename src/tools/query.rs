#![allow(dead_code)]

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use chrono::Utc;
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    config::Config,
    loki::client::LokiClient,
    response::{ResponseMode, format_log_result},
    time::resolve_time_range,
};

#[derive(Debug, Clone, Deserialize)]
pub struct QueryLogsInput {
    pub query: String,
    pub start: Option<String>,
    pub end: Option<String>,
    pub limit: Option<u32>,
    pub direction: Option<String>,
    pub response_mode: Option<ResponseMode>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QueryMetricsInput {
    pub query: String,
    pub start: Option<String>,
    pub end: Option<String>,
    pub step: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BuildQueryInput {
    pub labels: Option<BTreeMap<String, String>>,
    pub structured_metadata: Option<BTreeMap<String, String>>,
    pub line_filter: Option<String>,
    pub line_filter_regex: Option<String>,
    pub exclude: Option<String>,
    pub json_fields: Option<BTreeMap<String, String>>,
    pub aggregation: Option<String>,
    pub aggregation_range: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub limit: Option<u32>,
    pub response_mode: Option<ResponseMode>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TailInput {
    pub labels: BTreeMap<String, String>,
    pub lines: Option<u32>,
    pub response_mode: Option<ResponseMode>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunSavedQueryInput {
    pub name: String,
    pub override_range: Option<String>,
    pub response_mode: Option<ResponseMode>,
}

pub async fn query_logs(client: &LokiClient, timezone: Tz, input: QueryLogsInput) -> Result<Value> {
    let (start, end) = resolve_time_range(
        input.start.as_deref(),
        input.end.as_deref(),
        timezone,
        Utc::now(),
    )?;

    let requested_response_mode = input.response_mode.unwrap_or_default();
    let data = client
        .query_logs(
            &input.query,
            Some(start),
            Some(end),
            Some(input.limit.unwrap_or(100)),
            input.direction.as_deref(),
        )
        .await?;
    let (response_mode, formatted_data) = format_log_result(requested_response_mode, data);

    Ok(json!({
        "query": input.query,
        "start": start,
        "end": end,
        "response_mode_requested": requested_response_mode,
        "response_mode": response_mode,
        "data": formatted_data,
    }))
}

pub async fn query_metrics(
    client: &LokiClient,
    timezone: Tz,
    input: QueryMetricsInput,
) -> Result<Value> {
    let (start, end) = resolve_time_range(
        input.start.as_deref(),
        input.end.as_deref(),
        timezone,
        Utc::now(),
    )?;

    let data = client
        .query_metrics(&input.query, Some(start), Some(end), input.step.as_deref())
        .await?;

    Ok(json!({
        "query": input.query,
        "start": start,
        "end": end,
        "step": input.step,
        "data": data,
    }))
}

pub async fn build_query(
    client: &LokiClient,
    timezone: Tz,
    input: BuildQueryInput,
) -> Result<Value> {
    let mut query = build_query_string(&input)?;
    let requested_response_mode = input.response_mode.unwrap_or_default();

    let (start, end) = resolve_time_range(
        input.start.as_deref(),
        input.end.as_deref(),
        timezone,
        Utc::now(),
    )?;

    let (response_mode, data) = if let Some(aggregation) = input.aggregation.as_deref() {
        validate_aggregation(aggregation)?;
        let range = input
            .aggregation_range
            .as_deref()
            .unwrap_or("5m")
            .to_string();

        query = format!("{aggregation}({query}[{range}])");
        let metrics = client
            .query_metrics(&query, Some(start), Some(end), None)
            .await?;
        (requested_response_mode, metrics)
    } else {
        let logs = client
            .query_logs(
                &query,
                Some(start),
                Some(end),
                Some(input.limit.unwrap_or(100)),
                Some("backward"),
            )
            .await?;
        format_log_result(requested_response_mode, logs)
    };

    Ok(json!({
        "query": query,
        "start": start,
        "end": end,
        "response_mode_requested": requested_response_mode,
        "response_mode": response_mode,
        "data": data,
    }))
}

pub async fn tail(client: &LokiClient, timezone: Tz, input: TailInput) -> Result<Value> {
    if input.labels.is_empty() {
        bail!("tail labels must not be empty");
    }

    let selector = selector_from_labels(&input.labels);
    let requested_response_mode = input.response_mode.unwrap_or_default();

    let (start, end) = resolve_time_range(None, None, timezone, Utc::now())?;
    let data = client
        .query_logs(
            &selector,
            Some(start),
            Some(end),
            Some(input.lines.unwrap_or(50)),
            Some("backward"),
        )
        .await?;
    let (response_mode, formatted_data) = format_log_result(requested_response_mode, data);

    Ok(json!({
        "query": selector,
        "start": start,
        "end": end,
        "response_mode_requested": requested_response_mode,
        "response_mode": response_mode,
        "data": formatted_data,
    }))
}

pub async fn run_saved_query(
    client: &LokiClient,
    config: &Config,
    timezone: Tz,
    input: RunSavedQueryInput,
) -> Result<Value> {
    let Some(saved_query) = config
        .saved_queries
        .iter()
        .find(|query| query.name == input.name)
    else {
        bail!("saved query not found: {}", input.name);
    };

    let range = input
        .override_range
        .as_deref()
        .unwrap_or(saved_query.range.as_str());

    let (start, end) = resolve_time_range(Some(range), None, timezone, Utc::now())?;

    let requested_response_mode = input.response_mode.unwrap_or_default();
    let data = client
        .query_logs(
            &saved_query.query,
            Some(start),
            Some(end),
            Some(100),
            Some("backward"),
        )
        .await?;
    let (response_mode, formatted_data) = format_log_result(requested_response_mode, data);

    Ok(json!({
        "name": saved_query.name,
        "query": saved_query.query,
        "description": saved_query.description,
        "start": start,
        "end": end,
        "response_mode_requested": requested_response_mode,
        "response_mode": response_mode,
        "data": formatted_data,
    }))
}

pub(crate) fn build_query_string(input: &BuildQueryInput) -> Result<String> {
    let selector = selector_from_labels(input.labels.as_ref().unwrap_or(&BTreeMap::new()));

    let mut parts = vec![selector];

    if let Some(structured_metadata) = input.structured_metadata.as_ref() {
        for (field, value) in structured_metadata {
            parts.push(format!("| {field}=\"{}\"", escape_logql_value(value)));
        }
    }

    if let Some(line_filter) = input.line_filter.as_deref() {
        parts.push(format!("|= \"{}\"", escape_logql_value(line_filter)));
    }

    if let Some(line_filter_regex) = input.line_filter_regex.as_deref() {
        parts.push(format!("|~ \"{}\"", escape_logql_value(line_filter_regex)));
    }

    if let Some(exclude) = input.exclude.as_deref() {
        parts.push(format!("!= \"{}\"", escape_logql_value(exclude)));
    }

    if let Some(json_fields) = input.json_fields.as_ref()
        && !json_fields.is_empty()
    {
        parts.push("| json".to_string());
        for (field, value) in json_fields {
            parts.push(format!("| {field}=\"{}\"", escape_logql_value(value)));
        }
    }

    Ok(parts.join(" "))
}

pub(crate) fn selector_from_labels(labels: &BTreeMap<String, String>) -> String {
    if labels.is_empty() {
        return "{}".to_string();
    }

    let pairs = labels
        .iter()
        .map(|(key, value)| format!("{key}=\"{}\"", escape_logql_value(value)))
        .collect::<Vec<String>>();

    format!("{{{}}}", pairs.join(","))
}

pub(crate) fn validate_aggregation(aggregation: &str) -> Result<()> {
    match aggregation {
        "count_over_time" | "rate" | "bytes_over_time" | "bytes_rate" => Ok(()),
        _ => bail!(
            "unsupported aggregation: {aggregation}. expected one of count_over_time, rate, bytes_over_time, bytes_rate"
        ),
    }
}

fn escape_logql_value(input: &str) -> String {
    input.replace('\\', "\\\\").replace('"', "\\\"")
}
