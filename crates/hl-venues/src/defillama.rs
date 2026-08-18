//! Yield pools, via DefiLlama's public API.
//!
//! The closest thing to a natural fit this project has found. A yield is a rate that
//! decays as capital floods in, which is precisely what the crowding meter measures —
//! so the mapping needs no new machinery at all:
//!
//! * **APY** becomes the reward metric. Falling reward is margin being competed away.
//! * **TVL** becomes competitor density. Rising TVL is capital crowding in.
//!
//! Both directions are already handled correctly: reward is a higher-is-better metric
//! and competitors is the one where higher is worse.
//!
//! Unlike every venue that killed an earlier thesis, this one has no identity gate, no
//! terms to accept, no per-item human step, no minimum position, and publishes its
//! rates free and unauthenticated. One request returns every pool.
//!
//! What it cannot do is make a small position large. Income here is a percentage of
//! capital deployed, so the meter's job is choosing *where*, not manufacturing *how
//! much*.

use crate::http::Transport;
use anyhow::{Context, Result};
use hl_core::{EntryCost, Niche, NicheClass, Observation, Source};
use serde::Deserialize;

pub const POOLS_URL: &str = "https://yields.llama.fi/pools";

/// Which pools are worth tracking at all.
#[derive(Debug, Clone)]
pub struct PoolFilter {
    /// Only stablecoin pools. Non-stable pools mix price movement into the return,
    /// which is a different bet than harvesting a rate.
    pub stablecoin_only: bool,
    /// Floor on TVL. Thin pools post spectacular headline rates that no real position
    /// can capture, and they are the single most common way this data misleads.
    pub min_tvl_usd: f64,
    /// Ceiling on APY. Anything above this is a reward-token emission or a trap, not a
    /// yield, and it decays to nothing the moment emissions stop.
    pub max_apy: f64,
    pub min_apy: f64,
    /// Exclude pools DefiLlama itself flags as statistical outliers.
    pub exclude_outliers: bool,
    /// Cap on how many pools become niches, taken by TVL descending.
    pub max_pools: usize,
}

impl Default for PoolFilter {
    fn default() -> Self {
        Self {
            stablecoin_only: true,
            min_tvl_usd: 5_000_000.0,
            max_apy: 40.0,
            min_apy: 0.5,
            exclude_outliers: true,
            max_pools: 120,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Pool {
    pub pool: String,
    pub chain: String,
    pub project: String,
    pub symbol: String,
    #[serde(rename = "tvlUsd")]
    pub tvl_usd: Option<f64>,
    pub apy: Option<f64>,
    #[serde(rename = "apyBase")]
    pub apy_base: Option<f64>,
    #[serde(rename = "apyReward")]
    pub apy_reward: Option<f64>,
    #[serde(default)]
    pub stablecoin: bool,
    #[serde(default)]
    pub outlier: bool,
    #[serde(rename = "ilRisk", default)]
    pub il_risk: String,
    #[serde(rename = "apyPct7D")]
    pub apy_pct_7d: Option<f64>,
}

impl Pool {
    pub fn niche_id(&self) -> String {
        format!("defi:{}:{}:{}", self.chain, self.project, self.symbol)
    }

    /// The rate we would actually harvest, in basis points.
    ///
    /// Prefers `apyBase` — the yield the protocol generates — over headline `apy`, which
    /// folds in reward-token emissions that stop without warning and are frequently
    /// paid in something illiquid.
    pub fn base_bps(&self) -> Option<u64> {
        let apy = self.apy_base.or(self.apy)?;
        (apy.is_finite() && apy > 0.0).then(|| (apy * 100.0).round() as u64)
    }

    /// Capital already in the pool, in thousands of dollars. This is the crowd.
    pub fn crowd(&self) -> Option<f64> {
        self.tvl_usd.filter(|v| *v > 0.0).map(|v| v / 1000.0)
    }

    /// Share of the headline rate that is emissions rather than real yield.
    pub fn emission_share(&self) -> f64 {
        match (self.apy, self.apy_reward) {
            (Some(a), Some(r)) if a > 0.0 => (r / a).clamp(0.0, 1.0),
            _ => 0.0,
        }
    }
}

pub struct DefiLlamaSource {
    transport: Box<dyn Transport>,
    pub filter: PoolFilter,
    id: String,
}

impl DefiLlamaSource {
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Self {
            transport,
            filter: PoolFilter::default(),
            id: "defillama".into(),
        }
    }

    pub fn with_filter(mut self, filter: PoolFilter) -> Self {
        self.filter = filter;
        self
    }

    /// One request covers every pool, so a full sweep costs exactly one call.
    pub fn request_cost(&self) -> u32 {
        1
    }

    fn fetch(&self) -> Result<Vec<Pool>> {
        let resp = self
            .transport
            .get(POOLS_URL, &[("Accept", "application/json")])?;
        if resp.status != 200 {
            anyhow::bail!("defillama returned status {}", resp.status);
        }
        Ok(select(&parse_pools(&resp.body)?, &self.filter))
    }
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    data: Vec<Pool>,
}

pub fn parse_pools(body: &str) -> Result<Vec<Pool>> {
    let env: Envelope = serde_json::from_str(body).context("parsing defillama pools")?;
    Ok(env.data)
}

/// Apply the filter and take the largest pools that survive it.
pub fn select(pools: &[Pool], f: &PoolFilter) -> Vec<Pool> {
    let mut kept: Vec<Pool> = pools
        .iter()
        .filter(|p| !f.stablecoin_only || p.stablecoin)
        .filter(|p| !(f.exclude_outliers && p.outlier))
        // Impermanent loss is a second, uncorrelated way to lose the position; a rate
        // trend says nothing about it.
        .filter(|p| p.il_risk.is_empty() || p.il_risk.eq_ignore_ascii_case("no"))
        .filter(|p| p.tvl_usd.unwrap_or(0.0) >= f.min_tvl_usd)
        .filter(|p| {
            let apy = p.apy.unwrap_or(0.0);
            apy >= f.min_apy && apy <= f.max_apy
        })
        .cloned()
        .collect();
    kept.sort_by(|a, b| {
        b.tvl_usd
            .unwrap_or(0.0)
            .partial_cmp(&a.tvl_usd.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    kept.truncate(f.max_pools);
    kept
}

impl Source for DefiLlamaSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn niches(&self) -> Result<Vec<Niche>> {
        Ok(self
            .fetch()?
            .into_iter()
            .map(|p| {
                let emissions = p.emission_share();
                Niche {
                    id: p.niche_id(),
                    label: format!("{} {} on {}", p.project, p.symbol, p.chain),
                    class: NicheClass::IncentiveProgram,
                    opened_ms: None,
                    first_seen_ms: hl_core::now_millis(),
                    entry_cost: EntryCost {
                        // Gas is real but chain-dependent and not published here; the
                        // policy's budget check is the wrong place to guess at it.
                        money_cents: 0,
                        requests: 0,
                        seconds: 300,
                    },
                    closes_ms: None,
                    source_url: Some(format!("https://defillama.com/yields/pool/{}", p.pool)),
                    notes: format!(
                        "{:.2}% APY, ${:.1}M TVL, {:.0}% of headline is emissions",
                        p.apy.unwrap_or(0.0),
                        p.tvl_usd.unwrap_or(0.0) / 1e6,
                        emissions * 100.0
                    ),
                }
            })
            .collect())
    }

    fn observe(&self, _since_ms: u64) -> Result<Vec<Observation>> {
        let now = hl_core::now_millis();
        Ok(self
            .fetch()?
            .into_iter()
            .filter_map(|p| {
                let bps = p.base_bps()?;
                let mut o = Observation::new(p.niche_id(), now, "defillama").reward(bps);
                if let Some(c) = p.crowd() {
                    o = o.competitors(c);
                }
                Some(o)
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::FixtureTransport;

    fn body() -> String {
        r#"{"status":"success","data":[
          {"pool":"a","chain":"Base","project":"aave-v3","symbol":"USDC","tvlUsd":50000000,
           "apy":6.5,"apyBase":6.5,"apyReward":null,"stablecoin":true,"outlier":false,"ilRisk":"no"},
          {"pool":"b","chain":"Ethereum","project":"emissions-farm","symbol":"USDC","tvlUsd":20000000,
           "apy":30.0,"apyBase":2.0,"apyReward":28.0,"stablecoin":true,"outlier":false,"ilRisk":"no"},
          {"pool":"c","chain":"Ethereum","project":"tiny","symbol":"USDC","tvlUsd":10000,
           "apy":900.0,"apyBase":900.0,"apyReward":null,"stablecoin":true,"outlier":false,"ilRisk":"no"},
          {"pool":"d","chain":"Ethereum","project":"volatile","symbol":"ETH-USDC","tvlUsd":90000000,
           "apy":12.0,"apyBase":12.0,"apyReward":null,"stablecoin":false,"outlier":false,"ilRisk":"yes"},
          {"pool":"e","chain":"Ethereum","project":"flagged","symbol":"USDT","tvlUsd":80000000,
           "apy":25.0,"apyBase":25.0,"apyReward":null,"stablecoin":true,"outlier":true,"ilRisk":"no"}
        ]}"#
        .to_string()
    }

    fn source() -> DefiLlamaSource {
        DefiLlamaSource::new(Box::new(
            FixtureTransport::new().with(POOLS_URL, 200, body()),
        ))
    }

    #[test]
    fn a_whole_sweep_is_one_request() {
        assert_eq!(source().request_cost(), 1);
        assert!(!source().observe(0).unwrap().is_empty());
    }

    #[test]
    fn thin_pools_with_spectacular_rates_are_excluded() {
        // 900% on $10k of TVL is not a yield anyone can actually take.
        let ids: Vec<String> = source()
            .niches()
            .unwrap()
            .into_iter()
            .map(|n| n.id)
            .collect();
        assert!(!ids.iter().any(|i| i.contains("tiny")));
    }

    #[test]
    fn volatile_and_flagged_pools_are_excluded() {
        let ids: Vec<String> = source().niches().unwrap().into_iter().map(|n| n.id).collect();
        assert!(!ids.iter().any(|i| i.contains("volatile")), "IL risk is a separate bet");
        assert!(!ids.iter().any(|i| i.contains("flagged")), "outliers are excluded");
    }

    #[test]
    fn emissions_are_stripped_out_of_the_measured_rate() {
        // Headline 30% but only 2% is real. Tracking the headline would have the meter
        // fitting a trend to a token giveaway.
        let pools = parse_pools(&body()).unwrap();
        let farm = pools.iter().find(|p| p.project == "emissions-farm").unwrap();
        assert_eq!(farm.base_bps(), Some(200), "apyBase, not apy");
        assert!((farm.emission_share() - 28.0 / 30.0).abs() < 1e-6);

        let real = pools.iter().find(|p| p.project == "aave-v3").unwrap();
        assert_eq!(real.base_bps(), Some(650));
        assert_eq!(real.emission_share(), 0.0);
    }

    #[test]
    fn observations_carry_rate_and_crowd_in_the_directions_the_meter_expects() {
        let obs = source().observe(0).unwrap();
        let aave = obs.iter().find(|o| o.niche_id.contains("aave-v3")).unwrap();
        // Reward: higher is better, so a falling APY reads as erosion.
        assert_eq!(aave.reward_cents, Some(650));
        // Competitors: higher is worse, so rising TVL reads as crowding.
        assert_eq!(aave.competitors, Some(50_000.0));
    }

    #[test]
    fn a_rate_that_halves_is_measured_as_a_closing_niche() {
        use hl_probe::{crowding::CrowdingMeter, policy, PolicyConfig};
        // Twelve days of a 7% rate compressing to 3.5% as capital arrives.
        let obs: Vec<Observation> = (0..48)
            .map(|i| {
                let t = i as f64 / 4.0;
                let apy = 7.0 * 0.5_f64.powf(t / 6.0);
                let tvl = 20_000.0 * 2.0_f64.powf(t / 6.0);
                Observation::new("defi:x", hl_probe::crowding::days_ms(t), "defillama")
                    .reward((apy * 100.0) as u64)
                    .competitors(tvl)
            })
            .collect();
        let r = CrowdingMeter::default().report("defi:x", &obs, hl_probe::crowding::days_ms(12.0));
        let hl = r.half_life_days().expect("a compressing rate must register");
        assert!((hl - 6.0).abs() < 0.8, "half-life measured as {hl:.2}d");
        let d = policy::decide(&r, &EntryCost::default(), &PolicyConfig::default());
        assert_eq!(d.signal, hl_core::Signal::Hold, "reason: {}", d.reason);
    }

    #[test]
    fn a_stable_rate_is_not_reported_as_closing() {
        use hl_probe::{crowding::CrowdingMeter, policy, PolicyConfig};
        let mut rng = hl_core::Rng::new(5);
        let obs: Vec<Observation> = (0..48)
            .map(|i| {
                let t = i as f64 / 4.0;
                let apy = 6.0 * (1.0 + 0.02 * (rng.unit() - 0.5));
                Observation::new("defi:y", hl_probe::crowding::days_ms(t), "defillama")
                    .reward((apy * 100.0) as u64)
            })
            .collect();
        let r = CrowdingMeter::default().report("defi:y", &obs, hl_probe::crowding::days_ms(12.0));
        let d = policy::decide(&r, &EntryCost::default(), &PolicyConfig::default());
        assert_eq!(d.signal, hl_core::Signal::Enter, "reason: {}", d.reason);
    }
}

#[cfg(test)]
mod live_probe {
    use super::*;
    use crate::http::UreqTransport;

    #[test]
    #[ignore]
    fn live_fetch() {
        let s = DefiLlamaSource::new(Box::new(UreqTransport::default()));
        match s.niches() {
            Ok(n) => {
                eprintln!("niches: {}", n.len());
                for x in n.iter().take(5) {
                    eprintln!("  {} :: {}", x.id, x.notes);
                }
            }
            Err(e) => eprintln!("ERROR: {e:#}"),
        }
    }
}
