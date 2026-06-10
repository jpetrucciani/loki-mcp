#![allow(dead_code)]

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde_json::{Value, json};

use crate::{
    config::Config,
    loki::client::LokiClient,
    time::{parse_relative_duration, parse_time_reference},
};

type OptionalRange = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

pub struct LabelValuesInput<'a> {
    pub label: &'a str,
    pub start: Option<&'a str>,
    pub end: Option<&'a str>,
    pub range: Option<&'a str>,
    pub query: Option<&'a str>,
    pub prefix: Option<&'a str>,
    pub pattern: Option<&'a str>,
}

pub struct SearchLabelValuesInput<'a> {
    pub labels: Option<&'a [String]>,
    pub start: Option<&'a str>,
    pub end: Option<&'a str>,
    pub range: Option<&'a str>,
    pub query: Option<&'a str>,
    pub prefix: Option<&'a str>,
    pub pattern: Option<&'a str>,
    pub limit_per_label: Option<usize>,
}

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
    range: Option<&str>,
) -> Result<Value> {
    let (start_time, end_time) = parse_optional_range(start, end, range, timezone)?;
    let labels = client.labels(start_time, end_time).await?;

    Ok(json!({ "labels": labels }))
}

pub async fn label_values(
    client: &LokiClient,
    timezone: Tz,
    input: LabelValuesInput<'_>,
) -> Result<Value> {
    let (start_time, end_time) =
        parse_optional_range(input.start, input.end, input.range, timezone)?;
    let values = client
        .label_values(input.label, start_time, end_time, input.query)
        .await?;
    let values = filter_label_values(values, input.prefix, input.pattern)?;

    Ok(json!({
        "label": input.label,
        "values": values,
    }))
}

pub async fn search_label_values(
    client: &LokiClient,
    timezone: Tz,
    input: SearchLabelValuesInput<'_>,
) -> Result<Value> {
    require_filter(input.prefix, input.pattern)?;
    let limit_per_label = input.limit_per_label.unwrap_or(100);
    if limit_per_label == 0 {
        bail!("limit_per_label must be greater than zero");
    }

    let (start_time, end_time) =
        parse_optional_range(input.start, input.end, input.range, timezone)?;
    let candidate_labels = match input.labels {
        Some([]) => bail!("labels must not be empty when provided"),
        Some(labels) => labels.to_vec(),
        None => client.labels(start_time, end_time).await?,
    };

    let mut matches = Vec::new();
    let mut total_matches = 0_usize;
    for label in &candidate_labels {
        let values = client
            .label_values(label, start_time, end_time, input.query)
            .await?;
        let mut filtered = filter_label_values(values, input.prefix, input.pattern)?;
        let match_count = filtered.len();
        total_matches += match_count;

        if match_count == 0 {
            continue;
        }

        let truncated = filtered.len() > limit_per_label;
        if truncated {
            filtered.truncate(limit_per_label);
        }

        matches.push(json!({
            "label": label,
            "values": filtered,
            "match_count": match_count,
            "truncated": truncated,
        }));
    }

    Ok(json!({
        "labels_searched": candidate_labels.len(),
        "total_matches": total_matches,
        "limit_per_label": limit_per_label,
        "matches": matches,
    }))
}

pub async fn series(
    client: &LokiClient,
    timezone: Tz,
    matches: &[String],
    start: Option<&str>,
    end: Option<&str>,
    range: Option<&str>,
) -> Result<Value> {
    let (start_time, end_time) = parse_optional_range(start, end, range, timezone)?;
    let series = client.series(matches, start_time, end_time).await?;

    Ok(json!({ "series": series }))
}

fn parse_optional_range(
    start: Option<&str>,
    end: Option<&str>,
    range: Option<&str>,
    timezone: Tz,
) -> Result<OptionalRange> {
    if start.is_some() && range.is_some() {
        bail!("range cannot be combined with start");
    }

    let now = Utc::now();

    let end_time = end
        .map(|value| parse_time_reference(value, timezone, now))
        .transpose()?;

    let anchor = end_time.unwrap_or(now);
    let start_time = match (start, range) {
        (Some(value), None) => Some(parse_time_reference(value, timezone, anchor)?),
        (None, Some(value)) => Some(anchor - parse_relative_duration(value)?),
        (None, None) => None,
        (Some(_), Some(_)) => unreachable!("validated above"),
    };

    if let (Some(start_time), Some(end_time)) = (start_time, end_time)
        && start_time > end_time
    {
        bail!("start time must be less than or equal to end time");
    }

    Ok((start_time, end_time))
}

fn filter_label_values(
    values: Vec<String>,
    prefix: Option<&str>,
    pattern: Option<&str>,
) -> Result<Vec<String>> {
    let prefix = normalize_filter(prefix, "prefix")?;
    let pattern = normalize_filter(pattern, "pattern")?;

    Ok(values
        .into_iter()
        .filter(|value| {
            prefix.is_none_or(|prefix| value.starts_with(prefix))
                && pattern.is_none_or(|pattern| value.contains(pattern))
        })
        .collect())
}

fn require_filter(prefix: Option<&str>, pattern: Option<&str>) -> Result<()> {
    if normalize_filter(prefix, "prefix")?.is_some()
        || normalize_filter(pattern, "pattern")?.is_some()
    {
        return Ok(());
    }

    bail!("either prefix or pattern must be provided");
}

fn normalize_filter<'a>(value: Option<&'a str>, name: &str) -> Result<Option<&'a str>> {
    let Some(value) = value.map(str::trim) else {
        return Ok(None);
    };

    if value.is_empty() {
        bail!("{name} must not be empty when provided");
    }

    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use crate::tools::discovery::filter_label_values;

    #[test]
    fn filters_label_values_by_prefix_and_pattern() {
        let values = vec![
            "bot-git-tags".to_string(),
            "bot-hourly".to_string(),
            "api".to_string(),
            "bot-deploy-tags".to_string(),
        ];

        let filtered = filter_label_values(values, Some("bot-"), Some("tags")).expect("valid");

        assert_eq!(filtered, vec!["bot-git-tags", "bot-deploy-tags"]);
    }
}
