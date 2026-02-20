#![allow(dead_code)]

use thiserror::Error;

#[derive(Debug, Error)]
pub enum LokiMcpError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("loki request failed: {0}")]
    Loki(String),
    #[error("tool error: {0}")]
    Tool(String),
    #[error("guardrail rejection: {0}")]
    Guardrail(String),
}
