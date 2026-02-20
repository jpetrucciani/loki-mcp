#![allow(dead_code)]

use std::time::Duration;

use moka::future::Cache;

#[derive(Clone)]
pub struct QueryCache {
    cache: Cache<String, serde_json::Value>,
}

impl QueryCache {
    pub fn new(max_entries: u64, ttl: Duration) -> Self {
        let cache = Cache::builder()
            .max_capacity(max_entries)
            .time_to_live(ttl)
            .build();

        Self { cache }
    }

    pub async fn get(&self, key: &str) -> Option<serde_json::Value> {
        self.cache.get(key).await
    }

    pub async fn insert(&self, key: String, value: serde_json::Value) {
        self.cache.insert(key, value).await;
    }
}
