//! The last RAV this gateway signed, per collection.
//!
//! A RAV is cumulative and `collect()` pays out against whatever it says, so the value has to come
//! from the gateway's own record. Taking the floor from the caller lets any provider name its own
//! payout.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settled {
    #[serde(with = "u128_string")]
    pub value: u128,
    pub timestamp_ns: u64,
}

/// One receipt, reduced to what aggregation needs. `(timestamp_ns, nonce)` identifies it.
#[derive(Debug, Clone, Copy)]
pub struct ReceiptKey {
    pub timestamp_ns: u64,
    pub nonce: u64,
    pub value: u128,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FoldError {
    NothingToAggregate,
    Overflow,
}

/// Add the receipts not already covered by `last` to it. With no record, `floor` stands in for the
/// value and every receipt counts, which is why `floor` must never come from the caller.
pub fn fold(
    last: Option<Settled>,
    floor: u128,
    receipts: impl IntoIterator<Item = ReceiptKey>,
) -> Result<Settled, FoldError> {
    let (mut value, cutoff) = match last {
        Some(s) => (s.value, Some(s.timestamp_ns)),
        None => (floor, None),
    };
    let mut timestamp_ns = cutoff.unwrap_or(0);
    let mut seen = HashSet::new();
    let mut added = false;

    for r in receipts {
        if cutoff.is_some_and(|c| r.timestamp_ns <= c) || !seen.insert((r.timestamp_ns, r.nonce)) {
            continue;
        }
        value = value.checked_add(r.value).ok_or(FoldError::Overflow)?;
        timestamp_ns = timestamp_ns.max(r.timestamp_ns);
        added = true;
    }

    match (added, last) {
        (true, _) => Ok(Settled {
            value,
            timestamp_ns,
        }),
        // Everything was already covered: hand back the same RAV again.
        (false, Some(s)) => Ok(s),
        (false, None) => Err(FoldError::NothingToAggregate),
    }
}

pub struct RavLedger {
    path: Option<PathBuf>,
    entries: Mutex<HashMap<B256, Settled>>,
}

impl RavLedger {
    pub fn load(path: Option<PathBuf>) -> anyhow::Result<Self> {
        let entries = match &path {
            Some(p) if p.exists() => serde_json::from_slice(&std::fs::read(p)?)?,
            _ => HashMap::new(),
        };
        Ok(Self {
            path,
            entries: Mutex::new(entries),
        })
    }

    pub async fn get(&self, collection: B256) -> Option<Settled> {
        self.entries.lock().await.get(&collection).copied()
    }

    /// Run `f` against the collection's record while holding the lock, and keep its result only if
    /// it was written to disk. A RAV must never leave the gateway unless the gateway remembers it.
    pub async fn update<T, E>(
        &self,
        collection: B256,
        f: impl FnOnce(Option<Settled>) -> Result<(Settled, T), E>,
    ) -> Result<T, UpdateError<E>> {
        let mut entries = self.entries.lock().await;
        let previous = entries.get(&collection).copied();
        let (settled, out) = f(previous).map_err(UpdateError::Rejected)?;
        entries.insert(collection, settled);
        if let Err(e) = self.persist(&entries) {
            match previous {
                Some(p) => entries.insert(collection, p),
                None => entries.remove(&collection),
            };
            return Err(UpdateError::Persist(e));
        }
        Ok(out)
    }

    fn persist(&self, entries: &HashMap<B256, Settled>) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(entries)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum UpdateError<E> {
    Rejected(E),
    Persist(anyhow::Error),
}

mod u128_string {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(timestamp_ns: u64, nonce: u64, value: u128) -> ReceiptKey {
        ReceiptKey {
            timestamp_ns,
            nonce,
            value,
        }
    }

    #[test]
    fn first_rav_sums_receipts_over_the_trusted_floor() {
        let s = fold(None, 100, [r(1, 1, 10), r(2, 2, 20)]).unwrap();
        assert_eq!(
            s,
            Settled {
                value: 130,
                timestamp_ns: 2
            }
        );
    }

    #[test]
    fn duplicate_receipts_count_once() {
        let s = fold(None, 0, [r(1, 1, 10), r(1, 1, 10), r(1, 1, 10)]).unwrap();
        assert_eq!(s.value, 10);
    }

    #[test]
    fn receipts_already_covered_are_not_paid_twice() {
        let last = Settled {
            value: 30,
            timestamp_ns: 2,
        };
        let s = fold(Some(last), 0, [r(1, 1, 10), r(2, 2, 20), r(3, 3, 5)]).unwrap();
        assert_eq!(
            s,
            Settled {
                value: 35,
                timestamp_ns: 3
            }
        );
    }

    #[test]
    fn floor_is_ignored_once_a_record_exists() {
        let last = Settled {
            value: 30,
            timestamp_ns: 2,
        };
        let s = fold(Some(last), u128::MAX / 2, [r(3, 3, 5)]).unwrap();
        assert_eq!(s.value, 35);
    }

    #[test]
    fn resubmitting_only_covered_receipts_returns_the_same_rav() {
        let last = Settled {
            value: 30,
            timestamp_ns: 2,
        };
        assert_eq!(fold(Some(last), 0, [r(1, 1, 10)]).unwrap(), last);
    }

    #[test]
    fn overflow_is_refused_not_saturated() {
        let last = Settled {
            value: u128::MAX,
            timestamp_ns: 0,
        };
        assert_eq!(fold(Some(last), 0, [r(1, 1, 1)]), Err(FoldError::Overflow));
    }

    #[tokio::test]
    async fn ledger_survives_a_restart() {
        let path = std::env::temp_dir().join(format!("rav-ledger-{}.json", rand::random::<u64>()));
        let cid = B256::repeat_byte(7);
        let settled = Settled {
            value: u128::MAX - 1,
            timestamp_ns: 42,
        };

        let ledger = RavLedger::load(Some(path.clone())).unwrap();
        ledger
            .update::<(), ()>(cid, |_| Ok((settled, ())))
            .await
            .unwrap();

        let reloaded = RavLedger::load(Some(path.clone())).unwrap();
        assert_eq!(reloaded.entries.lock().await.get(&cid), Some(&settled));
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn failed_write_leaves_the_record_unchanged() {
        let dir = std::env::temp_dir().join(format!("no-such-dir-{}", rand::random::<u64>()));
        let ledger = RavLedger::load(Some(dir.join("ledger.json"))).unwrap();
        let cid = B256::repeat_byte(1);
        let out = ledger
            .update::<(), ()>(cid, |_| {
                Ok((
                    Settled {
                        value: 1,
                        timestamp_ns: 1,
                    },
                    (),
                ))
            })
            .await;
        assert!(matches!(out, Err(UpdateError::Persist(_))));
        assert!(ledger.entries.lock().await.is_empty());
    }
}
