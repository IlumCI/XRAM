//! Perpetual funding rates, via Hyperliquid's public API.
//!
//! A funding rate is the cleanest crowding signal in any market: it is the price one
//! side of a trade pays the other for being crowded. When too much capital is long, the
//! rate goes positive and longs pay shorts; the payment exists precisely to attract
//! capital to the empty side until the imbalance closes. So the rate *is* the crowd,
//! quoted in basis points, and it decays as the crowd arrives — which is the whole
//! premise of this project, stated by the venue itself.
//!
//! Mapping, unchanged from every other source:
//!
//! * **|funding|** becomes the reward metric — the rate a position on the paid side
//!   collects. Falling means the imbalance is closing.
//! * **Open interest in dollars** becomes competitor density. Rising means capital is
//!   arriving.
//!
//! Chosen over Gate, KuCoin, Binance and Bybit for one reason: it is permissionless.
//! The centralised venues need an account, which needs identity, which is the wall that
//! killed most of this project's earlier ideas. Binance and Bybit additionally
//! geo-block this egress outright.
//!
//! **Collecting funding is not free money.** It requires holding a leveraged position
//! with liquidation risk, and an extreme rate usually means something violent is
//! happening in the underlying. Capturing it delta-neutrally needs a second position on
//! another venue and more capital. This source measures where the rate is; it does not
//! claim the rate is safe.

use crate::http::Transport;
use anyhow::{Context, Result};
use hl_core::{EntryCost, Niche, NicheClass, Observation, Source};
use serde::Deserialize;

pub const INFO_URL: &str = "https://api.hyperliquid.xyz/info";
pub const INFO_BODY: &str = r#"{"type":"metaAndAssetCtxs"}"#;

/// Hyperliquid funds hourly, so the quoted rate annualises over 24 × 365.
pub const FUNDING_PERIODS_PER_YEAR: f64 = 24.0 * 365.0;

#[derive(Debug, Clone)]
pub struct PerpFilter {
    /// Floor on open interest, in dollars. Thin books quote spectacular rates that no
    /// real position can take without moving the market — the same trap as a thin yield
    /// pool.
    pub min_open_interest_usd: f64,
    /// Floor on 24h volume, so a stale market with a stuck rate is excluded.
    pub min_day_volume_usd: f64,
    /// Ceiling on the annualised rate. Past this the quote is a dislocation being
    /// actively resolved, not a rate anything can be planned around.
    pub max_apr_pct: f64,
    pub min_apr_pct: f64,
    pub max_markets: usize,
}

impl Default for PerpFilter {
    fn default() -> Self {
        Self {
            min_open_interest_usd: 1_000_000.0,
            min_day_volume_usd: 250_000.0,
            max_apr_pct: 400.0,
            min_apr_pct: 3.0,
            max_markets: 80,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Perp {
    pub name: String,
    /// Annualised funding, signed. Positive means longs pay shorts.
    pub funding_apr_pct: f64,
    pub open_interest_usd: f64,
    pub day_volume_usd: f64,
    pub mark_px: f64,
}

impl Perp {
    pub fn niche_id(&self) -> String {
        format!("perp:hyperliquid:{}", self.name)
    }

    /// Which side of the trade receives the funding payment.
    pub fn paid_side(&self) -> &'static str {
        if self.funding_apr_pct >= 0.0 {
            "short"
        } else {
            "long"
        }
    }

    /// The collectable rate in basis points, regardless of which side collects it.
    ///
    /// Magnitude rather than signed value: the meter fits how fast the opportunity is
    /// eroding, and an imbalance closing from either direction is the same erosion.
    pub fn collectable_bps(&self) -> u64 {
        (self.funding_apr_pct.abs() * 100.0).round() as u64
    }
}

pub struct HyperliquidSource {
    transport: Box<dyn Transport>,
    pub filter: PerpFilter,
    id: String,
}

impl HyperliquidSource {
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Self {
            transport,
            filter: PerpFilter::default(),
            id: "hyperliquid".into(),
        }
    }

    pub fn with_filter(mut self, filter: PerpFilter) -> Self {
        self.filter = filter;
        self
    }

    /// Every market arrives in one call.
    pub fn request_cost(&self) -> u32 {
        1
    }

    fn fetch(&self) -> Result<Vec<Perp>> {
        let resp = self.transport.post(
            INFO_URL,
            &[("Content-Type", "application/json")],
            INFO_BODY,
        )?;
        if resp.status != 200 {
            anyhow::bail!("hyperliquid returned status {}", resp.status);
        }
        Ok(select(&parse_perps(&resp.body)?, &self.filter))
    }
}

#[derive(Deserialize)]
struct Meta {
    universe: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
}

#[derive(Deserialize)]
struct Ctx {
    funding: Option<String>,
    #[serde(rename = "openInterest")]
    open_interest: Option<String>,
    #[serde(rename = "dayNtlVlm")]
    day_ntl_vlm: Option<String>,
    #[serde(rename = "markPx")]
    mark_px: Option<String>,
}

fn num(s: &Option<String>) -> f64 {
    s.as_deref().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0)
}

/// The response is a two-element array: metadata, then a context per asset, positionally
/// aligned. A length mismatch would silently misattribute every rate, so it is refused.
pub fn parse_perps(body: &str) -> Result<Vec<Perp>> {
    let (meta, ctxs): (Meta, Vec<Ctx>) =
        serde_json::from_str(body).context("parsing hyperliquid metaAndAssetCtxs")?;
    if meta.universe.len() != ctxs.len() {
        anyhow::bail!(
            "hyperliquid returned {} assets against {} contexts; refusing to pair them",
            meta.universe.len(),
            ctxs.len()
        );
    }
    Ok(meta
        .universe
        .into_iter()
        .zip(ctxs)
        .map(|(a, c)| {
            let mark = num(&c.mark_px);
            Perp {
                name: a.name,
                funding_apr_pct: num(&c.funding) * FUNDING_PERIODS_PER_YEAR * 100.0,
                open_interest_usd: num(&c.open_interest) * mark,
                day_volume_usd: num(&c.day_ntl_vlm),
                mark_px: mark,
            }
        })
        .collect())
}

pub fn select(perps: &[Perp], f: &PerpFilter) -> Vec<Perp> {
    let mut kept: Vec<Perp> = perps
        .iter()
        .filter(|p| p.open_interest_usd >= f.min_open_interest_usd)
        .filter(|p| p.day_volume_usd >= f.min_day_volume_usd)
        .filter(|p| {
            let a = p.funding_apr_pct.abs();
            a >= f.min_apr_pct && a <= f.max_apr_pct
        })
        .cloned()
        .collect();
    // By collectable rate, not by size: the point is finding the paid imbalance.
    kept.sort_by(|a, b| {
        b.funding_apr_pct
            .abs()
            .partial_cmp(&a.funding_apr_pct.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    kept.truncate(f.max_markets);
    kept
}

impl Source for HyperliquidSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn niches(&self) -> Result<Vec<Niche>> {
        Ok(self
            .fetch()?
            .into_iter()
            .map(|p| Niche {
                id: p.niche_id(),
                label: format!("{} perp funding", p.name),
                class: NicheClass::IncentiveProgram,
                opened_ms: None,
                first_seen_ms: hl_core::now_millis(),
                entry_cost: EntryCost {
                    money_cents: 0,
                    requests: 0,
                    seconds: 600,
                },
                closes_ms: None,
                source_url: Some(format!("https://app.hyperliquid.xyz/trade/{}", p.name)),
                notes: format!(
                    "{:.1}% APR paid to the {} side, ${:.1}M open interest",
                    p.funding_apr_pct.abs(),
                    p.paid_side(),
                    p.open_interest_usd / 1e6
                ),
            })
            .collect())
    }

    fn observe(&self, _since_ms: u64) -> Result<Vec<Observation>> {
        let now = hl_core::now_millis();
        Ok(self
            .fetch()?
            .into_iter()
            .map(|p| {
                Observation::new(p.niche_id(), now, "hyperliquid")
                    .reward(p.collectable_bps())
                    .competitors(p.open_interest_usd / 1000.0)
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::FixtureTransport;

    fn body() -> String {
        // funding is the hourly rate as a string, matching the live shape.
        r#"[{"universe":[
             {"name":"BTC"},{"name":"CROWDED"},{"name":"THIN"},{"name":"STALE"},{"name":"CALM"}
           ]},[
             {"funding":"0.0000125","openInterest":"20000","dayNtlVlm":"500000000","markPx":"100000"},
             {"funding":"-0.0003","openInterest":"1000","dayNtlVlm":"9000000","markPx":"2000"},
             {"funding":"0.002","openInterest":"10","dayNtlVlm":"400000","markPx":"5"},
             {"funding":"0.0005","openInterest":"100000","dayNtlVlm":"1000","markPx":"50"},
             {"funding":"0.0000001","openInterest":"50000","dayNtlVlm":"9000000","markPx":"100"}
           ]]"#
        .to_string()
    }

    fn source() -> HyperliquidSource {
        HyperliquidSource::new(Box::new(
            FixtureTransport::new().with(INFO_URL, 200, body()),
        ))
    }

    #[test]
    fn hourly_funding_annualises() {
        let p = parse_perps(&body()).unwrap();
        let btc = &p[0];
        // 0.0000125/hr × 24 × 365 × 100 = 10.95% APR, the live BTC-scale figure.
        assert!((btc.funding_apr_pct - 10.95).abs() < 0.01, "got {}", btc.funding_apr_pct);
        assert_eq!(btc.collectable_bps(), 1095);
    }

    #[test]
    fn the_paid_side_follows_the_sign() {
        let p = parse_perps(&body()).unwrap();
        assert_eq!(p[0].paid_side(), "short", "positive funding: longs pay shorts");
        assert_eq!(p[1].paid_side(), "long", "negative funding: shorts pay longs");
        // Either way the collectable magnitude is what erodes.
        assert!(p[1].collectable_bps() > 0);
    }

    #[test]
    fn thin_and_stale_markets_are_excluded() {
        let ids: Vec<String> = source().niches().unwrap().into_iter().map(|n| n.id).collect();
        assert!(!ids.iter().any(|i| i.contains("THIN")), "$50 of OI is not tradeable");
        assert!(!ids.iter().any(|i| i.contains("STALE")), "no volume means a stuck quote");
        assert!(!ids.iter().any(|i| i.contains("CALM")), "0.09% APR is not an opportunity");
        assert!(ids.iter().any(|i| i.contains("CROWDED")));
    }

    #[test]
    fn markets_rank_by_collectable_rate_not_by_size() {
        let n = source().niches().unwrap();
        assert!(
            n[0].id.contains("CROWDED"),
            "the paid imbalance outranks the big book: {:?}",
            n.iter().map(|x| &x.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn observations_carry_rate_and_crowd_in_the_expected_directions() {
        let obs = source().observe(0).unwrap();
        let btc = obs.iter().find(|o| o.niche_id.contains("BTC")).unwrap();
        assert_eq!(btc.reward_cents, Some(1095));
        // 20000 units × $100k = $2bn of OI, expressed in thousands.
        assert_eq!(btc.competitors, Some(2_000_000.0));
    }

    #[test]
    fn a_length_mismatch_is_refused_rather_than_misattributed() {
        // Pairing by position means a mismatch silently assigns BTC's rate to something
        // else, which is worse than no data.
        let bad = r#"[{"universe":[{"name":"BTC"},{"name":"ETH"}]},[
            {"funding":"0.001","openInterest":"10","dayNtlVlm":"1","markPx":"1"}]]"#;
        let err = parse_perps(bad).unwrap_err().to_string();
        assert!(err.contains("refusing to pair"), "got: {err}");
    }

    #[test]
    fn a_closing_imbalance_is_measured_as_a_closing_niche() {
        use hl_probe::{crowding::CrowdingMeter, crowding::days_ms, policy, PolicyConfig};
        // Funding compressing from 60% to 15% APR over 8 days as capital arrives.
        let obs: Vec<Observation> = (0..32)
            .map(|i| {
                let t = i as f64 / 4.0;
                let apr = 60.0 * 0.5_f64.powf(t / 4.0);
                Observation::new("perp:x", days_ms(t), "hyperliquid")
                    .reward((apr * 100.0) as u64)
                    .competitors(5_000.0 * 2.0_f64.powf(t / 4.0))
            })
            .collect();
        let r = CrowdingMeter::default().report("perp:x", &obs, days_ms(8.0));
        let hl = r.half_life_days().expect("a compressing rate must register");
        assert!((hl - 4.0).abs() < 0.6, "half-life {hl:.2}d");
        // Hold, not Exit: a 4-day runway still clears the 3-day payback, so the
        // position is worth harvesting but not worth building on.
        let d = policy::decide(&r, &EntryCost::default(), &PolicyConfig::default());
        assert_eq!(d.signal, hl_core::Signal::Hold, "reason: {}", d.reason);

        // Compress it twice as fast and the runway stops covering the payback at all.
        let fast: Vec<Observation> = (0..32)
            .map(|i| {
                let t = i as f64 / 4.0;
                Observation::new("perp:y", days_ms(t), "hyperliquid")
                    .reward((60.0 * 0.5_f64.powf(t / 1.5) * 100.0) as u64)
            })
            .collect();
        let r2 = CrowdingMeter::default().report("perp:y", &fast, days_ms(8.0));
        let d2 = policy::decide(&r2, &EntryCost::default(), &PolicyConfig::default());
        assert_eq!(d2.signal, hl_core::Signal::Exit, "reason: {}", d2.reason);
    }
}
