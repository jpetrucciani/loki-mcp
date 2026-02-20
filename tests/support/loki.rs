use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, OnceLock},
    time::{Duration as StdDuration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use tokio::sync::OwnedMutexGuard;
use tokio::time::{Instant, sleep};

pub struct LokiTestHarness {
    base_url: String,
    run_id: String,
    process: Option<Child>,
    temp_root: PathBuf,
    stdout_log_path: PathBuf,
    stderr_log_path: PathBuf,
    stopped: bool,
    _guard: OwnedMutexGuard<()>,
}

impl LokiTestHarness {
    pub async fn start() -> Result<Self> {
        let lock = harness_lock().lock_owned().await;
        ensure_loki_available()?;

        let run_id = generate_run_id()?;
        let temp_root = std::env::temp_dir().join(format!("loki-mcp-it-{run_id}"));
        fs::create_dir_all(&temp_root)
            .with_context(|| format!("failed to create temp test directory {:?}", temp_root))?;
        let stdout_log_path = temp_root.join("loki.stdout.log");
        let stderr_log_path = temp_root.join("loki.stderr.log");

        let http_port = allocate_local_port()?;
        let grpc_port = allocate_local_port()?;
        let config_path = write_loki_config(&temp_root, http_port, grpc_port)?;
        let process = spawn_loki_process(&config_path, &stdout_log_path, &stderr_log_path)?;
        let base_url = format!("http://127.0.0.1:{http_port}");

        let harness = Self {
            base_url,
            run_id,
            process: Some(process),
            temp_root,
            stdout_log_path,
            stderr_log_path,
            stopped: false,
            _guard: lock,
        };

        harness
            .wait_until_ready(StdDuration::from_secs(60))
            .await
            .context("loki process did not become ready")?;

        Ok(harness)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn scoped_selector(&self) -> String {
        format!("{{test_run_id=\"{}\"}}", self.run_id)
    }

    pub async fn seed_example_logs(&self) -> Result<()> {
        let now = Utc::now();
        let stream_a_values = vec![
            log_value(
                now - Duration::seconds(90),
                "level=error msg=\"failed login\" user=42",
            ),
            log_value(
                now - Duration::seconds(70),
                "level=warn msg=\"slow request\" duration_ms=285",
            ),
            log_value(
                now - Duration::seconds(50),
                "level=info msg=\"request completed\" status=200",
            ),
        ];
        let stream_b_values = vec![
            log_value(
                now - Duration::seconds(85),
                "level=error msg=\"db timeout\" retry=1",
            ),
            log_value(
                now - Duration::seconds(65),
                "level=info msg=\"retry success\"",
            ),
            log_value(
                now - Duration::seconds(45),
                "level=debug msg=\"cache miss\" key=profile:42",
            ),
        ];

        let payload = json!({
            "streams": [
                {
                    "stream": {
                        "app": "api",
                        "namespace": "production",
                        "level": "error",
                        "pod": "api-0",
                        "test_run_id": self.run_id,
                    },
                    "values": stream_a_values,
                },
                {
                    "stream": {
                        "app": "worker",
                        "namespace": "production",
                        "level": "info",
                        "pod": "worker-0",
                        "test_run_id": self.run_id,
                    },
                    "values": stream_b_values,
                },
            ]
        });

        let client = reqwest::Client::new();
        let response = client
            .post(format!("{}/loki/api/v1/push", self.base_url))
            .json(&payload)
            .send()
            .await
            .context("failed to push seed logs")?;
        if !response.status().is_success() {
            bail!(
                "failed to push seed logs: status={} body={}",
                response.status(),
                response.text().await.unwrap_or_default()
            );
        }

        self.wait_until_queryable(&self.scoped_selector(), StdDuration::from_secs(30))
            .await
            .context("seeded logs did not become queryable in time")
    }

    #[allow(dead_code)]
    pub fn stop(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }

        if let Some(process) = self.process.as_mut() {
            let _ = process.kill();
            let _ = process.wait();
        }

        self.stopped = true;
        cleanup_temp_dir(&self.temp_root);
        Ok(())
    }

    async fn wait_until_ready(&self, timeout: StdDuration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let client = reqwest::Client::new();

        loop {
            if Instant::now() > deadline {
                let stderr_tail = read_log_tail(&self.stderr_log_path, 80);
                let stdout_tail = read_log_tail(&self.stdout_log_path, 40);
                bail!(
                    "timed out waiting for Loki readiness\nstderr tail:\n{stderr_tail}\nstdout tail:\n{stdout_tail}"
                );
            }

            if let Ok(response) = client.get(format!("{}/ready", self.base_url)).send().await
                && response.status().is_success()
            {
                return Ok(());
            }

            sleep(StdDuration::from_millis(250)).await;
        }
    }

    async fn wait_until_queryable(&self, query: &str, timeout: StdDuration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let client = reqwest::Client::new();

        loop {
            if Instant::now() > deadline {
                bail!("timed out waiting for query results");
            }

            let end = Utc::now();
            let start = end - Duration::minutes(30);
            let request = client
                .get(format!("{}/loki/api/v1/query_range", self.base_url))
                .query(&[
                    ("query", query.to_string()),
                    ("start", timestamp_nanos(start)?),
                    ("end", timestamp_nanos(end)?),
                    ("limit", "100".to_string()),
                ]);

            if let Ok(response) = request.send().await
                && response.status().is_success()
            {
                let body: Value = response
                    .json()
                    .await
                    .context("failed to decode query_range response")?;

                let has_results = body
                    .get("data")
                    .and_then(|value| value.get("result"))
                    .and_then(Value::as_array)
                    .map(|entries| !entries.is_empty())
                    .unwrap_or(false);

                if has_results {
                    return Ok(());
                }
            }

            sleep(StdDuration::from_millis(250)).await;
        }
    }
}

impl Drop for LokiTestHarness {
    fn drop(&mut self) {
        if !self.stopped {
            if let Some(process) = self.process.as_mut() {
                let _ = process.kill();
                let _ = process.wait();
            }
            self.stopped = true;
        }
        cleanup_temp_dir(&self.temp_root);
    }
}

fn ensure_loki_available() -> Result<()> {
    let status = Command::new("loki")
        .arg("--version")
        .status()
        .context("failed to execute loki binary from PATH")?;

    if !status.success() {
        bail!(
            "loki binary is required for integration tests, add grafana-loki to your PATH (for example via `nix-shell`)"
        );
    }

    Ok(())
}

fn harness_lock() -> Arc<tokio::sync::Mutex<()>> {
    static LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

fn allocate_local_port() -> Result<u16> {
    let listener =
        TcpListener::bind("127.0.0.1:0").context("failed to allocate a local TCP port")?;
    let port = listener
        .local_addr()
        .context("failed to read allocated local address")?
        .port();
    drop(listener);

    Ok(port)
}

fn spawn_loki_process(
    config_path: &Path,
    stdout_log_path: &Path,
    stderr_log_path: &Path,
) -> Result<Child> {
    let stdout_log = fs::File::create(stdout_log_path)
        .with_context(|| format!("failed to create stdout log {:?}", stdout_log_path))?;
    let stderr_log = fs::File::create(stderr_log_path)
        .with_context(|| format!("failed to create stderr log {:?}", stderr_log_path))?;

    Command::new("loki")
        .arg(format!("-config.file={}", config_path.display()))
        .stdout(Stdio::from(stdout_log))
        .stderr(Stdio::from(stderr_log))
        .spawn()
        .context("failed to spawn loki process")
}

fn write_loki_config(temp_root: &Path, http_port: u16, grpc_port: u16) -> Result<PathBuf> {
    let path_prefix = temp_root.join("loki-data");
    let chunks_dir = path_prefix.join("chunks");
    let rules_dir = path_prefix.join("rules");
    let index_dir = path_prefix.join("index");
    let cache_dir = path_prefix.join("index-cache");
    let compactor_dir = path_prefix.join("compactor");
    fs::create_dir_all(&chunks_dir).with_context(|| format!("failed creating {:?}", chunks_dir))?;
    fs::create_dir_all(&rules_dir).with_context(|| format!("failed creating {:?}", rules_dir))?;
    fs::create_dir_all(&index_dir).with_context(|| format!("failed creating {:?}", index_dir))?;
    fs::create_dir_all(&cache_dir).with_context(|| format!("failed creating {:?}", cache_dir))?;
    fs::create_dir_all(&compactor_dir)
        .with_context(|| format!("failed creating {:?}", compactor_dir))?;

    let config_contents = format!(
        r#"auth_enabled: false

server:
  http_listen_address: 127.0.0.1
  http_listen_port: {http_port}
  grpc_listen_address: 127.0.0.1
  grpc_listen_port: {grpc_port}
  log_level: error

common:
  instance_addr: 127.0.0.1
  path_prefix: {path_prefix}
  replication_factor: 1
  ring:
    kvstore:
      store: inmemory
  storage:
    filesystem:
      chunks_directory: {chunks_dir}
      rules_directory: {rules_dir}

schema_config:
  configs:
    - from: 2024-01-01
      store: tsdb
      object_store: filesystem
      schema: v13
      index:
        prefix: index_
        period: 24h

storage_config:
  filesystem:
    directory: {chunks_dir}
  tsdb_shipper:
    active_index_directory: {index_dir}
    cache_location: {cache_dir}

compactor:
  working_directory: {compactor_dir}

ruler:
  alertmanager_url: http://127.0.0.1:9093

limits_config:
  reject_old_samples: false
  reject_old_samples_max_age: 168h

analytics:
  reporting_enabled: false
"#,
        http_port = http_port,
        grpc_port = grpc_port,
        path_prefix = path_prefix.display(),
        chunks_dir = chunks_dir.display(),
        rules_dir = rules_dir.display(),
        index_dir = index_dir.display(),
        cache_dir = cache_dir.display(),
        compactor_dir = compactor_dir.display(),
    );

    let config_path = temp_root.join("loki-config.yaml");
    fs::write(&config_path, config_contents)
        .with_context(|| format!("failed to write loki config at {:?}", config_path))?;

    Ok(config_path)
}

fn cleanup_temp_dir(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

fn read_log_tail(path: &Path, max_lines: usize) -> String {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) => return format!("<failed to read {}: {error}>", path.display()),
    };

    let mut lines = content.lines().rev().take(max_lines).collect::<Vec<_>>();
    lines.reverse();

    if lines.is_empty() {
        return "<empty>".to_string();
    }

    lines.join("\n")
}

fn generate_run_id() -> Result<String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| anyhow!("system clock error: {error}"))?
        .as_nanos();
    Ok(format!("it-{nanos}"))
}

fn timestamp_nanos(timestamp: chrono::DateTime<Utc>) -> Result<String> {
    let nanos = timestamp
        .timestamp_nanos_opt()
        .ok_or_else(|| anyhow!("timestamp is out of range for nanoseconds"))?;
    Ok(nanos.to_string())
}

fn log_value(timestamp: chrono::DateTime<Utc>, line: &str) -> Value {
    json!([
        timestamp_nanos(timestamp).expect("timestamp must be valid"),
        line
    ])
}
