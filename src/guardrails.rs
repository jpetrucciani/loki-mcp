#![allow(dead_code)]

use anyhow::{Result, anyhow, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardrailDecision {
    Allow,
    RejectBytes,
    RejectStreams,
}

pub fn evaluate(
    estimated_bytes_scanned: u64,
    estimated_streams: u64,
    max_bytes_scanned: Option<u64>,
    max_streams: Option<u64>,
) -> GuardrailDecision {
    if let Some(limit) = max_bytes_scanned
        && limit > 0
        && estimated_bytes_scanned > limit
    {
        return GuardrailDecision::RejectBytes;
    }

    if let Some(limit) = max_streams
        && limit > 0
        && estimated_streams > limit
    {
        return GuardrailDecision::RejectStreams;
    }

    GuardrailDecision::Allow
}

pub fn parse_byte_size(input: &str) -> Result<u64> {
    let compact = input
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect::<String>();
    if compact.is_empty() {
        bail!("size must not be empty");
    }

    let split_index = compact
        .char_indices()
        .find_map(|(index, character)| {
            if character.is_ascii_digit() {
                None
            } else {
                Some(index)
            }
        })
        .unwrap_or(compact.len());

    let value_text = &compact[..split_index];
    let unit_text = compact[split_index..].to_ascii_uppercase();

    if value_text.is_empty() {
        bail!("size is missing numeric value");
    }

    let value = value_text
        .parse::<u64>()
        .map_err(|error| anyhow!("invalid numeric size value: {value_text} ({error})"))?;

    let multiplier: u64 = match unit_text.as_str() {
        "" | "B" => 1,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        "KIB" => 1_024,
        "MIB" => 1_048_576,
        "GIB" => 1_073_741_824,
        "TIB" => 1_099_511_627_776,
        _ => bail!("unsupported byte size unit: {unit_text}"),
    };

    value
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("byte size is too large"))
}

#[cfg(test)]
mod tests {
    use crate::guardrails::{GuardrailDecision, evaluate, parse_byte_size};

    #[test]
    fn rejects_on_bytes_limit() {
        let result = evaluate(100, 10, Some(50), Some(100));
        assert_eq!(result, GuardrailDecision::RejectBytes);
    }

    #[test]
    fn rejects_on_stream_limit() {
        let result = evaluate(100, 101, Some(200), Some(100));
        assert_eq!(result, GuardrailDecision::RejectStreams);
    }

    #[test]
    fn parses_decimal_and_binary_byte_sizes() {
        assert_eq!(parse_byte_size("500MB").expect("valid"), 500_000_000);
        assert_eq!(parse_byte_size("2GiB").expect("valid"), 2_147_483_648);
    }
}
