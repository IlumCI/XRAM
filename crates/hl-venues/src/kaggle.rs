//! Kaggle competitions.
//!
//! The cleanest niche in the array, because crowding is not a proxy here — it is
//! published. `teamCount` is literally how many others are chasing the same prize, and
//! the prize is a stated number, so expected value per entrant is a division rather
//! than an inference.
//!
//! Two properties make these niches behave unlike the others:
//!
//! * Reward is *fixed* while entrants accumulate, so value per entrant decays
//!   mechanically as the deadline approaches. That is real erosion, not an artefact.
//! * Every competition has a **published deadline**, which no amount of measurement
//!   would reveal. It is carried on the niche and bounds the runway directly.
//!
//! Authentication is a bearer token read from the environment. Absent a token the
//! source simply reports no niches, so a checkout without credentials still sweeps
//! everything else rather than failing.

use crate::http::Transport;
use crate::timefmt::parse_rfc3339_utc;
use anyhow::{Context, Result};
use hl_core::{EntryCost, Niche, NicheClass, Observation, Source};
use serde::Deserialize;

/// Kaggle's API takes a bearer token; basic auth with the same value is rejected.
pub fn auth_token() -> Option<String> {
    for key in ["KAGGLE_KEY", "KAGGLE_API_TOKEN"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

pub struct KaggleSource {
    transport: Box<dyn Transport>,
    id: String,
    /// How many listing pages to walk. Two covers the active set comfortably.
    pub pages: u32,
}

impl KaggleSource {
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Self {
            transport,
            id: "kaggle".into(),
            pages: 2,
        }
    }

    pub fn request_cost(&self) -> u32 {
        if auth_token().is_some() {
            self.pages
        } else {
            0
        }
    }

    pub fn page_url(page: u32) -> String {
        format!("https://www.kaggle.com/api/v1/competitions/list?page={page}")
    }

    fn fetch(&self) -> Result<Vec<Competition>> {
        let Some(token) = auth_token() else {
            return Ok(Vec::new());
        };
        let bearer = format!("Bearer {token}");
        let mut all = Vec::new();
        for page in 1..=self.pages {
            let resp = self.transport.get(
                &Self::page_url(page),
                &[("Authorization", bearer.as_str()), ("Accept", "application/json")],
            )?;
            if resp.status == 401 || resp.status == 403 {
                anyhow::bail!("kaggle rejected the token (status {})", resp.status);
            }
            if resp.status != 200 {
                anyhow::bail!("kaggle returned status {}", resp.status);
            }
            let page_items = parse_competitions(&resp.body)?;
            let empty = page_items.is_empty();
            all.extend(page_items);
            if empty {
                break;
            }
        }
        Ok(all)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Competition {
    #[serde(default)]
    pub r#ref: String,
    #[serde(default)]
    pub title: String,
    /// Free text: "850,000 Usd", but also "Knowledge", "Swag", "Points".
    #[serde(default)]
    pub reward: String,
    #[serde(default)]
    pub team_count: u64,
    #[serde(default)]
    pub deadline: Option<String>,
    #[serde(default)]
    pub enabled_date: Option<String>,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub submissions_disabled: bool,
}

impl Competition {
    /// Short slug for the competition.
    ///
    /// The API returns `ref` as a full URL rather than the bare slug the docs imply, so
    /// the last path segment is taken. Niche ids end up in tables and filenames; a
    /// 60-character URL in that position is unreadable.
    pub fn slug(&self) -> &str {
        self.r#ref
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(&self.r#ref)
    }

    pub fn niche_id(&self) -> String {
        format!("kaggle:{}", self.slug())
    }

    /// Expected value per entering team, in cents. `None` for non-cash prizes, which
    /// must not be silently scored as zero-value-but-comparable: a Knowledge
    /// competition is a different kind of thing, not a cheap one.
    pub fn cents_per_team(&self) -> Option<f64> {
        let prize = parse_reward_cents(&self.reward)?;
        Some(prize as f64 / self.team_count.max(1) as f64)
    }
}

/// Parse Kaggle's free-text reward field into cents.
///
/// Non-cash rewards ("Knowledge", "Swag", "Points") return `None` rather than zero.
pub fn parse_reward_cents(text: &str) -> Option<u64> {
    let cleaned: String = text
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == ',' || *c == '.')
        .collect();
    let digits: String = cleaned.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    // Only USD amounts are comparable; anything else is left unscored rather than
    // treated as dollars.
    if !text.to_ascii_lowercase().contains("usd") {
        return None;
    }
    digits.parse::<u64>().ok().map(|d| d.saturating_mul(100))
}

pub fn parse_competitions(body: &str) -> Result<Vec<Competition>> {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        r#ref: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        reward: String,
        #[serde(default, rename = "teamCount")]
        team_count: u64,
        #[serde(default)]
        deadline: Option<String>,
        #[serde(default, rename = "enabledDate")]
        enabled_date: Option<String>,
        #[serde(default)]
        category: String,
        #[serde(default, rename = "submissionsDisabled")]
        submissions_disabled: bool,
    }
    let raw: Vec<Raw> = serde_json::from_str(body).context("parsing kaggle competition list")?;
    Ok(raw
        .into_iter()
        .map(|r| Competition {
            r#ref: r.r#ref,
            title: r.title,
            reward: r.reward,
            team_count: r.team_count,
            deadline: r.deadline,
            enabled_date: r.enabled_date,
            category: r.category,
            submissions_disabled: r.submissions_disabled,
        })
        .collect())
}

impl Source for KaggleSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn niches(&self) -> Result<Vec<Niche>> {
        Ok(self
            .fetch()?
            .into_iter()
            .filter(|c| !c.submissions_disabled)
            .map(|c| {
                let closes_ms = c.deadline.as_deref().and_then(parse_rfc3339_utc);
                Niche {
                    id: c.niche_id(),
                    label: c.title.clone(),
                    class: NicheClass::IncentiveProgram,
                    opened_ms: c.enabled_date.as_deref().and_then(parse_rfc3339_utc),
                    first_seen_ms: hl_core::now_millis(),
                    entry_cost: EntryCost {
                        money_cents: 0,
                        requests: 0,
                        // Entry is free; the cost is work, and this is the honest order
                        // of magnitude for a competition attempt rather than a poll.
                        seconds: 6 * 3600,
                    },
                    closes_ms,
                    source_url: Some(if c.r#ref.starts_with("http") {
                        c.r#ref.clone()
                    } else {
                        format!("https://www.kaggle.com/c/{}", c.r#ref)
                    }),
                    notes: format!(
                        "{} teams, reward {}, category {}",
                        c.team_count, c.reward, c.category
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
            .filter(|c| !c.submissions_disabled && c.team_count > 0)
            .map(|c| {
                let mut o = Observation::new(c.niche_id(), now, "kaggle")
                    .competitors(c.team_count as f64);
                // Reward per team is the quantity that actually decays, and it is what
                // an entrant is really paid.
                if let Some(cents) = c.cents_per_team() {
                    o = o.reward(cents.round().max(1.0) as u64);
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

    const PAGE: &str = r#"[
      {"ref":"https://www.kaggle.com/competitions/arc-prize-2026","title":"ARC Prize","reward":"850,000 Usd","teamCount":2382,
       "deadline":"2026-11-02T23:59:00Z","enabledDate":"2026-03-25T17:38:23Z","category":"Featured","submissionsDisabled":false},
      {"ref":"titanic","title":"Titanic","reward":"Knowledge","teamCount":10377,
       "deadline":"2030-01-01T00:00:00Z","enabledDate":"2012-09-28T00:00:00Z","category":"Getting Started","submissionsDisabled":false},
      {"ref":"closed-one","title":"Closed","reward":"1,000 Usd","teamCount":5,
       "deadline":"2026-01-01T00:00:00Z","enabledDate":"2025-01-01T00:00:00Z","category":"Featured","submissionsDisabled":true}
    ]"#;

    fn with_token<T>(f: impl FnOnce() -> T) -> T {
        std::env::set_var("KAGGLE_KEY", "test-token");
        let out = f();
        std::env::remove_var("KAGGLE_KEY");
        out
    }

    fn source(pages: u32) -> KaggleSource {
        let mut t = FixtureTransport::new();
        for p in 1..=pages {
            t = t.with(KaggleSource::page_url(p), 200, if p == 1 { PAGE } else { "[]" });
        }
        let mut s = KaggleSource::new(Box::new(t));
        s.pages = pages;
        s
    }

    #[test]
    fn a_url_shaped_ref_becomes_a_readable_slug() {
        // The API returns `ref` as a full URL, not the bare slug the docs imply.
        let c = parse_competitions(
            r#"[{"ref":"https://www.kaggle.com/competitions/arc-prize-2026-arc-agi-3","teamCount":1}]"#,
        )
        .unwrap();
        assert_eq!(c[0].slug(), "arc-prize-2026-arc-agi-3");
        assert_eq!(c[0].niche_id(), "kaggle:arc-prize-2026-arc-agi-3");

        // A bare slug still works, in case the shape changes back.
        let c = parse_competitions(r#"[{"ref":"titanic","teamCount":1}]"#).unwrap();
        assert_eq!(c[0].niche_id(), "kaggle:titanic");
    }

    #[test]
    fn non_cash_rewards_are_unscored_not_zero() {
        assert_eq!(parse_reward_cents("850,000 Usd"), Some(85_000_000));
        assert_eq!(parse_reward_cents("1,000 Usd"), Some(100_000));
        assert_eq!(parse_reward_cents("Knowledge"), None);
        assert_eq!(parse_reward_cents("Swag"), None);
        assert_eq!(parse_reward_cents("Points"), None);
        // A number without a currency is not dollars.
        assert_eq!(parse_reward_cents("5000 Eur"), None);
    }

    #[test]
    fn expected_value_per_team_is_a_division_not_an_inference() {
        let c = parse_competitions(PAGE).unwrap();
        let arc = &c[0];
        let per_team = arc.cents_per_team().unwrap();
        assert!(
            (per_team - 85_000_000.0 / 2382.0).abs() < 1.0,
            "got {per_team}"
        );
        assert_eq!(c[1].cents_per_team(), None, "Knowledge has no cash value");
    }

    #[test]
    fn deadlines_are_carried_onto_the_niche() {
        with_token(|| {
            let niches = source(2).niches().unwrap();
            let arc = niches.iter().find(|n| n.id == "kaggle:arc-prize-2026").unwrap();
            let closes = arc.closes_ms.expect("a competition has a published deadline");
            assert_eq!(closes, parse_rfc3339_utc("2026-11-02T23:59:00Z").unwrap());
            assert!(arc.opened_ms.is_some());
        });
    }

    #[test]
    fn closed_competitions_are_excluded() {
        with_token(|| {
            let niches = source(2).niches().unwrap();
            assert!(!niches.iter().any(|n| n.id == "kaggle:closed-one"));
            assert_eq!(niches.len(), 2);
        });
    }

    #[test]
    fn observations_carry_team_count_and_reward_per_team() {
        with_token(|| {
            let obs = source(2).observe(0).unwrap();
            let arc = obs.iter().find(|o| o.niche_id == "kaggle:arc-prize-2026").unwrap();
            assert_eq!(arc.competitors, Some(2382.0));
            assert!(arc.reward_cents.unwrap() > 35_000);
            let titanic = obs.iter().find(|o| o.niche_id == "kaggle:titanic").unwrap();
            assert_eq!(titanic.reward_cents, None, "no cash prize, no reward reading");
            assert_eq!(titanic.competitors, Some(10377.0));
        });
    }

    #[test]
    fn without_a_token_the_source_is_silent_rather_than_broken() {
        std::env::remove_var("KAGGLE_KEY");
        std::env::remove_var("KAGGLE_API_TOKEN");
        let s = source(2);
        assert_eq!(s.request_cost(), 0);
        assert!(s.niches().unwrap().is_empty());
        assert!(s.observe(0).unwrap().is_empty());
    }

    #[test]
    fn a_rejected_token_is_reported_clearly() {
        with_token(|| {
            let t = FixtureTransport::new().with(KaggleSource::page_url(1), 401, "Unauthenticated");
            let mut s = KaggleSource::new(Box::new(t));
            s.pages = 1;
            let err = s.observe(0).unwrap_err().to_string();
            assert!(err.contains("rejected the token"), "got: {err}");
        });
    }
}
