#![allow(dead_code)]

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use reqwest::{Client, Method, RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    config::LokiConfig,
    loki::{
        auth::LokiAuth,
        types::{LokiApiResponse, LokiHealth, LokiQueryStats},
    },
    time::parse_std_duration,
};

#[derive(Clone)]
pub struct LokiClient {
    client: Client,
    base_url: String,
    tenant_id: Option<String>,
    auth: LokiAuth,
}

impl LokiClient {
    pub fn new(config: &LokiConfig) -> Result<Self> {
        let timeout = parse_std_duration(&config.timeout)
            .with_context(|| format!("invalid loki.timeout: {}", config.timeout))?;

        let mut builder = Client::builder()
            .timeout(timeout)
            .user_agent("loki-mcp/0.1.0");

        if let Some(ca_cert_path) = config.ca_cert.as_deref() {
            let certificate_bytes = std::fs::read(ca_cert_path)
                .with_context(|| format!("failed to read CA certificate from {ca_cert_path}"))?;
            let certificate = reqwest::Certificate::from_pem(&certificate_bytes)
                .with_context(|| format!("invalid PEM CA certificate at {ca_cert_path}"))?;
            builder = builder.add_root_certificate(certificate);
        }

        let client = builder
            .build()
            .context("failed to build Loki HTTP client")?;

        let auth = LokiAuth::from_config(config)?;

        Ok(Self {
            client,
            base_url: config.url.trim_end_matches('/').to_string(),
            tenant_id: config.tenant_id.clone(),
            auth,
        })
    }

    pub async fn check_health(&self) -> Result<LokiHealth> {
        let readiness = self.probe_readiness().await;
        let build_info = self
            .get_optional_json("/loki/api/v1/status/buildinfo")
            .await;
        let ring_status = self.get_optional_json("/distributor/ring").await;
        let api_reachable = match readiness {
            ReadinessProbe::Status(status) if status == StatusCode::NOT_FOUND => {
                build_info.is_some() || self.is_loki_api_reachable().await
            }
            _ => false,
        };
        let (healthy, message) = evaluate_health(readiness, api_reachable);

        Ok(LokiHealth {
            healthy,
            message,
            build_info,
            ring_status,
        })
    }

    pub async fn labels(
        &self,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
    ) -> Result<Vec<String>> {
        let mut params = Vec::new();
        append_time_range(&mut params, start, end)?;

        let request = self
            .request(Method::GET, "/loki/api/v1/labels")
            .query(&params);
        self.send_api_data(request).await
    }

    pub async fn label_values(
        &self,
        label: &str,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        query: Option<&str>,
    ) -> Result<Vec<String>> {
        validate_label_name(label)?;

        let mut params = Vec::new();
        append_time_range(&mut params, start, end)?;
        if let Some(selector) = query {
            params.push(("query".to_string(), selector.to_string()));
        }

        let path = format!("/loki/api/v1/label/{label}/values");
        let request = self.request(Method::GET, &path).query(&params);
        self.send_api_data(request).await
    }

    pub async fn series(
        &self,
        matches: &[String],
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
    ) -> Result<Vec<Value>> {
        if matches.is_empty() {
            bail!("at least one series matcher is required");
        }

        let mut params = Vec::new();
        for matcher in matches {
            params.push(("match[]".to_string(), matcher.to_string()));
        }
        append_time_range(&mut params, start, end)?;

        let request = self
            .request(Method::GET, "/loki/api/v1/series")
            .query(&params);
        self.send_api_data(request).await
    }

    pub async fn query_logs(
        &self,
        query: &str,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        limit: Option<u32>,
        direction: Option<&str>,
    ) -> Result<Value> {
        let mut params = vec![("query".to_string(), query.to_string())];
        append_time_range(&mut params, start, end)?;

        if let Some(limit) = limit {
            params.push(("limit".to_string(), limit.to_string()));
        }

        if let Some(direction) = direction {
            params.push(("direction".to_string(), direction.to_string()));
        }

        let request = self
            .request(Method::GET, "/loki/api/v1/query_range")
            .query(&params);
        self.send_api_data(request).await
    }

    pub async fn query_metrics(
        &self,
        query: &str,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        step: Option<&str>,
    ) -> Result<Value> {
        let mut params = vec![("query".to_string(), query.to_string())];
        append_time_range(&mut params, start, end)?;

        if let Some(step) = step {
            params.push(("step".to_string(), step.to_string()));
        }

        let request = self
            .request(Method::GET, "/loki/api/v1/query_range")
            .query(&params);
        self.send_api_data(request).await
    }

    pub async fn query_stats(
        &self,
        query: &str,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
    ) -> Result<LokiQueryStats> {
        let mut params = vec![("query".to_string(), query.to_string())];
        append_time_range(&mut params, start, end)?;

        let request = self
            .request(Method::GET, "/loki/api/v1/index/stats")
            .query(&params);

        let data = self.send_api_data_or_raw(request).await?;
        Ok(LokiQueryStats::from_value(data))
    }

    pub async fn detect_patterns(
        &self,
        query: &str,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        step: Option<&str>,
    ) -> Result<Value> {
        let mut params = vec![("query".to_string(), query.to_string())];
        append_time_range(&mut params, start, end)?;

        if let Some(step) = step {
            params.push(("step".to_string(), step.to_string()));
        }

        let request = self
            .request(Method::GET, "/loki/api/v1/patterns")
            .query(&params);
        self.send_api_data(request).await
    }

    pub async fn query_runtime_stats(
        &self,
        query: &str,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
    ) -> Result<LokiQueryStats> {
        let data = self
            .query_logs(query, start, end, Some(1), Some("backward"))
            .await?;
        let summary = data
            .get("stats")
            .and_then(|value| value.get("summary"))
            .cloned()
            .unwrap_or(Value::Null);

        let bytes_processed = value_at_path_u64(&summary, &["totalBytesProcessed"]);
        let streams = data
            .get("result")
            .and_then(Value::as_array)
            .map(|entries| entries.len() as u64);
        let entries = value_at_path_u64(&summary, &["totalLinesProcessed"]);
        let chunks = value_at_path_u64(&summary, &["totalChunksMatched"]);
        let raw = data.get("stats").cloned().unwrap_or(data);

        Ok(LokiQueryStats {
            bytes_processed,
            streams,
            chunks,
            entries,
            raw,
        })
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let endpoint = format!("{}{}", self.base_url, path);
        let builder = self.client.request(method, endpoint);

        let builder = if let Some(tenant_id) = self.tenant_id.as_deref() {
            builder.header("X-Scope-OrgID", tenant_id)
        } else {
            builder
        };

        self.auth.apply(builder)
    }

    async fn send_json<T: DeserializeOwned>(&self, builder: RequestBuilder) -> Result<T> {
        let response = builder.send().await.context("request to Loki failed")?;

        let response = response
            .error_for_status()
            .context("Loki returned non-success status")?;

        response
            .json::<T>()
            .await
            .context("failed to decode Loki JSON response")
    }

    async fn send_api_data<T: DeserializeOwned + Default>(
        &self,
        builder: RequestBuilder,
    ) -> Result<T> {
        let envelope: LokiApiResponse<T> = self.send_json(builder).await?;

        if envelope.status == "success" {
            return Ok(envelope.data);
        }

        let error_type = envelope
            .error_type
            .unwrap_or_else(|| "unknown_error".to_string());
        let message = envelope
            .error
            .unwrap_or_else(|| "Loki returned an error response".to_string());
        bail!("loki api error ({error_type}): {message}")
    }

    async fn send_api_data_or_raw(&self, builder: RequestBuilder) -> Result<Value> {
        let value: Value = self.send_json(builder).await?;

        let Some(status) = value.get("status").and_then(Value::as_str) else {
            // Loki index stats may return a raw payload without the normal status/data envelope.
            return Ok(value);
        };

        if status == "success" {
            return Ok(value.get("data").cloned().unwrap_or(Value::Null));
        }

        let error_type = value
            .get("errorType")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error");
        let message = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Loki returned an error response");
        bail!("loki api error ({error_type}): {message}")
    }

    async fn get_optional_json(&self, path: &str) -> Option<Value> {
        let response = self.request(Method::GET, path).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }

        response.json::<Value>().await.ok()
    }

    async fn probe_readiness(&self) -> ReadinessProbe {
        match self.request(Method::GET, "/ready").send().await {
            Ok(response) if response.status().is_success() => ReadinessProbe::Ready,
            Ok(response) => ReadinessProbe::Status(response.status()),
            Err(error) => ReadinessProbe::Error(error.to_string()),
        }
    }

    async fn is_loki_api_reachable(&self) -> bool {
        match self
            .request(Method::GET, "/loki/api/v1/labels")
            .send()
            .await
        {
            Ok(response) => response.status().is_success(),
            Err(_) => false,
        }
    }
}

#[derive(Debug)]
enum ReadinessProbe {
    Ready,
    Status(StatusCode),
    Error(String),
}

fn evaluate_health(readiness: ReadinessProbe, api_reachable: bool) -> (bool, Option<String>) {
    match readiness {
        ReadinessProbe::Ready => (true, None),
        ReadinessProbe::Status(status) if status == StatusCode::NOT_FOUND && api_reachable => (
            true,
            Some(format!(
                "loki /ready returned status {status}; Loki API endpoints are reachable"
            )),
        ),
        ReadinessProbe::Status(status) => (false, Some(format!("loki returned status {status}"))),
        ReadinessProbe::Error(error) => (false, Some(error)),
    }
}

fn append_time_range(
    params: &mut Vec<(String, String)>,
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
) -> Result<()> {
    if let Some(start) = start {
        params.push(("start".to_string(), timestamp_nanos(start)?));
    }

    if let Some(end) = end {
        params.push(("end".to_string(), timestamp_nanos(end)?));
    }

    Ok(())
}

fn timestamp_nanos(value: DateTime<Utc>) -> Result<String> {
    let nanos = value
        .timestamp_nanos_opt()
        .ok_or_else(|| anyhow!("timestamp is out of range for nanoseconds"))?;

    Ok(nanos.to_string())
}

fn validate_label_name(label: &str) -> Result<()> {
    if label.is_empty() {
        bail!("label must not be empty");
    }

    if label
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == ':')
    {
        Ok(())
    } else {
        bail!("label contains unsupported characters: {label}")
    }
}

fn value_at_path_u64(value: &Value, path: &[&str]) -> Option<u64> {
    let mut cursor = value;
    for key in path {
        cursor = cursor.get(*key)?;
    }

    if let Some(number) = cursor.as_u64() {
        return Some(number);
    }
    if let Some(number) = cursor.as_i64() {
        return u64::try_from(number).ok();
    }

    cursor.as_str().and_then(|text| text.parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;

    use crate::loki::client::{ReadinessProbe, evaluate_health};

    #[test]
    fn health_is_true_when_ready_endpoint_succeeds() {
        let (healthy, message) = evaluate_health(ReadinessProbe::Ready, false);
        assert!(healthy);
        assert!(message.is_none());
    }

    #[test]
    fn health_is_true_when_ready_is_404_but_api_is_reachable() {
        let (healthy, message) =
            evaluate_health(ReadinessProbe::Status(StatusCode::NOT_FOUND), true);
        assert!(healthy);
        assert!(
            message
                .as_deref()
                .unwrap_or_default()
                .contains("Loki API endpoints are reachable")
        );
    }

    #[test]
    fn health_is_false_when_ready_is_404_and_api_is_not_reachable() {
        let (healthy, message) =
            evaluate_health(ReadinessProbe::Status(StatusCode::NOT_FOUND), false);
        assert!(!healthy);
        assert_eq!(
            message.as_deref(),
            Some("loki returned status 404 Not Found"),
        );
    }

    #[test]
    fn health_is_false_when_ready_returns_server_error() {
        let (healthy, message) = evaluate_health(
            ReadinessProbe::Status(StatusCode::SERVICE_UNAVAILABLE),
            true,
        );
        assert!(!healthy);
        assert_eq!(
            message.as_deref(),
            Some("loki returned status 503 Service Unavailable"),
        );
    }
}
