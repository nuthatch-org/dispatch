//! Sampled cross-checks, in place of sending every deterministic call to three providers.
//!
//! Only calls pinned to a settled block are compared: two honest nodes asked about "latest" may
//! disagree simply because their heads differ. A mismatch puts both providers on full quorum
//! until it expires, so a liar pays for itself in quorum traffic rather than going unnoticed.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use alloy_primitives::Address;
use serde_json::Value;

/// Blocks behind head before a numbered block counts as settled.
pub const CONFIRMATIONS: u64 = 64;

/// Whether the call names a block that will not change: a hash, or a number at least
/// `CONFIRMATIONS` behind `head`.
pub fn is_settled(method: &str, params: &Option<Value>, head: u64) -> bool {
    let Some(Value::Array(arr)) = params.as_ref() else {
        return false;
    };
    let settled = |tag: Option<&Value>| match tag {
        Some(Value::String(s)) => {
            parse_hex(s).is_some_and(|n| n.saturating_add(CONFIRMATIONS) <= head)
        }
        Some(Value::Object(o)) => o.contains_key("blockHash"),
        _ => false,
    };
    match method {
        "eth_getTransactionByHash" | "eth_getTransactionReceipt" | "eth_getBlockByHash" => true,
        "eth_getBalance" | "eth_getCode" | "eth_getTransactionCount" | "eth_call" => {
            settled(arr.get(1))
        }
        "eth_getStorageAt" => settled(arr.get(2)),
        "eth_getLogs" => match arr.first() {
            Some(Value::Object(f)) if f.contains_key("blockHash") => true,
            Some(Value::Object(f)) => settled(f.get("toBlock")) && settled(f.get("fromBlock")),
            _ => false,
        },
        _ => false,
    }
}

fn parse_hex(s: &str) -> Option<u64> {
    u64::from_str_radix(s.strip_prefix("0x")?, 16).ok()
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Agree,
    Disagree,
    /// One side had nothing to say (null, error), which proves nothing either way.
    Inconclusive,
}

impl Outcome {
    pub fn label(&self) -> &'static str {
        match self {
            Outcome::Agree => "agree",
            Outcome::Disagree => "disagree",
            Outcome::Inconclusive => "inconclusive",
        }
    }
}

pub fn compare(a: Option<&Value>, b: Option<&Value>) -> Outcome {
    match (a, b) {
        (Some(a), Some(b)) if !a.is_null() && !b.is_null() => {
            if a == b {
                Outcome::Agree
            } else {
                Outcome::Disagree
            }
        }
        _ => Outcome::Inconclusive,
    }
}

pub struct Suspects {
    until: Mutex<HashMap<Address, Instant>>,
    ttl: Duration,
}

impl Suspects {
    pub fn new(ttl: Duration) -> Self {
        Self {
            until: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    pub fn mark(&self, provider: Address) {
        self.until
            .lock()
            .unwrap()
            .insert(provider, Instant::now() + self.ttl);
    }

    pub fn is_suspect(&self, provider: Address) -> bool {
        let mut until = self.until.lock().unwrap();
        match until.get(&provider) {
            Some(t) if *t > Instant::now() => true,
            Some(_) => {
                until.remove(&provider);
                false
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn latest_and_recent_blocks_are_not_settled() {
        let head = 1_000;
        assert!(!is_settled("eth_call", &Some(json!([{}, "latest"])), head));
        assert!(!is_settled("eth_call", &Some(json!([{}, "0x3e0"])), head)); // 992
        assert!(!is_settled("eth_blockNumber", &Some(json!([])), head));
    }

    #[test]
    fn old_numbers_and_hashes_are_settled() {
        let head = 1_000;
        assert!(is_settled("eth_call", &Some(json!([{}, "0x64"])), head));
        assert!(is_settled(
            "eth_getBalance",
            &Some(json!(["0xabc", {"blockHash": "0x01"}])),
            head
        ));
        assert!(is_settled(
            "eth_getTransactionReceipt",
            &Some(json!(["0x01"])),
            head
        ));
        assert!(is_settled(
            "eth_getLogs",
            &Some(json!([{"fromBlock": "0x1", "toBlock": "0x64"}])),
            head
        ));
        assert!(!is_settled(
            "eth_getLogs",
            &Some(json!([{"fromBlock": "0x1", "toBlock": "latest"}])),
            head
        ));
    }

    #[test]
    fn null_on_either_side_proves_nothing() {
        let v = json!("0x1");
        assert_eq!(compare(Some(&v), Some(&v)), Outcome::Agree);
        assert_eq!(compare(Some(&v), Some(&json!("0x2"))), Outcome::Disagree);
        assert_eq!(compare(Some(&v), Some(&Value::Null)), Outcome::Inconclusive);
        assert_eq!(compare(None, Some(&v)), Outcome::Inconclusive);
    }

    #[test]
    fn suspicion_expires() {
        let s = Suspects::new(Duration::from_millis(1));
        let p = Address::repeat_byte(1);
        s.mark(p);
        std::thread::sleep(Duration::from_millis(5));
        assert!(!s.is_suspect(p));

        let s = Suspects::new(Duration::from_secs(60));
        s.mark(p);
        assert!(s.is_suspect(p));
        assert!(!s.is_suspect(Address::repeat_byte(2)));
    }
}
