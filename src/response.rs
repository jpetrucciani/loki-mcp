#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResponseMode {
    Raw,
    Truncated,
    Summary,
    #[default]
    Smart,
}

impl ResponseMode {
    pub fn resolve_for_line_count(self, line_count: usize) -> Self {
        match self {
            Self::Smart => {
                if line_count <= 50 {
                    Self::Raw
                } else if line_count <= 500 {
                    Self::Truncated
                } else {
                    Self::Summary
                }
            }
            other => other,
        }
    }
}

pub fn format_log_result(requested_mode: ResponseMode, raw_data: Value) -> (ResponseMode, Value) {
    let entries = flatten_log_entries(&raw_data);
    let applied_mode = requested_mode.resolve_for_line_count(entries.len());

    match applied_mode {
        ResponseMode::Raw => (
            applied_mode,
            json!({
                "mode": "raw",
                "total_lines": entries.len(),
                "result": raw_data,
            }),
        ),
        ResponseMode::Truncated => {
            let edge = if requested_mode == ResponseMode::Smart {
                15
            } else {
                10
            };
            let (lines, omitted_lines) = truncate_lines(&entries, edge);
            let mut payload = json!({
                "mode": "truncated",
                "total_lines": entries.len(),
                "shown_lines": lines.len(),
                "omitted_lines": omitted_lines,
                "lines": lines,
            });

            if requested_mode == ResponseMode::Smart {
                let summary = summary_payload(&entries, false);
                if let Some(object) = payload.as_object_mut() {
                    object.insert(
                        "pattern_summary".to_string(),
                        summary["top_patterns"].clone(),
                    );
                }
            }

            (applied_mode, payload)
        }
        ResponseMode::Summary => {
            let include_samples = requested_mode == ResponseMode::Smart;
            (applied_mode, summary_payload(&entries, include_samples))
        }
        ResponseMode::Smart => (
            // `Smart` always resolves above, but keep a safe fallback.
            ResponseMode::Raw,
            json!({
                "mode": "raw",
                "total_lines": entries.len(),
                "result": raw_data,
            }),
        ),
    }
}

#[derive(Debug, Clone, Serialize)]
struct LogLineEntry {
    timestamp: String,
    line: String,
    stream: BTreeMap<String, String>,
}

fn flatten_log_entries(raw_data: &Value) -> Vec<LogLineEntry> {
    let mut entries = Vec::new();

    let Some(streams) = raw_data.get("result").and_then(Value::as_array) else {
        return entries;
    };

    for stream in streams {
        let stream_labels = stream
            .get("stream")
            .and_then(Value::as_object)
            .map(|object| {
                object
                    .iter()
                    .map(|(key, value)| (key.clone(), value.as_str().unwrap_or("").to_string()))
                    .collect::<BTreeMap<String, String>>()
            })
            .unwrap_or_default();

        let Some(values) = stream.get("values").and_then(Value::as_array) else {
            continue;
        };

        for value in values {
            let Some(pair) = value.as_array() else {
                continue;
            };
            if pair.len() != 2 {
                continue;
            }

            let Some(timestamp_nanos) = pair[0].as_str() else {
                continue;
            };
            let Some(line) = pair[1].as_str() else {
                continue;
            };

            let timestamp = nanos_to_rfc3339(timestamp_nanos).unwrap_or_else(|| {
                // Keep timestamp info even if conversion fails.
                timestamp_nanos.to_string()
            });

            entries.push(LogLineEntry {
                timestamp,
                line: line.to_string(),
                stream: stream_labels.clone(),
            });
        }
    }

    entries
}

fn truncate_lines(entries: &[LogLineEntry], edge_count: usize) -> (Vec<LogLineEntry>, usize) {
    if entries.len() <= edge_count.saturating_mul(2) {
        return (entries.to_vec(), 0);
    }

    let mut lines = Vec::with_capacity(edge_count.saturating_mul(2));
    lines.extend(entries.iter().take(edge_count).cloned());
    lines.extend(
        entries
            .iter()
            .skip(entries.len().saturating_sub(edge_count))
            .cloned(),
    );

    let omitted = entries.len().saturating_sub(lines.len());
    (lines, omitted)
}

fn summary_payload(entries: &[LogLineEntry], include_samples: bool) -> Value {
    let mut level_counts = BTreeMap::<String, u64>::new();
    let mut pattern_counts = HashMap::<String, u64>::new();
    let mut pattern_sample = HashMap::<String, LogLineEntry>::new();
    let mut time_buckets = BTreeMap::<String, u64>::new();

    let mut first_timestamp: Option<DateTime<Utc>> = None;
    let mut last_timestamp: Option<DateTime<Utc>> = None;

    for entry in entries {
        if let Some(level) = detect_level(&entry.line) {
            *level_counts.entry(level).or_insert(0) += 1;
        }

        let pattern = normalize_pattern(&entry.line);
        *pattern_counts.entry(pattern.clone()).or_insert(0) += 1;
        pattern_sample
            .entry(pattern)
            .or_insert_with(|| entry.clone());

        if let Some(timestamp) = parse_entry_timestamp(&entry.timestamp) {
            first_timestamp =
                Some(first_timestamp.map_or(timestamp, |current| current.min(timestamp)));
            last_timestamp =
                Some(last_timestamp.map_or(timestamp, |current| current.max(timestamp)));

            let bucket = time_bucket_5m(timestamp);
            *time_buckets.entry(bucket).or_insert(0) += 1;
        }
    }

    let mut top_patterns = pattern_counts.into_iter().collect::<Vec<(String, u64)>>();
    top_patterns.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    if top_patterns.len() > 10 {
        top_patterns.truncate(10);
    }

    let patterns = if include_samples {
        top_patterns
            .iter()
            .map(|(pattern, count)| {
                let sample = pattern_sample
                    .get(pattern)
                    .map(|entry| {
                        json!({
                            "timestamp": entry.timestamp,
                            "line": entry.line,
                        })
                    })
                    .unwrap_or(Value::Null);
                json!({
                    "pattern": pattern,
                    "count": count,
                    "sample": sample,
                })
            })
            .collect::<Vec<Value>>()
    } else {
        top_patterns
            .iter()
            .map(|(pattern, count)| {
                json!({
                    "pattern": pattern,
                    "count": count,
                })
            })
            .collect::<Vec<Value>>()
    };

    json!({
        "mode": "summary",
        "total_lines": entries.len(),
        "first_timestamp": first_timestamp.map(|value| value.to_rfc3339()),
        "last_timestamp": last_timestamp.map(|value| value.to_rfc3339()),
        "level_breakdown": level_counts,
        "top_patterns": patterns,
        "time_distribution_5m": time_buckets,
    })
}

fn nanos_to_rfc3339(timestamp_nanos: &str) -> Option<String> {
    let nanos = timestamp_nanos.parse::<i64>().ok()?;
    let seconds = nanos.div_euclid(1_000_000_000);
    let nanos_part = nanos.rem_euclid(1_000_000_000) as u32;
    let timestamp = DateTime::<Utc>::from_timestamp(seconds, nanos_part)?;
    Some(timestamp.to_rfc3339())
}

fn parse_entry_timestamp(timestamp: &str) -> Option<DateTime<Utc>> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(timestamp) {
        return Some(parsed.with_timezone(&Utc));
    }

    nanos_to_rfc3339(timestamp)
        .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn time_bucket_5m(timestamp: DateTime<Utc>) -> String {
    let bucket_seconds = timestamp.timestamp().div_euclid(300) * 300;
    DateTime::<Utc>::from_timestamp(bucket_seconds, 0)
        .map(|bucket| bucket.to_rfc3339())
        .unwrap_or_else(|| timestamp.to_rfc3339())
}

fn detect_level(line: &str) -> Option<String> {
    let lowercase = line.to_ascii_lowercase();
    for level in ["error", "warn", "info", "debug", "trace"] {
        if lowercase.contains(level) {
            return Some(level.to_string());
        }
    }

    None
}

fn normalize_pattern(line: &str) -> String {
    let mut normalized = String::with_capacity(line.len());
    let mut previous_was_digit = false;

    for character in line.chars() {
        if character.is_ascii_digit() {
            if !previous_was_digit {
                normalized.push('#');
            }
            previous_was_digit = true;
        } else {
            previous_was_digit = false;
            normalized.push(character);
        }
    }

    normalized
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
}
