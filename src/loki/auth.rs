#![allow(dead_code)]

use anyhow::{Result, bail};
use reqwest::RequestBuilder;

use crate::config::LokiConfig;

#[derive(Debug, Clone)]
pub enum LokiAuth {
    None,
    Basic { username: String, password: String },
    Bearer { token: String },
}

impl LokiAuth {
    pub fn from_config(config: &LokiConfig) -> Result<Self> {
        match config.auth_type.as_str() {
            "none" => Ok(Self::None),
            "basic" => {
                let Some(username) = config.username.clone() else {
                    bail!("loki.username is required when loki.auth_type=basic");
                };
                let Some(password) = config.password.clone() else {
                    bail!("loki.password is required when loki.auth_type=basic");
                };

                Ok(Self::Basic { username, password })
            }
            "bearer" => {
                let Some(token) = config.token.clone() else {
                    bail!("loki.token is required when loki.auth_type=bearer");
                };

                Ok(Self::Bearer { token })
            }
            other => bail!("unsupported loki auth type: {other}"),
        }
    }

    pub fn apply(&self, builder: RequestBuilder) -> RequestBuilder {
        match self {
            Self::None => builder,
            Self::Basic { username, password } => builder.basic_auth(username, Some(password)),
            Self::Bearer { token } => builder.bearer_auth(token),
        }
    }
}
