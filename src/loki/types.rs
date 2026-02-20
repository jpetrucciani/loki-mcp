#![allow(dead_code)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LokiApiResponse<T> {
    pub status: String,
    #[serde(default)]
    pub data: T,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default, rename = "errorType")]
    pub error_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LokiHealth {
    pub healthy: bool,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub build_info: Option<Value>,
    #[serde(default)]
    pub ring_status: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LokiQueryStats {
    #[serde(default)]
    pub bytes_processed: Option<u64>,
    #[serde(default)]
    pub streams: Option<u64>,
    #[serde(default)]
    pub chunks: Option<u64>,
    #[serde(default)]
    pub entries: Option<u64>,
    #[serde(default)]
    pub raw: Value,
}

impl LokiQueryStats {
    pub fn from_value(value: Value) -> Self {
        let bytes_processed = extract_u64(&value, &["bytes", "bytesProcessed", "bytes_processed"]);
        let streams = extract_u64(&value, &["streams", "streamCount", "stream_count"]);
        let chunks = extract_u64(&value, &["chunks", "chunkCount", "chunk_count"]);
        let entries = extract_u64(&value, &["entries", "entryCount", "entry_count"]);

        Self {
            bytes_processed,
            streams,
            chunks,
            entries,
            raw: value,
        }
    }
}

pub type LabelSet = BTreeMap<String, String>;

fn extract_u64(value: &Value, keys: &[&str]) -> Option<u64> {
    if let Some(object) = value.as_object() {
        for key in keys {
            if let Some(found) = object.get(*key)
                && let Some(number) = parse_u64_value(found)
            {
                return Some(number);
            }
        }
    }

    None
}

fn parse_u64_value(value: &Value) -> Option<u64> {
    if let Some(number) = value.as_u64() {
        return Some(number);
    }

    if let Some(number) = value.as_i64() {
        return u64::try_from(number).ok();
    }

    value.as_str().and_then(|text| text.parse::<u64>().ok())
}
