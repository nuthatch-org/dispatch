//! Binding a response attestation to the provider that served it.
//!
//! A signature that recovers to its own stated signer proves only that somebody signed. It is
//! evidence against a provider only when the signer is the provider itself, or an operator
//! HorizonStaking says may act for it on this data service.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use alloy::{sol, sol_types::SolCall};
use alloy_primitives::Address;

sol! {
    function isAuthorized(address serviceProvider, address verifier, address operator)
        external view returns (bool);
}

const TTL: Duration = Duration::from_secs(600);

pub struct Attestor {
    /// Arbitrum RPC and HorizonStaking address; without them only the provider's own key counts.
    chain: Option<(String, Address)>,
    data_service: Address,
    cache: Mutex<HashMap<(Address, Address), (bool, Instant)>>,
}

impl Attestor {
    pub fn new(chain: Option<(String, Address)>, data_service: Address) -> Self {
        Self {
            chain,
            data_service,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub async fn is_bound(
        &self,
        client: &reqwest::Client,
        provider: Address,
        signer: Address,
    ) -> bool {
        if signer == provider {
            return true;
        }
        let Some((rpc_url, staking)) = &self.chain else {
            return false;
        };
        if let Some((ok, at)) = self.cache.lock().unwrap().get(&(provider, signer)) {
            if at.elapsed() < TTL {
                return *ok;
            }
        }
        match query(
            client,
            rpc_url,
            *staking,
            provider,
            self.data_service,
            signer,
        )
        .await
        {
            Ok(ok) => {
                self.cache
                    .lock()
                    .unwrap()
                    .insert((provider, signer), (ok, Instant::now()));
                ok
            }
            // Not cached: a flaky RPC should not mark an honest operator unbound for ten minutes.
            Err(e) => {
                tracing::warn!(%provider, %signer, "isAuthorized lookup failed: {e:#}");
                false
            }
        }
    }
}

async fn query(
    client: &reqwest::Client,
    rpc_url: &str,
    staking: Address,
    provider: Address,
    data_service: Address,
    operator: Address,
) -> anyhow::Result<bool> {
    let data = isAuthorizedCall {
        serviceProvider: provider,
        verifier: data_service,
        operator,
    }
    .abi_encode();
    let resp: serde_json::Value = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_call",
            "params": [
                { "to": format!("{staking:#x}"), "data": format!("0x{}", hex::encode(data)) },
                "latest"
            ]
        }))
        .send()
        .await?
        .json()
        .await?;
    let raw = resp["result"]
        .as_str()
        .and_then(|h| hex::decode(h.trim_start_matches("0x")).ok())
        .filter(|b| b.len() == 32)
        .ok_or_else(|| anyhow::anyhow!("unexpected eth_call result: {resp}"))?;
    Ok(raw[31] == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn provider_key_is_bound_and_a_stranger_is_not_without_chain_access() {
        let a = Attestor::new(None, Address::ZERO);
        let client = reqwest::Client::new();
        let provider = Address::repeat_byte(1);
        assert!(a.is_bound(&client, provider, provider).await);
        assert!(!a.is_bound(&client, provider, Address::repeat_byte(2)).await);
    }

    #[tokio::test]
    async fn cached_answer_is_used_without_a_lookup() {
        // The RPC URL is unroutable, so any lookup would fail and return false.
        let a = Attestor::new(
            Some(("http://127.0.0.1:9".into(), Address::ZERO)),
            Address::ZERO,
        );
        let (provider, operator) = (Address::repeat_byte(1), Address::repeat_byte(2));
        a.cache
            .lock()
            .unwrap()
            .insert((provider, operator), (true, Instant::now()));
        assert!(
            a.is_bound(&reqwest::Client::new(), provider, operator)
                .await
        );
    }
}
