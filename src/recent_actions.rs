use std::{collections::VecDeque, sync::Arc, time::Duration as StdDuration};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutcome {
    Success,
    Error,
    RateLimited,
    GuardrailReject,
    InvalidTool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecentAction {
    pub timestamp: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub tool: String,
    pub outcome: ActionOutcome,
    pub duration_ms: u64,
    pub identity_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    pub query_redacted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RecentActionInput {
    pub request_id: Option<String>,
    pub tool: String,
    pub outcome: ActionOutcome,
    pub duration_ms: u64,
    pub identity_hash: String,
    pub tenant_id: Option<String>,
    pub query: Option<String>,
    pub error_class: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct RecentActionsStore {
    max_entries: usize,
    ttl: StdDuration,
    store_query_text: bool,
    store_error_text: bool,
    entries: Arc<RwLock<VecDeque<RecentAction>>>,
}

impl RecentActionsStore {
    pub fn new(
        max_entries: u64,
        ttl: StdDuration,
        store_query_text: bool,
        store_error_text: bool,
    ) -> Self {
        let bounded_max_entries = if max_entries == 0 { 1 } else { max_entries };
        let max_entries = usize::try_from(bounded_max_entries).unwrap_or(usize::MAX);

        Self {
            max_entries,
            ttl,
            store_query_text,
            store_error_text,
            entries: Arc::new(RwLock::new(VecDeque::new())),
        }
    }

    pub async fn record(&self, input: RecentActionInput) {
        let now = Utc::now();
        let mut entries = self.entries.write().await;
        prune_expired(&mut entries, now, self.ttl);

        while entries.len() >= self.max_entries {
            let _ = entries.pop_front();
        }

        let query_redacted = input.query.is_some() && !self.store_query_text;
        let query = if self.store_query_text {
            input.query
        } else {
            None
        };
        let error = if self.store_error_text {
            input.error
        } else {
            None
        };

        entries.push_back(RecentAction {
            timestamp: now,
            request_id: input.request_id,
            tool: input.tool,
            outcome: input.outcome,
            duration_ms: input.duration_ms,
            identity_hash: input.identity_hash,
            tenant_id: input.tenant_id,
            query,
            query_redacted,
            error_class: input.error_class,
            error,
        });
    }

    pub async fn list(&self, limit: usize) -> Vec<RecentAction> {
        let bounded_limit = limit.clamp(1, 1000);
        let now = Utc::now();
        let mut entries = self.entries.write().await;
        prune_expired(&mut entries, now, self.ttl);

        entries
            .iter()
            .rev()
            .take(bounded_limit)
            .cloned()
            .collect::<Vec<RecentAction>>()
    }
}

fn prune_expired(entries: &mut VecDeque<RecentAction>, now: DateTime<Utc>, ttl: StdDuration) {
    let ttl = match Duration::from_std(ttl) {
        Ok(value) => value,
        Err(_) => return,
    };

    while let Some(front) = entries.front() {
        if now.signed_duration_since(front.timestamp) > ttl {
            let _ = entries.pop_front();
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration as StdDuration;

    use crate::recent_actions::{ActionOutcome, RecentActionInput, RecentActionsStore};

    fn action(tool: &str) -> RecentActionInput {
        RecentActionInput {
            request_id: Some("req-1".to_string()),
            tool: tool.to_string(),
            outcome: ActionOutcome::Success,
            duration_ms: 12,
            identity_hash: "hash".to_string(),
            tenant_id: None,
            query: Some("{app=\"api\"}".to_string()),
            error_class: None,
            error: None,
        }
    }

    #[tokio::test]
    async fn keeps_most_recent_entries_with_max_capacity() {
        let store = RecentActionsStore::new(2, StdDuration::from_secs(60), false, false);
        store.record(action("a")).await;
        store.record(action("b")).await;
        store.record(action("c")).await;

        let entries = store.list(10).await;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].tool, "c");
        assert_eq!(entries[1].tool, "b");
    }

    #[tokio::test]
    async fn redacts_query_when_query_storage_disabled() {
        let store = RecentActionsStore::new(10, StdDuration::from_secs(60), false, false);
        store.record(action("query")).await;

        let entries = store.list(10).await;
        assert_eq!(entries.len(), 1);
        assert!(entries[0].query.is_none());
        assert!(entries[0].query_redacted);
    }
}
