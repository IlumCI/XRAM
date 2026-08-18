//! Authorized security-audit contests (Cantina, Sherlock).
//!
//! This is the one seam found this session that is *non-rival*: a contest is a public
//! invitation to review specific code for a prize pool, and two reviewers who find two
//! different bugs are both paid. Crowding does not deplete the reward the way it depletes
//! a yield — it only depletes the *unfound bugs*, and that is a quantity these platforms
//! publish directly as a running findings count.
//!
//! So the mapping onto the crowding meter is, once again, exact — but it means something
//! real here rather than being an analogy:
//!
//! * **prize pool** is the reward.
//! * **findings already submitted** is the crowd. A contest with a large pot and few
//!   findings is an under-reviewed seam; one with hundreds of findings is picked over.
//! * **contest deadline** is a hard close, carried like a Kaggle deadline.
//!
//! Legitimacy is structural: every niche here is an explicit, published invitation to
//! review a named scope. Nothing in this module submits, exploits, or even fetches
//! contract source — it ranks *where a human should spend review time*, and the human
//! does the review inside the contest's own rules. KYC requirements are surfaced, not
//! hidden, because they are a real gate on who can collect.

use crate::http::Transport;
use crate::timefmt::parse_rfc3339_utc;
use anyhow::{Context, Result};
use hl_core::{EntryCost, Niche, NicheClass, Observation, Source};
use serde::Deserialize;

pub const CANTINA_URL: &str = "https://cantina.xyz/api/v0/competitions";
pub const SHERLOCK_URL: &str = "https://audits.sherlock.xyz/api/contests";

/// One authorized contest, normalised across platforms.
#[derive(Debug, Clone)]
pub struct Contest {
    pub platform: &'static str,
    pub slug: String,
    pub name: String,
    pub prize_usd: f64,
    /// Findings already submitted, where the platform reports it. This is the depletion
    /// signal — `None` when unpublished, which is treated as unknown rather than zero.
    pub findings_so_far: Option<u64>,
    pub starts_ms: Option<u64>,
    pub ends_ms: Option<u64>,
    /// True while the contest is open for submissions.
    pub live: bool,
    /// A collection gate: identity verification required to be paid.
    pub kyc_required: bool,
    pub url: String,
}

impl Contest {
    pub fn niche_id(&self) -> String {
        format!("audit:{}:{}", self.platform, self.slug)
    }

    /// Days of review time left. `None` when there is no published deadline.
    pub fn days_left(&self, now_ms: u64) -> Option<f64> {
        self.ends_ms
            .map(|e| e.saturating_sub(now_ms) as f64 / hl_core::MS_PER_DAY)
    }

    /// Prize per finding already in — a coarse read on how far the pot has been split.
    ///
    /// `None` when findings are unpublished. Not an expected value: it says nothing about
    /// whether *your* finding would be unique or valid, only how contested the pot is.
    pub fn prize_per_finding(&self) -> Option<f64> {
        self.findings_so_far
            .filter(|f| *f > 0)
            .map(|f| self.prize_usd / f as f64)
    }
}

pub struct ContestsSource {
    transport: Box<dyn Transport>,
    id: String,
    /// Include contests that have already closed. Off by default: closed contests are
    /// history, useful only for measuring how findings accumulated.
    pub include_closed: bool,
}

impl ContestsSource {
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Self {
            transport,
            id: "contests".into(),
            include_closed: false,
        }
    }

    pub fn including_closed(mut self) -> Self {
        self.include_closed = true;
        self
    }

    /// Cantina and Sherlock: one request each.
    pub fn request_cost(&self) -> u32 {
        2
    }

    pub fn fetch(&self) -> Result<Vec<Contest>> {
        let mut out = Vec::new();
        let mut errors = Vec::new();

        match self.transport.get(CANTINA_URL, &[("Accept", "application/json")]) {
            Ok(r) if r.status == 200 => match parse_cantina(&r.body) {
                Ok(c) => out.extend(c),
                Err(e) => errors.push(format!("cantina: {e}")),
            },
            Ok(r) => errors.push(format!("cantina: status {}", r.status)),
            Err(e) => errors.push(format!("cantina: {e}")),
        }
        match self.transport.get(SHERLOCK_URL, &[("Accept", "application/json")]) {
            Ok(r) if r.status == 200 => match parse_sherlock(&r.body) {
                Ok(c) => out.extend(c),
                Err(e) => errors.push(format!("sherlock: {e}")),
            },
            Ok(r) => errors.push(format!("sherlock: status {}", r.status)),
            Err(e) => errors.push(format!("sherlock: {e}")),
        }

        // Both platforms failing is a real failure; one failing is survivable.
        if out.is_empty() && !errors.is_empty() {
            anyhow::bail!("no contests fetched: {}", errors.join("; "));
        }
        if !self.include_closed {
            out.retain(|c| c.live);
        }
        Ok(out)
    }
}

// ---- Cantina ----

#[derive(Deserialize)]
struct CantinaComp {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    status: String,
    #[serde(rename = "totalRewardPot", default)]
    total_reward_pot: Option<String>,
    #[serde(rename = "totalFindings", default)]
    total_findings: Option<u64>,
    #[serde(rename = "kycRequired", default)]
    kyc_required: bool,
    #[serde(default)]
    timeframe: Option<CantinaTimeframe>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Deserialize)]
struct CantinaTimeframe {
    start: Option<String>,
    end: Option<String>,
}

pub fn parse_cantina(body: &str) -> Result<Vec<Contest>> {
    let comps: Vec<CantinaComp> =
        serde_json::from_str(body).context("parsing cantina competitions")?;
    Ok(comps
        .into_iter()
        .map(|c| {
            let (starts, ends) = c
                .timeframe
                .map(|t| {
                    (
                        t.start.as_deref().and_then(parse_rfc3339_utc),
                        t.end.as_deref().and_then(parse_rfc3339_utc),
                    )
                })
                .unwrap_or((None, None));
            Contest {
                platform: "cantina",
                slug: c.id.clone(),
                name: c.name.unwrap_or_else(|| "unnamed".into()),
                prize_usd: c
                    .total_reward_pot
                    .as_deref()
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0),
                findings_so_far: c.total_findings,
                starts_ms: starts,
                ends_ms: ends,
                live: c.status.eq_ignore_ascii_case("live")
                    || c.status.eq_ignore_ascii_case("active")
                    || c.status.eq_ignore_ascii_case("open"),
                kyc_required: c.kyc_required,
                url: c
                    .url
                    .unwrap_or_else(|| format!("https://cantina.xyz/competitions/{}", c.id)),
            }
        })
        .collect())
}

// ---- Sherlock ----

#[derive(Deserialize)]
struct SherlockEnvelope {
    #[serde(default)]
    items: Vec<SherlockContest>,
}

#[derive(Deserialize)]
struct SherlockContest {
    id: serde_json::Value,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    prize_pool: Option<f64>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    starts_at: Option<i64>,
    #[serde(default)]
    ends_at: Option<i64>,
    #[serde(default)]
    private: bool,
}

pub fn parse_sherlock(body: &str) -> Result<Vec<Contest>> {
    // The endpoint returns either a bare array or a paginated envelope depending on
    // params; accept both so a shape change does not silently empty the source.
    let env: SherlockEnvelope = match serde_json::from_str::<SherlockEnvelope>(body) {
        Ok(e) if !e.items.is_empty() => e,
        _ => SherlockEnvelope {
            items: serde_json::from_str(body).unwrap_or_default(),
        },
    };
    Ok(env
        .items
        .into_iter()
        // A private contest is invite-only; it is not an open invitation and must not be
        // surfaced as one.
        .filter(|c| !c.private)
        .map(|c| {
            let slug = match &c.id {
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::String(s) => s.clone(),
                _ => "unknown".into(),
            };
            let ms = |s: Option<i64>| s.filter(|v| *v > 0).map(|v| v as u64 * 1000);
            Contest {
                platform: "sherlock",
                slug: slug.clone(),
                name: c.title.unwrap_or_else(|| "untitled".into()),
                prize_usd: c.prize_pool.unwrap_or(0.0),
                findings_so_far: None,
                starts_ms: ms(c.starts_at),
                ends_ms: ms(c.ends_at),
                // Sherlock uses uppercase state codes. An open contest is one accepting
                // submissions; JUDGING and FINISHED are past that, so they are not live.
                live: {
                    let u = c.status.to_ascii_uppercase();
                    u.contains("RUNNING")
                        || u.contains("ACTIVE")
                        || u.contains("LIVE")
                        || u == "SHERLOCK_STARTED"
                        || u == "STARTED"
                },
                kyc_required: false,
                url: format!("https://audits.sherlock.xyz/contests/{slug}"),
            }
        })
        .collect())
}

impl Source for ContestsSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn niches(&self) -> Result<Vec<Niche>> {
        Ok(self
            .fetch()?
            .into_iter()
            .map(|c| Niche {
                id: c.niche_id(),
                label: format!("{} [{}]", c.name, c.platform),
                class: NicheClass::IncentiveProgram,
                opened_ms: c.starts_ms,
                first_seen_ms: hl_core::now_millis(),
                entry_cost: EntryCost {
                    money_cents: 0,
                    requests: 0,
                    // A real review is days of skilled human work; recorded honestly so
                    // nothing here reads as passive.
                    seconds: 2 * 24 * 3600,
                },
                closes_ms: c.ends_ms,
                source_url: Some(c.url.clone()),
                notes: format!(
                    "${:.0} pot{}{}",
                    c.prize_usd,
                    c.findings_so_far
                        .map(|f| format!(", {f} findings so far"))
                        .unwrap_or_default(),
                    if c.kyc_required { ", KYC required" } else { "" }
                ),
            })
            .collect())
    }

    fn observe(&self, _since_ms: u64) -> Result<Vec<Observation>> {
        let now = hl_core::now_millis();
        Ok(self
            .fetch()?
            .into_iter()
            .filter(|c| c.prize_usd > 0.0)
            .map(|c| {
                // Reward in whole dollars (the meter works in integer "cents"; for audit
                // pots, dollars keep the numbers in a sane range).
                let mut o = Observation::new(c.niche_id(), now, "contests")
                    .reward(c.prize_usd.round() as u64);
                if let Some(f) = c.findings_so_far {
                    o = o.competitors(f as f64);
                }
                o
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::FixtureTransport;

    const CANTINA: &str = r#"[
      {"id":"live1","name":"Fresh Protocol","status":"live","totalRewardPot":"200000",
       "totalFindings":4,"kycRequired":false,
       "timeframe":{"start":"2026-08-15T00:00:00Z","end":"2026-09-15T00:00:00Z"},
       "url":"https://cantina.xyz/competitions/live1"},
      {"id":"live2","name":"Picked Over","status":"live","totalRewardPot":"30000",
       "totalFindings":300,"kycRequired":true,
       "timeframe":{"start":"2026-08-10T00:00:00Z","end":"2026-08-19T00:00:00Z"}},
      {"id":"done1","name":"Finished","status":"complete","totalRewardPot":"2500000",
       "totalFindings":80,"kycRequired":false,
       "timeframe":{"start":"2026-01-01T00:00:00Z","end":"2026-02-01T00:00:00Z"}}
    ]"#;

    const SHERLOCK: &str = r#"{"items":[
      {"id":42,"title":"Big Public Contest","prize_pool":700000,"status":"running",
       "starts_at":1785000000,"ends_at":1786000000,"private":false},
      {"id":43,"title":"Invite Only","prize_pool":100000,"status":"running",
       "starts_at":1785000000,"ends_at":1786000000,"private":true}
    ]}"#;

    fn source(include_closed: bool) -> ContestsSource {
        let t = FixtureTransport::new()
            .with(CANTINA_URL, 200, CANTINA)
            .with(SHERLOCK_URL, 200, SHERLOCK);
        let s = ContestsSource::new(Box::new(t));
        if include_closed {
            s.including_closed()
        } else {
            s
        }
    }

    #[test]
    fn a_null_name_does_not_fail_the_whole_parse() {
        // Cantina returns null names for some entries; one such value used to reject the
        // entire array, dropping every live contest with it.
        let body = r#"[
          {"id":"x","name":null,"status":"live","totalRewardPot":"100",
           "totalFindings":1,"kycRequired":false,"url":null,
           "timeframe":{"start":"2026-08-15T00:00:00Z","end":"2026-09-15T00:00:00Z"}},
          {"id":"y","name":"Named","status":"live","totalRewardPot":"200",
           "totalFindings":2,"kycRequired":false,"timeframe":null}
        ]"#;
        let c = parse_cantina(body).unwrap();
        assert_eq!(c.len(), 2, "a null name must not drop the batch");
        assert_eq!(c[0].name, "unnamed");
        assert!(c[1].ends_ms.is_none(), "a null timeframe is tolerated");
    }

    #[test]
    fn both_platforms_are_merged_and_closed_ones_dropped_by_default() {
        let c = source(false).fetch().unwrap();
        assert_eq!(c.len(), 3, "two live cantina + one public sherlock; done1 dropped");
        assert!(c.iter().all(|x| x.live));
    }

    #[test]
    fn private_contests_are_not_surfaced_as_open_invitations() {
        let c = source(true).fetch().unwrap();
        assert!(!c.iter().any(|x| x.name == "Invite Only"), "private is not an invitation");
    }

    #[test]
    fn the_depletion_signal_distinguishes_fresh_from_picked_over() {
        let c = source(false).fetch().unwrap();
        let fresh = c.iter().find(|x| x.name == "Fresh Protocol").unwrap();
        let over = c.iter().find(|x| x.name == "Picked Over").unwrap();
        // $200k / 4 findings is a far better place to spend review time than $30k / 300.
        assert!(fresh.prize_per_finding().unwrap() > over.prize_per_finding().unwrap() * 100.0);
    }

    #[test]
    fn deadlines_and_kyc_are_carried_onto_the_niche() {
        let niches = source(false).niches().unwrap();
        let fresh = niches.iter().find(|n| n.label.contains("Fresh Protocol")).unwrap();
        assert!(fresh.closes_ms.is_some());
        let over = niches.iter().find(|n| n.label.contains("Picked Over")).unwrap();
        assert!(over.notes.contains("KYC required"));
    }

    #[test]
    fn observations_carry_prize_and_findings_in_the_right_directions() {
        let obs = source(false).observe(0).unwrap();
        let fresh = obs.iter().find(|o| o.niche_id.contains("live1")).unwrap();
        assert_eq!(fresh.reward_cents, Some(200_000));
        assert_eq!(fresh.competitors, Some(4.0));
    }

    #[test]
    fn one_platform_failing_still_returns_the_other() {
        let t = FixtureTransport::new()
            .with(CANTINA_URL, 200, CANTINA)
            .with(SHERLOCK_URL, 500, "down");
        let s = ContestsSource::new(Box::new(t));
        let c = s.fetch().unwrap();
        assert!(c.iter().all(|x| x.platform == "cantina"));
        assert!(!c.is_empty());
    }

    #[test]
    fn both_platforms_failing_is_an_error_not_silence() {
        let t = FixtureTransport::new()
            .with(CANTINA_URL, 503, "x")
            .with(SHERLOCK_URL, 500, "y");
        assert!(ContestsSource::new(Box::new(t)).fetch().is_err());
    }
}



