//! Provider discovery from the contract's own events, read from the chain or from a nuthatch nest.
//!
//! The hosted subgraph went stale once already: it kept indexing a superseded contract, listed
//! providers that had left and missed the one that joined. Reading `RPCDataService` events from
//! Arbitrum directly removes that layer. The first scan walks the contract's history in chunks the
//! RPC will accept; after that each poll reads only the blocks since the last one.

use std::collections::{BTreeSet, HashMap};

use alloy::{sol, sol_types::SolEvent};
use alloy_primitives::{Address, Bytes, B256};
use serde::Deserialize;

use crate::config::{CapabilityTier, ProviderConfig};

sol! {
    event ProviderRegistered(address indexed provider, string endpoint, string geoHash);
    event ProviderDeregistered(address indexed provider);
    event ServiceStarted(address indexed provider, uint64 indexed chainId, uint8 tier, string endpoint);
    event ServiceStopped(address indexed provider, uint64 indexed chainId, uint8 tier);
}

/// Block `RPCDataService` was deployed at on Arbitrum One; nothing before it can matter.
pub const DEFAULT_START_BLOCK: u64 = 456_917_519;
/// The public Arbitrum RPC refuses `eth_getLogs` over more than 10,000,000 blocks.
const CHUNK: u64 = 9_000_000;

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Registered {
        provider: Address,
        endpoint: String,
        geo_hash: String,
    },
    Deregistered {
        provider: Address,
    },
    Started {
        provider: Address,
        chain_id: u64,
        tier: u8,
    },
    Stopped {
        provider: Address,
        chain_id: u64,
        tier: u8,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLog {
    topics: Vec<B256>,
    data: Bytes,
    block_number: String,
    log_index: String,
}

fn decode(log: &RawLog) -> Option<Event> {
    let t0 = *log.topics.first()?;
    let topics = log.topics.iter().copied();
    if t0 == ProviderRegistered::SIGNATURE_HASH {
        let e = ProviderRegistered::decode_raw_log(topics, &log.data, true).ok()?;
        Some(Event::Registered {
            provider: e.provider,
            endpoint: e.endpoint,
            geo_hash: e.geoHash,
        })
    } else if t0 == ProviderDeregistered::SIGNATURE_HASH {
        let e = ProviderDeregistered::decode_raw_log(topics, &log.data, true).ok()?;
        Some(Event::Deregistered {
            provider: e.provider,
        })
    } else if t0 == ServiceStarted::SIGNATURE_HASH {
        let e = ServiceStarted::decode_raw_log(topics, &log.data, true).ok()?;
        Some(Event::Started {
            provider: e.provider,
            chain_id: e.chainId,
            tier: e.tier,
        })
    } else if t0 == ServiceStopped::SIGNATURE_HASH {
        let e = ServiceStopped::decode_raw_log(topics, &log.data, true).ok()?;
        Some(Event::Stopped {
            provider: e.provider,
            chain_id: e.chainId,
            tier: e.tier,
        })
    } else {
        None
    }
}

fn tier(t: u8) -> Option<CapabilityTier> {
    match t {
        0 => Some(CapabilityTier::Standard),
        1 => Some(CapabilityTier::Archive),
        2 => Some(CapabilityTier::Debug),
        _ => None, // 3 is WebSocket: a transport, not a routing tier
    }
}

/// Replay events in chain order into the providers that are registered now, with the services they
/// have started and not stopped. A provider with no live service is left out: it serves nothing.
pub fn fold(events: &[Event]) -> Vec<ProviderConfig> {
    struct State {
        endpoint: String,
        geo_hash: String,
        services: BTreeSet<(u64, u8)>,
    }
    let mut live: HashMap<Address, State> = HashMap::new();
    for e in events {
        match e {
            Event::Registered {
                provider,
                endpoint,
                geo_hash,
            } => {
                let s = live.entry(*provider).or_insert_with(|| State {
                    endpoint: String::new(),
                    geo_hash: String::new(),
                    services: BTreeSet::new(),
                });
                s.endpoint = endpoint.clone();
                s.geo_hash = geo_hash.clone();
            }
            Event::Deregistered { provider } => {
                live.remove(provider);
            }
            Event::Started {
                provider,
                chain_id,
                tier,
            } => {
                if let Some(s) = live.get_mut(provider) {
                    s.services.insert((*chain_id, *tier));
                }
            }
            Event::Stopped {
                provider,
                chain_id,
                tier,
            } => {
                if let Some(s) = live.get_mut(provider) {
                    s.services.remove(&(*chain_id, *tier));
                }
            }
        }
    }
    let mut out: Vec<ProviderConfig> = live
        .into_iter()
        .filter_map(|(address, s)| {
            let mut caps: HashMap<u64, Vec<CapabilityTier>> = HashMap::new();
            for (chain, t) in &s.services {
                if let Some(t) = tier(*t) {
                    let v = caps.entry(*chain).or_default();
                    if !v.contains(&t) {
                        v.push(t);
                    }
                }
            }
            if caps.is_empty() {
                return None;
            }
            Some(ProviderConfig {
                address,
                endpoint: s.endpoint.trim_end_matches('/').to_string(),
                chains: caps.keys().copied().collect(),
                region: Some(s.geo_hash).filter(|g| !g.is_empty()),
                capabilities: Vec::new(),
                chain_capabilities: caps,
                price_floor: None,
            })
        })
        .collect();
    out.sort_by_key(|p| p.address);
    out
}

/// Remembers every event seen and the next block to read, so each poll after the first is one
/// small `eth_getLogs` call.
pub struct ChainScanner {
    rpc_url: String,
    contract: Address,
    next_block: u64,
    events: Vec<Event>,
}

impl ChainScanner {
    pub fn new(rpc_url: String, contract: Address, start_block: u64) -> Self {
        Self {
            rpc_url,
            contract,
            next_block: start_block,
            events: Vec::new(),
        }
    }

    pub async fn poll(&mut self, client: &reqwest::Client) -> anyhow::Result<Vec<ProviderConfig>> {
        let head = self.head(client).await?;
        while self.next_block <= head {
            let to = (self.next_block + CHUNK - 1).min(head);
            let mut logs = self.logs(client, self.next_block, to).await?;
            logs.sort_by_key(|l| (hex_u64(&l.block_number), hex_u64(&l.log_index)));
            self.events.extend(logs.iter().filter_map(decode));
            self.next_block = to + 1;
        }
        Ok(fold(&self.events))
    }

    async fn rpc(
        &self,
        client: &reqwest::Client,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let resp: serde_json::Value = client
            .post(&self.rpc_url)
            .json(&serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
            .send()
            .await?
            .json()
            .await?;
        if let Some(e) = resp.get("error") {
            anyhow::bail!("{method}: {e}");
        }
        Ok(resp["result"].clone())
    }

    async fn head(&self, client: &reqwest::Client) -> anyhow::Result<u64> {
        let r = self
            .rpc(client, "eth_blockNumber", serde_json::json!([]))
            .await?;
        Ok(hex_u64(r.as_str().unwrap_or("0x0")))
    }

    async fn logs(
        &self,
        client: &reqwest::Client,
        from: u64,
        to: u64,
    ) -> anyhow::Result<Vec<RawLog>> {
        let topics = [
            ProviderRegistered::SIGNATURE_HASH,
            ProviderDeregistered::SIGNATURE_HASH,
            ServiceStarted::SIGNATURE_HASH,
            ServiceStopped::SIGNATURE_HASH,
        ];
        let r = self
            .rpc(
                client,
                "eth_getLogs",
                serde_json::json!([{
                    "address": format!("{:#x}", self.contract),
                    "fromBlock": format!("{from:#x}"),
                    "toBlock": format!("{to:#x}"),
                    "topics": [topics],
                }]),
            )
            .await?;
        Ok(serde_json::from_value(r)?)
    }
}

/// Reads the same four events from a nuthatch nest indexing `RPCDataService`, through its `/sql`.
/// The nest has already decoded and ordered them; this only replays them.
pub struct NestSource {
    sql_url: String,
}

impl NestSource {
    pub fn new(nest_url: &str) -> Self {
        Self {
            sql_url: format!("{}/sql", nest_url.trim_end_matches('/')),
        }
    }

    pub async fn poll(&self, client: &reqwest::Client) -> anyhow::Result<Vec<ProviderConfig>> {
        let mut rows: Vec<(u64, u64, Event)> = Vec::new();
        for (table, cols) in [
            (
                "dispatch__provider_registered",
                "provider, endpoint, \"geoHash\" AS geo",
            ),
            ("dispatch__provider_deregistered", "provider"),
            (
                "dispatch__service_started",
                "provider, \"chainId\" AS chain, tier",
            ),
            (
                "dispatch__service_stopped",
                "provider, \"chainId\" AS chain, tier",
            ),
        ] {
            let q = format!("SELECT block_number, log_index, {cols} FROM {table}");
            // `GET /sql?q=` is what every nest serves; a `dev` nest refuses `POST /sql` with 405.
            let resp: serde_json::Value = client
                .get(&self.sql_url)
                .query(&[("q", q.as_str())])
                .send()
                .await?
                .json()
                .await?;
            if let Some(e) = resp.get("error") {
                anyhow::bail!("{table}: {e}");
            }
            if resp["truncated"].as_bool() == Some(true) {
                anyhow::bail!(
                    "{table}: the nest truncated the answer, refusing a partial provider list"
                );
            }
            for r in resp["rows"].as_array().cloned().unwrap_or_default() {
                let num = |k: &str| {
                    r[k].as_u64()
                        .or_else(|| r[k].as_str().and_then(|s| s.parse().ok()))
                };
                let text = |k: &str| r[k].as_str().unwrap_or_default().to_string();
                let Ok(provider) = text("provider").parse::<Address>() else {
                    continue;
                };
                let (Some(block), Some(idx)) = (num("block_number"), num("log_index")) else {
                    continue;
                };
                let ev = match table {
                    "dispatch__provider_registered" => Event::Registered {
                        provider,
                        endpoint: text("endpoint"),
                        geo_hash: text("geo"),
                    },
                    "dispatch__provider_deregistered" => Event::Deregistered { provider },
                    _ => {
                        let (Some(chain_id), Some(tier)) = (num("chain"), num("tier")) else {
                            continue;
                        };
                        let tier = tier as u8;
                        if table == "dispatch__service_started" {
                            Event::Started {
                                provider,
                                chain_id,
                                tier,
                            }
                        } else {
                            Event::Stopped {
                                provider,
                                chain_id,
                                tier,
                            }
                        }
                    }
                };
                rows.push((block, idx, ev));
            }
        }
        rows.sort_by_key(|(b, i, _)| (*b, *i));
        Ok(fold(
            &rows.into_iter().map(|(_, _, e)| e).collect::<Vec<_>>(),
        ))
    }
}

fn hex_u64(s: &str) -> u64 {
    u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(b: u8) -> Address {
        Address::repeat_byte(b)
    }

    #[test]
    fn replay_keeps_only_registered_providers_with_live_services() {
        let ev = vec![
            Event::Registered {
                provider: a(1),
                endpoint: "https://one/".into(),
                geo_hash: "u0yj".into(),
            },
            Event::Started {
                provider: a(1),
                chain_id: 10,
                tier: 0,
            },
            Event::Started {
                provider: a(1),
                chain_id: 10,
                tier: 2,
            },
            Event::Registered {
                provider: a(2),
                endpoint: "https://two".into(),
                geo_hash: String::new(),
            },
            Event::Started {
                provider: a(2),
                chain_id: 42161,
                tier: 0,
            },
            Event::Deregistered { provider: a(2) },
            Event::Registered {
                provider: a(3),
                endpoint: "https://three".into(),
                geo_hash: String::new(),
            },
            Event::Started {
                provider: a(3),
                chain_id: 1,
                tier: 1,
            },
            Event::Stopped {
                provider: a(3),
                chain_id: 1,
                tier: 1,
            },
            Event::Started {
                provider: a(4),
                chain_id: 1,
                tier: 0,
            }, // never registered
        ];
        let p = fold(&ev);
        assert_eq!(
            p.len(),
            1,
            "deregistered, stopped and unregistered providers are dropped"
        );
        assert_eq!(p[0].address, a(1));
        assert_eq!(p[0].endpoint, "https://one");
        assert_eq!(p[0].region.as_deref(), Some("u0yj"));
        let caps = &p[0].chain_capabilities[&10];
        assert!(caps.contains(&CapabilityTier::Standard) && caps.contains(&CapabilityTier::Debug));
    }

    #[test]
    fn a_registered_log_decodes() {
        let e = ProviderRegistered {
            provider: a(7),
            endpoint: "https://x".into(),
            geoHash: "u0".into(),
        };
        let log = e.encode_log_data();
        let raw = RawLog {
            topics: log.topics().to_vec(),
            data: log.data.clone(),
            block_number: "0x1".into(),
            log_index: "0x0".into(),
        };
        assert_eq!(
            decode(&raw),
            Some(Event::Registered {
                provider: a(7),
                endpoint: "https://x".into(),
                geo_hash: "u0".into()
            })
        );
    }

    #[tokio::test]
    async fn the_nest_source_replays_its_tables_in_chain_order() {
        use axum::{extract::Query, routing::get, Json, Router};
        use serde_json::{json, Value};
        use std::collections::HashMap;

        // Rows arrive per table and out of chain order; numbers sometimes as strings.
        async fn sql(Query(params): Query<HashMap<String, String>>) -> Json<Value> {
            let q = params.get("q").cloned().unwrap_or_default();
            let p = "0x0101010101010101010101010101010101010101";
            let rows = if q.contains("provider_registered") {
                json!([{ "block_number": 10, "log_index": 0, "provider": p, "endpoint": "https://one", "geo": "u0" }])
            } else if q.contains("service_started") {
                json!([
                    { "block_number": 12, "log_index": 1, "provider": p, "chain": 10, "tier": 0 },
                    { "block_number": "11", "log_index": "0", "provider": p, "chain": "10", "tier": "2" }
                ])
            } else if q.contains("service_stopped") {
                json!([{ "block_number": 13, "log_index": 0, "provider": p, "chain": 10, "tier": 2 }])
            } else {
                json!([])
            };
            Json(json!({ "rows": rows, "truncated": false }))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/sql", get(sql)))
                .await
                .unwrap()
        });

        let p = NestSource::new(&format!("http://{addr}"))
            .poll(&reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].endpoint, "https://one");
        // Debug started at 11 and stopped at 13; standard started at 12 and stays.
        assert_eq!(p[0].chain_capabilities[&10], vec![CapabilityTier::Standard]);
    }
}
