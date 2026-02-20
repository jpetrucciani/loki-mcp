#![allow(dead_code)]

use std::{num::NonZeroU32, sync::Arc};

use governor::{DefaultKeyedRateLimiter, Quota, RateLimiter};

#[derive(Clone)]
pub struct ToolRateLimiter {
    limiter: Arc<DefaultKeyedRateLimiter<String>>,
}

impl ToolRateLimiter {
    pub fn new(rps: f64, burst: u32) -> Option<Self> {
        if rps <= 0.0 || burst == 0 {
            return None;
        }

        let per_second = NonZeroU32::new(rps.ceil() as u32)?;
        let burst_nz = NonZeroU32::new(burst)?;
        let quota = Quota::per_second(per_second).allow_burst(burst_nz);

        Some(Self {
            limiter: Arc::new(RateLimiter::keyed(quota)),
        })
    }

    pub fn check(
        &self,
        tool_name: &str,
        identity: &str,
        tenant_id: Option<&str>,
    ) -> Result<(), String> {
        let key = format!(
            "{tool_name}|{identity}|{}",
            tenant_id.unwrap_or("default_tenant")
        );

        self.limiter.check_key(&key).map(|_| ()).map_err(|error| {
            format!("rate limit exceeded for tool={tool_name}, identity={identity}: {error}")
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::rate_limit::ToolRateLimiter;

    #[test]
    fn enforces_limit_per_tool_identity_key() {
        let limiter = ToolRateLimiter::new(1.0, 1).expect("limiter should build");

        assert!(limiter.check("loki_query_logs", "alice", None).is_ok());
        assert!(limiter.check("loki_query_logs", "alice", None).is_err());

        assert!(limiter.check("loki_query_logs", "bob", None).is_ok());
        assert!(limiter.check("loki_query_metrics", "alice", None).is_ok());
    }
}
