#![allow(dead_code)]

use anyhow::Result;
use chrono::Utc;
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    loki::client::LokiClient,
    time::{parse_time_reference, resolve_time_range},
};

#[derive(Debug, Clone, Deserialize)]
pub struct QueryStatsInput {
    pub query: String,
    pub start: Option<String>,
    pub end: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DetectPatternsInput {
    pub query: String,
    pub start: Option<String>,
    pub end: Option<String>,
    pub step: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompareRangesInput {
    pub query: String,
    pub baseline_start: String,
    pub baseline_end: String,
    pub compare_start: String,
    pub compare_end: String,
}

pub async fn query_stats(
    client: &LokiClient,
    timezone: Tz,
    input: QueryStatsInput,
) -> Result<Value> {
    let (start, end) = resolve_time_range(
        input.start.as_deref(),
        input.end.as_deref(),
        timezone,
        Utc::now(),
    )?;

    let stats = client
        .query_stats(&input.query, Some(start), Some(end))
        .await?;

    Ok(json!({
        "query": input.query,
        "start": start,
        "end": end,
        "stats": stats,
    }))
}

pub async fn detect_patterns(
    client: &LokiClient,
    timezone: Tz,
    input: DetectPatternsInput,
) -> Result<Value> {
    let (start, end) = resolve_time_range(
        input.start.as_deref(),
        input.end.as_deref(),
        timezone,
        Utc::now(),
    )?;

    let patterns = client
        .detect_patterns(&input.query, Some(start), Some(end), input.step.as_deref())
        .await?;

    Ok(json!({
        "query": input.query,
        "start": start,
        "end": end,
        "patterns": patterns,
    }))
}

pub async fn compare_ranges(
    client: &LokiClient,
    timezone: Tz,
    input: CompareRangesInput,
) -> Result<Value> {
    let now = Utc::now();
    let baseline_start = parse_time_reference(&input.baseline_start, timezone, now)?;
    let baseline_end = parse_time_reference(&input.baseline_end, timezone, now)?;
    let compare_start = parse_time_reference(&input.compare_start, timezone, now)?;
    let compare_end = parse_time_reference(&input.compare_end, timezone, now)?;

    let baseline_data = client
        .query_logs(
            &input.query,
            Some(baseline_start),
            Some(baseline_end),
            Some(1000),
            Some("backward"),
        )
        .await?;

    let compare_data = client
        .query_logs(
            &input.query,
            Some(compare_start),
            Some(compare_end),
            Some(1000),
            Some("backward"),
        )
        .await?;

    let baseline_lines = count_lines(&baseline_data);
    let compare_lines = count_lines(&compare_data);

    Ok(json!({
        "query": input.query,
        "baseline": {
            "start": baseline_start,
            "end": baseline_end,
            "line_count": baseline_lines,
        },
        "compare": {
            "start": compare_start,
            "end": compare_end,
            "line_count": compare_lines,
        },
        "delta": {
            "line_count": compare_lines as i64 - baseline_lines as i64,
            "ratio": ratio(compare_lines, baseline_lines),
        },
    }))
}

fn count_lines(data: &Value) -> u64 {
    let Some(result) = data.get("result").and_then(Value::as_array) else {
        return 0;
    };

    let mut count = 0_u64;
    for stream in result {
        if let Some(values) = stream.get("values").and_then(Value::as_array) {
            count += values.len() as u64;
        }
    }

    count
}

fn ratio(compare: u64, baseline: u64) -> f64 {
    if baseline == 0 {
        return 0.0;
    }

    compare as f64 / baseline as f64
}
