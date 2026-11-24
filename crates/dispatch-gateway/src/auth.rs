//! API keys. The gateway is the payer for every request it forwards, so an unauthenticated gateway
//! spends its own escrow on behalf of anyone who finds the URL.
//!
//! Only keccak256 hashes of keys are configured; `dispatch-gateway keygen <name>` mints a key.

use std::{collections::HashMap, num::NonZeroU32, sync::Arc};

use alloy_primitives::{keccak256, B256};
use axum::http::HeaderMap;
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use serde::Deserialize;

use crate::error::GatewayError;

#[derive(Debug, Deserialize, Clone)]
pub struct AuthConfig {
    #[serde(default)]
    pub keys: Vec<KeyConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct KeyConfig {
    /// Shown in logs and metrics. Never the key itself.
    pub name: String,
    /// keccak256 of the key, 0x-prefixed hex.
    pub key_hash: B256,
    /// Per-key request budget; unlimited when absent.
    pub requests_per_second: Option<u32>,
    /// Requests per UTC day. Every request is paid from the gateway's escrow, so this bounds what
    /// one key can spend. Counted in memory: a restart starts the day's count again.
    pub requests_per_day: Option<u64>,
}

pub struct KeyEntry {
    pub name: String,
    limiter: Option<DefaultDirectRateLimiter>,
    per_day: Option<u64>,
    /// (UTC day number, requests counted on it).
    today: std::sync::Mutex<(u64, u64)>,
}

fn utc_day() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400)
}

impl KeyEntry {
    fn charge_day(&self, n: u64, day: u64) -> Result<(), GatewayError> {
        let Some(cap) = self.per_day else {
            return Ok(());
        };
        let mut today = self.today.lock().unwrap();
        if today.0 != day {
            *today = (day, 0);
        }
        if today.1 + n > cap {
            return Err(GatewayError::DailyQuotaExceeded(cap));
        }
        today.1 += n;
        Ok(())
    }
}

pub enum Caller {
    Key(Arc<KeyEntry>),
    /// Only when no `[auth]` section is configured.
    Anonymous,
}

impl Caller {
    pub fn label(&self) -> &str {
        match self {
            Caller::Key(k) => &k.name,
            Caller::Anonymous => "anonymous",
        }
    }

    /// Charge `n` requests against the key's budget. A batch counts once per item.
    pub fn check_rate(&self, n: u32) -> Result<(), GatewayError> {
        let Caller::Key(k) = self else {
            return Ok(());
        };
        if let (Some(limiter), Some(nz)) = (&k.limiter, NonZeroU32::new(n)) {
            if !matches!(limiter.check_n(nz), Ok(Ok(()))) {
                return Err(GatewayError::RateLimited);
            }
        }
        k.charge_day(n as u64, utc_day())
    }
}

pub struct ApiKeys {
    /// None means auth is not configured and every caller is anonymous.
    by_hash: Option<HashMap<B256, Arc<KeyEntry>>>,
}

impl ApiKeys {
    pub fn new(config: Option<&AuthConfig>) -> Self {
        let Some(config) = config else {
            tracing::warn!(
                "no [auth] section: the gateway serves and pays for requests from anyone"
            );
            return Self { by_hash: None };
        };
        let by_hash = config
            .keys
            .iter()
            .map(|k| {
                let limiter = k
                    .requests_per_second
                    .and_then(NonZeroU32::new)
                    .map(|rps| RateLimiter::direct(Quota::per_second(rps)));
                (
                    k.key_hash,
                    Arc::new(KeyEntry {
                        name: k.name.clone(),
                        limiter,
                        per_day: k.requests_per_day,
                        today: std::sync::Mutex::new((0, 0)),
                    }),
                )
            })
            .collect();
        Self {
            by_hash: Some(by_hash),
        }
    }

    /// Resolve the caller from a key in the path, `X-Api-Key`, or `Authorization: Bearer`.
    pub fn authenticate(
        &self,
        path_key: Option<&str>,
        headers: &HeaderMap,
    ) -> Result<Caller, GatewayError> {
        let Some(by_hash) = &self.by_hash else {
            return Ok(Caller::Anonymous);
        };
        let key = path_key
            .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
            .or_else(|| {
                headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
            })
            .ok_or(GatewayError::Unauthorized)?;
        by_hash
            .get(&keccak256(key.trim().as_bytes()))
            .cloned()
            .map(Caller::Key)
            .ok_or(GatewayError::Unauthorized)
    }
}

/// A fresh key and the hash that goes in the config.
pub fn generate() -> (String, B256) {
    let key = format!("dsp_{}", hex::encode(rand::random::<[u8; 24]>()));
    let hash = keccak256(key.as_bytes());
    (key, hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(rps: Option<u32>) -> (ApiKeys, String) {
        let (key, hash) = generate();
        let cfg = AuthConfig {
            keys: vec![KeyConfig {
                name: "graphops".into(),
                key_hash: hash,
                requests_per_second: rps,
                requests_per_day: None,
            }],
        };
        (ApiKeys::new(Some(&cfg)), key)
    }

    #[test]
    fn without_auth_config_everyone_is_anonymous() {
        let caller = ApiKeys::new(None)
            .authenticate(None, &HeaderMap::new())
            .unwrap();
        assert_eq!(caller.label(), "anonymous");
    }

    #[test]
    fn with_auth_config_a_missing_or_wrong_key_is_refused() {
        let (keys, _) = keys(None);
        assert!(matches!(
            keys.authenticate(None, &HeaderMap::new()),
            Err(GatewayError::Unauthorized)
        ));
        assert!(matches!(
            keys.authenticate(Some("dsp_wrong"), &HeaderMap::new()),
            Err(GatewayError::Unauthorized)
        ));
    }

    #[test]
    fn key_is_accepted_from_path_header_or_bearer() {
        let (keys, key) = keys(None);
        assert_eq!(
            keys.authenticate(Some(&key), &HeaderMap::new())
                .unwrap()
                .label(),
            "graphops"
        );

        let mut h = HeaderMap::new();
        h.insert("x-api-key", key.parse().unwrap());
        assert_eq!(keys.authenticate(None, &h).unwrap().label(), "graphops");

        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Bearer {key}").parse().unwrap());
        assert_eq!(keys.authenticate(None, &h).unwrap().label(), "graphops");
    }

    #[test]
    fn a_batch_spends_its_whole_size_against_the_budget() {
        let (keys, key) = keys(Some(10));
        let caller = keys.authenticate(Some(&key), &HeaderMap::new()).unwrap();
        assert!(caller.check_rate(10).is_ok());
        assert!(matches!(
            caller.check_rate(1),
            Err(GatewayError::RateLimited)
        ));
    }

    #[test]
    fn keygen_output_parses_as_config() {
        let (key, hash) = generate();
        let cfg: AuthConfig = toml::from_str(&format!(
            "[[keys]]\nname = \"graphops\"\nkey_hash = \"{hash}\"\nrequests_per_second = 5"
        ))
        .unwrap();
        let caller = ApiKeys::new(Some(&cfg))
            .authenticate(Some(&key), &HeaderMap::new())
            .unwrap();
        assert_eq!(caller.label(), "graphops");
    }

    #[test]
    fn the_daily_cap_counts_batches_and_resets_on_a_new_day() {
        let e = KeyEntry {
            name: "k".into(),
            limiter: None,
            per_day: Some(10),
            today: std::sync::Mutex::new((0, 0)),
        };
        assert!(e.charge_day(7, 100).is_ok());
        assert!(matches!(
            e.charge_day(4, 100),
            Err(GatewayError::DailyQuotaExceeded(10))
        ));
        assert!(e.charge_day(3, 100).is_ok());
        assert!(e.charge_day(1, 100).is_err());
        assert!(
            e.charge_day(10, 101).is_ok(),
            "a new UTC day starts the count again"
        );
    }
}
