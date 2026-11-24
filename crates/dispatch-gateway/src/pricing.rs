//! Prices in USD, settled in GRT.
//!
//! Buyers compare against flat per-million USD prices, so that is what the gateway publishes. The
//! GRT value of each receipt is worked out when it is signed, from a configured rate that a feed
//! may keep current.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;

use crate::config::CapabilityTier;

#[derive(Debug, Deserialize, Clone)]
pub struct PricingConfig {
    /// USD per million requests for calls a full node can answer.
    #[serde(default = "default_standard")]
    pub standard_usd_per_million: f64,
    /// USD per million requests needing historical state.
    #[serde(default = "default_archive")]
    pub archive_usd_per_million: f64,
    /// USD per million `debug_*`, `trace_*` and `eth_getLogs` requests.
    #[serde(default = "default_trace")]
    pub trace_usd_per_million: f64,
    /// GRT price in USD, used until the feed answers and whenever it is not configured.
    pub grt_usd: f64,
    /// Optional price feed: a URL returning JSON, and a JSON pointer to the USD price in it.
    pub grt_usd_feed: Option<FeedConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct FeedConfig {
    pub url: String,
    pub pointer: String,
    #[serde(default = "default_feed_interval")]
    pub interval_secs: u64,
    /// Older than this, the rate is reported as stale. It is still used: a stale price is better
    /// than refusing traffic, and GRT does not move far in an hour.
    #[serde(default = "default_feed_max_age")]
    pub max_age_secs: u64,
}

fn default_standard() -> f64 {
    1.0
}
fn default_archive() -> f64 {
    3.0
}
fn default_trace() -> f64 {
    4.0
}
fn default_feed_interval() -> u64 {
    300
}
fn default_feed_max_age() -> u64 {
    3600
}

/// A provider's own minimum, in USD per million requests per class. The gateway pays the higher of
/// this and its list price, so a provider names its price instead of refusing receipts below it.
#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq)]
pub struct PriceFloor {
    pub standard_usd_per_million: Option<f64>,
    pub archive_usd_per_million: Option<f64>,
    pub trace_usd_per_million: Option<f64>,
}

impl PriceFloor {
    fn usd(&self, class: PriceClass) -> Option<f64> {
        match class {
            PriceClass::Standard => self.standard_usd_per_million,
            PriceClass::Archive => self.archive_usd_per_million,
            PriceClass::Trace => self.trace_usd_per_million,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceClass {
    Standard,
    Archive,
    Trace,
}

impl PriceClass {
    pub fn of(method: &str, tier: &CapabilityTier) -> Self {
        match tier {
            CapabilityTier::Debug => PriceClass::Trace,
            _ if method == "eth_getLogs" => PriceClass::Trace,
            CapabilityTier::Archive => PriceClass::Archive,
            CapabilityTier::Standard => PriceClass::Standard,
        }
    }
}

pub struct Pricer {
    config: PricingConfig,
    grt_usd_bits: AtomicU64,
    updated_unix: AtomicU64,
}

impl Pricer {
    pub fn new(config: PricingConfig) -> anyhow::Result<Self> {
        anyhow::ensure!(
            config.grt_usd > 0.0 && config.grt_usd.is_finite(),
            "pricing.grt_usd must be a positive price"
        );
        Ok(Self {
            grt_usd_bits: AtomicU64::new(config.grt_usd.to_bits()),
            updated_unix: AtomicU64::new(now_unix()),
            config,
        })
    }

    pub fn grt_usd(&self) -> f64 {
        f64::from_bits(self.grt_usd_bits.load(Ordering::Relaxed))
    }

    /// GRT wei for one request of this class at the list price.
    pub fn receipt_value(&self, class: PriceClass) -> u128 {
        self.receipt_value_for(class, None)
    }

    /// GRT wei for one request of this class to a provider, which is never paid below its floor.
    pub fn receipt_value_for(&self, class: PriceClass, floor: Option<&PriceFloor>) -> u128 {
        let list = match class {
            PriceClass::Standard => self.config.standard_usd_per_million,
            PriceClass::Archive => self.config.archive_usd_per_million,
            PriceClass::Trace => self.config.trace_usd_per_million,
        };
        let usd_per_million = floor
            .and_then(|f| f.usd(class))
            .map_or(list, |f| f.max(list));
        // usd/request ÷ usd/GRT × 1e18 wei/GRT, simplified: usd_per_million × 1e12 ÷ grt_usd.
        (usd_per_million * 1e12 / self.grt_usd()).round() as u128
    }

    pub fn is_stale(&self) -> bool {
        let Some(feed) = &self.config.grt_usd_feed else {
            return false;
        };
        now_unix().saturating_sub(self.updated_unix.load(Ordering::Relaxed)) > feed.max_age_secs
    }

    fn set_rate(&self, usd: f64) {
        self.grt_usd_bits.store(usd.to_bits(), Ordering::Relaxed);
        self.updated_unix.store(now_unix(), Ordering::Relaxed);
    }

    pub fn price_list(&self) -> serde_json::Value {
        serde_json::json!({
            "currency": "USD",
            "per_million": {
                "standard": self.config.standard_usd_per_million,
                "archive": self.config.archive_usd_per_million,
                "trace": self.config.trace_usd_per_million,
            },
            "trace_includes": ["debug_*", "trace_*", "eth_getLogs"],
            "settled_in": "GRT",
            "grt_usd": self.grt_usd(),
            "grt_usd_stale": self.is_stale(),
        })
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Keep the GRT rate current from the configured feed. Implausible answers are ignored.
pub async fn run_feed(pricer: Arc<Pricer>, client: reqwest::Client) {
    let Some(feed) = pricer.config.grt_usd_feed.clone() else {
        return;
    };
    let mut tick = tokio::time::interval(Duration::from_secs(feed.interval_secs));
    loop {
        tick.tick().await;
        let fetched = async {
            let v: serde_json::Value = client.get(&feed.url).send().await?.json().await?;
            v.pointer(&feed.pointer)
                .and_then(|p| p.as_f64())
                .ok_or_else(|| anyhow::anyhow!("no number at {} in {v}", feed.pointer))
        }
        .await;
        match fetched {
            Ok(usd) if plausible(pricer.grt_usd(), usd) => pricer.set_rate(usd),
            Ok(usd) => tracing::warn!(
                usd,
                current = pricer.grt_usd(),
                "ignoring implausible GRT price"
            ),
            Err(e) => tracing::warn!("GRT price feed failed: {e:#}"),
        }
        if pricer.is_stale() {
            tracing::warn!(grt_usd = pricer.grt_usd(), "GRT price is stale");
        }
    }
}

/// A feed glitch must not reprice every receipt tenfold in either direction.
fn plausible(current: f64, new: f64) -> bool {
    new.is_finite() && new > 0.0 && new / current < 10.0 && current / new < 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pricer(grt_usd: f64) -> Pricer {
        Pricer::new(PricingConfig {
            standard_usd_per_million: 1.0,
            archive_usd_per_million: 3.0,
            trace_usd_per_million: 4.0,
            grt_usd,
            grt_usd_feed: None,
        })
        .unwrap()
    }

    #[test]
    fn a_million_trace_calls_cost_four_dollars_of_grt() {
        let p = pricer(0.08);
        let per_request = p.receipt_value(PriceClass::Trace);
        // $4 at $0.08/GRT is 50 GRT for the million.
        assert_eq!(per_request * 1_000_000, 50_000_000_000_000_000_000);
    }

    #[test]
    fn classes_follow_tier_and_method() {
        assert_eq!(
            PriceClass::of("eth_getLogs", &CapabilityTier::Standard),
            PriceClass::Trace
        );
        assert_eq!(
            PriceClass::of("eth_getLogs", &CapabilityTier::Archive),
            PriceClass::Trace
        );
        assert_eq!(
            PriceClass::of("trace_block", &CapabilityTier::Debug),
            PriceClass::Trace
        );
        assert_eq!(
            PriceClass::of("eth_call", &CapabilityTier::Archive),
            PriceClass::Archive
        );
        assert_eq!(
            PriceClass::of("eth_call", &CapabilityTier::Standard),
            PriceClass::Standard
        );
    }

    #[test]
    fn grt_halving_doubles_the_grt_charged() {
        let a = pricer(0.10).receipt_value(PriceClass::Archive);
        let b = pricer(0.05).receipt_value(PriceClass::Archive);
        assert_eq!(b, 2 * a);
    }

    #[test]
    fn implausible_feed_values_are_refused() {
        assert!(plausible(0.08, 0.09));
        assert!(!plausible(0.08, 0.8));
        assert!(!plausible(0.08, 0.0));
        assert!(!plausible(0.08, f64::NAN));
    }

    #[test]
    fn zero_rate_is_refused_at_startup() {
        assert!(Pricer::new(PricingConfig {
            standard_usd_per_million: 1.0,
            archive_usd_per_million: 3.0,
            trace_usd_per_million: 4.0,
            grt_usd: 0.0,
            grt_usd_feed: None,
        })
        .is_err());
    }

    #[test]
    fn a_provider_floor_raises_its_price_and_never_lowers_it() {
        let p = pricer(0.02);
        let floor = PriceFloor {
            standard_usd_per_million: Some(4.0),
            archive_usd_per_million: Some(0.5),
            trace_usd_per_million: None,
        };
        let list = |c| p.receipt_value(c);
        let with = |c| p.receipt_value_for(c, Some(&floor));
        assert_eq!(with(PriceClass::Standard), 4 * list(PriceClass::Standard));
        assert_eq!(with(PriceClass::Archive), list(PriceClass::Archive));
        assert_eq!(with(PriceClass::Trace), list(PriceClass::Trace));
    }
}
