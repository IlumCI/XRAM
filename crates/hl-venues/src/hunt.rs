//! High-rate window hunting.
//!
//! This is the aggressive end of the same machinery. Rates above a few hundred percent
//! exist, they are mechanically transient, and at a small position size the entry cost
//! is repaid in hours — so the window is genuinely capturable in a way a 5% pool never
//! is. What kills people here is not the rate, it is what the rate is *denominated in*.
//!
//! So the ranking never uses headline APY. It separates:
//!
//! * **`apy_base`** — yield paid in the asset you deposited. Realisable.
//! * **`apy_reward`** — yield paid in an emission token, quoted at today's price, on a
//!   schedule that is diluting that price as it pays. A 1,400% reward APY in a token
//!   that falls 95% is 70%.
//!
//! Two further distortions are labelled rather than hidden, because both make a
//! candidate look better than it is: a pool listing a huge rate today is, by
//! construction, one that has not collapsed yet, and pools that died are not in the
//! source data at all.

use crate::defillama::Pool;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy)]
pub struct HuntFilter {
    pub min_apy: f64,
    /// Floor on TVL. Below this the quoted rate is usually one trade annualised.
    pub min_tvl_usd: f64,
    /// Ceiling on TVL — a huge pool cannot pay a huge rate for long, and if it shows
    /// one the number is wrong.
    pub max_tvl_usd: f64,
    pub max_results: usize,
}

impl Default for HuntFilter {
    fn default() -> Self {
        Self {
            min_apy: 100.0,
            min_tvl_usd: 50_000.0,
            max_tvl_usd: 50_000_000.0,
            max_results: 25,
        }
    }
}

/// Base yield above this is not a rate anyone is offering.
///
/// A few hundred percent can be genuine — leveraged lending against heavy borrow
/// demand reaches it. Tens of thousands of percent is fee income from one busy day
/// annualised, and it will be gone tomorrow.
pub const VOLUME_SPIKE_APY: f64 = 1_000.0;

/// How a candidate can go to zero, named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskKind {
    /// Rate is paid in the deposited asset. The rate itself is realisable.
    BaseYield,
    /// Rate is mostly an emission token, quoted at a price its own emission is diluting.
    EmissionToken,
    /// Rate is fee income annualised from recent volume. One busy day reads as a year.
    VolumeSpike,
    /// The source quotes a rate but attributes none of it to either base yield or
    /// rewards. Something is generating the number that this data does not explain —
    /// often token price appreciation booked as yield — and an unexplained rate should
    /// never be presented as a realisable one.
    Unexplained,
    /// Rate is trading fees on a liquidity pair. Earned in the deposited assets, so it
    /// is realisable — but the fees are high *because* the pair moves, and the inventory
    /// loss from that movement is not in the quoted number and routinely exceeds it.
    LpFees,
}

impl RiskKind {
    pub fn label(self) -> &'static str {
        match self {
            RiskKind::BaseYield => "base yield",
            RiskKind::EmissionToken => "emission token",
            RiskKind::VolumeSpike => "volume spike",
            RiskKind::LpFees => "LP fees, IL not priced in",
            RiskKind::Unexplained => "unexplained by the data",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub niche_id: String,
    pub chain: String,
    pub project: String,
    pub symbol: String,
    pub apy: f64,
    pub apy_base: f64,
    pub apy_reward: f64,
    pub tvl_usd: f64,
    pub stablecoin: bool,
    /// True when the position holds two or more assets.
    pub is_pair: bool,
    /// DefiLlama's own impermanent-loss flag.
    pub il_risk: String,
    pub risk: RiskKind,
    /// Share of the headline rate that is emissions, 0..1.
    pub emission_share: f64,
    pub url: String,
}

impl Candidate {
    /// What $`capital` earns per hour at the *base* rate alone, in cents.
    ///
    /// Deliberately excludes emissions: it is the floor of what the position pays if
    /// every reward token turns out to be worthless, which is the assumption worth
    /// planning against.
    pub fn base_cents_per_hour(&self, capital_usd: f64) -> f64 {
        capital_usd * (self.apy_base / 100.0) / 365.0 / 24.0 * 100.0
    }

    /// The same figure taking the headline rate at face value — the optimistic bound.
    pub fn headline_cents_per_hour(&self, capital_usd: f64) -> f64 {
        capital_usd * (self.apy / 100.0) / 365.0 / 24.0 * 100.0
    }

    /// Hours for the position to repay a given entry cost at the base rate.
    ///
    /// `None` when the base rate is zero, which means the entry is never repaid by
    /// anything except the emission token holding its price.
    pub fn hours_to_repay(&self, capital_usd: f64, entry_cost_usd: f64) -> Option<f64> {
        let per_hour = self.base_cents_per_hour(capital_usd) / 100.0;
        (per_hour > 0.0).then(|| entry_cost_usd / per_hour)
    }
}

/// Classify and rank high-rate candidates.
///
/// Ordered by realisable base yield first, then by headline. A pool paying 30% in the
/// asset you deposited outranks one quoting 14,000% in a token it is printing.
pub fn hunt(pools: &[Pool], f: &HuntFilter) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = pools
        .iter()
        .filter(|p| {
            let tvl = p.tvl_usd.unwrap_or(0.0);
            let apy = p.apy.unwrap_or(0.0);
            apy >= f.min_apy && tvl >= f.min_tvl_usd && tvl <= f.max_tvl_usd
        })
        .map(|p| {
            let apy = p.apy.unwrap_or(0.0);
            let base = p.apy_base.unwrap_or(0.0);
            let reward = p.apy_reward.unwrap_or(0.0);
            let emission_share = if apy > 0.0 { (reward / apy).clamp(0.0, 1.0) } else { 0.0 };
            let is_pair = p.exposure.eq_ignore_ascii_case("multi")
                || p.il_risk.eq_ignore_ascii_case("yes")
                || p.symbol.contains('-');
            let risk = if apy > 0.0 && base + reward < apy * 0.1 {
                RiskKind::Unexplained
            } else if emission_share > 0.5 {
                RiskKind::EmissionToken
            } else if base > VOLUME_SPIKE_APY {
                RiskKind::VolumeSpike
            } else if is_pair {
                RiskKind::LpFees
            } else {
                RiskKind::BaseYield
            };
            Candidate {
                niche_id: p.niche_id(),
                chain: p.chain.clone(),
                project: p.project.clone(),
                symbol: p.symbol.clone(),
                apy,
                apy_base: base,
                apy_reward: reward,
                tvl_usd: p.tvl_usd.unwrap_or(0.0),
                stablecoin: p.stablecoin,
                is_pair,
                il_risk: p.il_risk.clone(),
                risk,
                emission_share,
                url: format!("https://defillama.com/yields/pool/{}", p.pool),
            }
        })
        .collect();

    out.sort_by(|a, b| {
        let key = |c: &Candidate| match c.risk {
            // Single-asset yield ranks above everything: it is the only kind whose
            // quoted number is what the position actually pays.
            RiskKind::BaseYield => (3, c.apy_base),
            RiskKind::LpFees => (2, c.apy_base),
            RiskKind::EmissionToken => (1, c.apy),
            RiskKind::VolumeSpike => (0, c.apy_base),
            // Last, always. A number nothing accounts for is not an opportunity.
            RiskKind::Unexplained => (-1, c.apy),
        };
        key(b).partial_cmp(&key(a)).unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(f.max_results);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::defillama::parse_pools;

    fn pools() -> Vec<Pool> {
        parse_pools(
            r#"{"data":[
              {"pool":"a","chain":"Base","project":"real-lending","symbol":"USDC","tvlUsd":2000000,
               "apy":140.0,"apyBase":140.0,"apyReward":null,"stablecoin":true,"outlier":false,"ilRisk":"no","exposure":"single"},
              {"pool":"f","chain":"Base","project":"uniswap-v3","symbol":"WETH-MEMECOIN","tvlUsd":800000,
               "apy":700.0,"apyBase":700.0,"apyReward":null,"stablecoin":false,"outlier":false,"ilRisk":"yes","exposure":"multi"},
              {"pool":"b","chain":"BSC","project":"printer","symbol":"ZBU","tvlUsd":500000,
               "apy":414855.0,"apyBase":0.0,"apyReward":414855.0,"stablecoin":false,"outlier":false,"ilRisk":"no"},
              {"pool":"c","chain":"Base","project":"uniswap-v4","symbol":"ETH-AEON","tvlUsd":60000,
               "apy":63706.0,"apyBase":63706.0,"apyReward":0.0,"stablecoin":false,"outlier":false,"ilRisk":"no"},
              {"pool":"d","chain":"Base","project":"dust","symbol":"X","tvlUsd":900,
               "apy":90000.0,"apyBase":90000.0,"apyReward":0.0,"stablecoin":false,"outlier":false,"ilRisk":"no"},
              {"pool":"e","chain":"Base","project":"boring","symbol":"USDC","tvlUsd":9000000,
               "apy":6.0,"apyBase":6.0,"apyReward":null,"stablecoin":true,"outlier":false,"ilRisk":"no"}
            ]}"#,
        )
        .unwrap()
    }

    #[test]
    fn realisable_yield_outranks_a_bigger_printed_number() {
        let c = hunt(&pools(), &HuntFilter::default());
        assert_eq!(
            c[0].project, "real-lending",
            "140% paid in USDC must outrank 414,855% paid in a token being printed"
        );
        assert_eq!(c[0].risk, RiskKind::BaseYield);
    }

    #[test]
    fn the_three_ways_the_number_lies_are_each_named() {
        let c = hunt(&pools(), &HuntFilter::default());
        let by = |p: &str| c.iter().find(|x| x.project == p).unwrap().risk;
        assert_eq!(by("real-lending"), RiskKind::BaseYield);
        assert_eq!(by("printer"), RiskKind::EmissionToken);
        assert_eq!(by("uniswap-v4"), RiskKind::VolumeSpike);
    }

    #[test]
    fn a_rate_the_data_cannot_account_for_ranks_last() {
        let p = parse_pools(
            r#"{"data":[
              {"pool":"z","chain":"Aptos","project":"ghost","symbol":"TAPT","tvlUsd":900000,
               "apy":1011.0,"apyBase":0.0,"apyReward":0.0,"stablecoin":false,"outlier":false,
               "ilRisk":"no","exposure":"single"},
              {"pool":"a","chain":"Base","project":"real","symbol":"USDC","tvlUsd":2000000,
               "apy":140.0,"apyBase":140.0,"apyReward":null,"stablecoin":true,"outlier":false,
               "ilRisk":"no","exposure":"single"}
            ]}"#,
        )
        .unwrap();
        let c = hunt(&p, &HuntFilter::default());
        assert_eq!(c[0].project, "real");
        let ghost = c.iter().find(|x| x.project == "ghost").unwrap();
        assert_eq!(ghost.risk, RiskKind::Unexplained);
        assert_eq!(ghost.base_cents_per_hour(20.0), 0.0);
    }

    #[test]
    fn a_liquidity_pair_is_not_the_same_object_as_a_lending_rate() {
        // 700% of trading fees on a volatile pair must not outrank 140% paid in USDC.
        // The fees are high because the pair moves, and that movement is the cost.
        let c = hunt(&pools(), &HuntFilter::default());
        assert_eq!(c[0].project, "real-lending", "single-asset yield ranks first");
        let lp = c.iter().find(|x| x.project == "uniswap-v3").unwrap();
        assert_eq!(lp.risk, RiskKind::LpFees);
        assert!(lp.is_pair);
        assert!(lp.risk.label().contains("IL not priced in"));
    }

    #[test]
    fn dust_pools_and_boring_pools_are_both_excluded() {
        let c = hunt(&pools(), &HuntFilter::default());
        assert!(!c.iter().any(|x| x.project == "dust"), "$900 TVL is one trade");
        assert!(!c.iter().any(|x| x.project == "boring"), "6% is not what this is for");
    }

    #[test]
    fn emission_share_separates_the_realisable_from_the_quoted() {
        let c = hunt(&pools(), &HuntFilter::default());
        let printer = c.iter().find(|x| x.project == "printer").unwrap();
        assert!((printer.emission_share - 1.0).abs() < 1e-9);
        // The floor assumption: every reward token worthless.
        assert_eq!(printer.base_cents_per_hour(20.0), 0.0);
        assert!(printer.headline_cents_per_hour(20.0) > 900.0);
        assert_eq!(printer.hours_to_repay(20.0, 0.15), None, "nothing repays the entry");
    }

    #[test]
    fn a_real_rate_repays_its_entry_cost_in_hours() {
        let c = hunt(&pools(), &HuntFilter::default());
        let real = c.iter().find(|x| x.project == "real-lending").unwrap();
        // 140% APY on $20 is about 0.32 cents an hour.
        let h = real.hours_to_repay(20.0, 0.15).unwrap();
        assert!((40.0..60.0).contains(&h), "entry repaid in {h:.0}h");
    }
}
