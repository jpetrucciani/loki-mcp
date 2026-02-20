# loki-mcp

[![build](https://github.com/jpetrucciani/loki-mcp/actions/workflows/build.yml/badge.svg)](https://github.com/jpetrucciani/loki-mcp/actions/workflows/build.yml)
[![release](https://github.com/jpetrucciani/loki-mcp/actions/workflows/release.yml/badge.svg)](https://github.com/jpetrucciani/loki-mcp/actions/workflows/release.yml)
[![license](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE)
[![uses nix](https://img.shields.io/badge/uses-nix-%237EBAE4)](https://nixos.org/)
![rust](https://img.shields.io/badge/Rust-1.95%2B-orange.svg)

`loki-mcp` is a Model Context Protocol (MCP) server for querying Grafana Loki, built for AI agents and automation.

## Features

- 15 read-only MCP tools for discovery, querying, analysis, and health checks
- Config layering with validation: `TOML -> env -> CLI`
- Loki auth modes: `none`, `basic`, `bearer`
- Guardrails for bytes/streams limits with fail-closed behavior
- Per-tool and per-identity rate limiting
- Response modes for large result sets: `raw`, `truncated`, `summary`, `smart`
- Built-in observability: `/healthz`, `/readyz`, `/metrics`, request ids, recent action tracking
- CI coverage for test/build/format/clippy, plus tagged release automation

## MCP Tool Surface

Discovery:

- `loki_describe_schema`
- `loki_list_labels`
- `loki_label_values`
- `loki_series`

Query and execution:

- `loki_query_logs`
- `loki_query_metrics`
- `loki_build_query`
- `loki_tail`
- `loki_run_saved_query`
- `loki_query_stats`

Analysis and authoring:

- `loki_detect_patterns`
- `loki_compare_ranges`
- `loki_explain_query`
- `loki_suggest_metric_rule`

Utility:

- `loki_check_health`

## Installation

### Option 1: Release binaries

Download the archive for your platform from [GitHub Releases](https://github.com/jpetrucciani/loki-mcp/releases), extract it, and place `loki-mcp` on your `PATH`.

### Option 2: Build from source

```bash
cargo build --release
./target/release/loki-mcp --help
```

### Option 3: Container image

Tagged releases publish multi-arch images to GHCR:

```bash
docker run --rm -p 8080:8080 \
  -v "$PWD/config.toml:/config.toml:ro" \
  ghcr.io/jpetrucciani/loki-mcp:vX.Y.Z \
  --config /config.toml
```

## Quickstart

This section assumes `loki-mcp` is on your `PATH`. If not, use `./target/release/loki-mcp`.

1. Copy the example config:

```bash
cp config.example.toml config.toml
```

2. Edit `config.toml` and set `loki.url` and auth fields for your environment.
3. Start the server:

```bash
loki-mcp --config config.toml
```

4. Verify local endpoints:

```bash
curl -fsS http://127.0.0.1:8080/healthz
curl -fsS http://127.0.0.1:8080/readyz
curl -fsS http://127.0.0.1:8080/metrics | head
```

MCP transport endpoint: `http://127.0.0.1:8080/mcp`

## Configuration

`config.example.toml` is the reference template.

Precedence (lowest to highest):

1. TOML file
2. Environment variables
3. CLI flags

Examples:

```bash
# explicit config path
loki-mcp --config /etc/loki-mcp/config.toml

# config path via env
LOKI_MCP_CONFIG=/etc/loki-mcp/config.toml loki-mcp

# CLI override
loki-mcp --config config.toml --listen 0.0.0.0:8080 --loki-url https://loki:3100

# flattened env aliases
LOKI_MCP_LOKI_URL=https://loki:3100 LOKI_MCP_LISTEN=0.0.0.0:8080 loki-mcp

# nested env form (double underscore)
LOKI_MCP_LOKI__URL=https://loki:3100 LOKI_MCP_SERVER__LISTEN=0.0.0.0:8080 loki-mcp
```

Common env keys:

- `LOKI_MCP_LISTEN`
- `LOKI_MCP_TIMEZONE`
- `LOKI_MCP_LOG_LEVEL`
- `LOKI_MCP_IDENTITY_HEADER`
- `LOKI_MCP_LOKI_URL`
- `LOKI_MCP_LOKI_TENANT_ID`
- `LOKI_MCP_LOKI_AUTH_TYPE`
- `LOKI_MCP_LOKI_USERNAME`
- `LOKI_MCP_LOKI_PASSWORD`
- `LOKI_MCP_LOKI_TOKEN`
- `LOKI_MCP_LOKI_CA_CERT`
- `LOKI_MCP_RATE_LIMIT_RPS`
- `LOKI_MCP_GUARDRAILS_MAX_BYTES_SCANNED`
- `LOKI_MCP_RECENT_ACTIONS_ENABLED`

## Security and Trust Model

Loki auth:

- `loki.auth_type=none`
- `loki.auth_type=basic` requires `username` and `password`
- `loki.auth_type=bearer` requires `token`

Use environment variables for secrets instead of committing credentials to TOML.

MCP auth:

- Built-in MCP authentication is intentionally disabled in v1.
- Deploy `loki-mcp` behind a trusted reverse proxy/ingress.
- Enforce authN/authZ at the proxy (OIDC/JWT/mTLS).
- Forward an identity header and set `server.identity_header` to match.

Rate limiting identity keys are resolved in this order:

1. configured `identity_header`
2. first hop in `x-forwarded-for`
3. remote IP

## Runtime Behavior

Time handling:

- If `start` and `end` are omitted, default range is last `30m` ending at `now`
- Supported references: RFC3339, durations like `15m`, `now`, `today`, `yesterday`, `since 2pm`

Response modes (`loki_query_logs`, `loki_build_query`, `loki_tail`, `loki_run_saved_query`):

- `raw`, `truncated`, `summary`, `smart` (default)
- `smart` thresholds are `<= 50` lines => `raw`, `51-500` => `truncated`, `> 500` => `summary`

Guardrails:

- Pre-checks query cost via `/loki/api/v1/index/stats`
- Falls back to runtime stats if needed
- Fails closed when estimates are unavailable
- Tuned via `[guardrails]` (`max_bytes_scanned`, `max_streams`, and related skips)

Cache and recent actions:

- In-memory cache controlled by `[cache]`
- `/readyz` result is cached briefly to reduce probe load
- Optional bounded recent action history via `[recent_actions]` (`/debug/recent-actions`)

## HTTP Endpoints

- `GET /healthz`, liveness
- `GET /readyz`, readiness (`200` healthy, `503` unhealthy)
- `GET /metrics`, Prometheus metrics
- `GET` and `POST /mcp`, MCP Streamable HTTP
- `GET /debug/recent-actions?limit=100`, recent tool activity (`404` when disabled)

Every HTTP response includes `x-request-id`.

Metrics use `[metrics].prefix` (default: `loki_mcp`):

- `<prefix>_http_requests_total`
- `<prefix>_tool_calls_total{tool,outcome}`
- `<prefix>_tool_cache_total{tool,result}`
- `<prefix>_tool_guardrail_rejections_total{tool}`
- `<prefix>_tool_rate_limited_total{tool}`
- `<prefix>_readiness_cache_total{result}`

## Development

Nix-first local shell:

```bash
nix-shell
```

CI parity checks:

```bash
cargo fmt -- --check
cargo clippy --all --benches --tests --examples --all-features -- -D warnings -W clippy::collapsible_else_if
cargo test --verbose
```

Integration tests require a local `loki` binary on `PATH`.

## Troubleshooting

- `guardrail pre-check failed ...`, Loki could not provide cost estimates, narrow selector/range or adjust guardrails
- `query rejected by guardrail ...`, query exceeded configured bytes/streams limits
- `rate limit exceeded ...`, increase `[rate_limit]` limits or configure a stronger `identity_header`
- `loki process did not become ready` in tests, verify `loki --version` and loopback port availability
- `loki_check_health` reports `/ready` 404, often expected behind gateways/proxies when other Loki APIs are reachable
- TLS failures against Loki, set `loki.ca_cert` for private CAs
- `/debug/recent-actions` returns 404, set `[recent_actions].enabled=true`
