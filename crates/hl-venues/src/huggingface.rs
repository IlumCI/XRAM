//! Tag saturation on the Hugging Face Hub.
//!
//! The cheapest honest crowding measurement available anywhere: one unauthenticated
//! request returns the 100 newest artefacts carrying a tag, and the time span they
//! cover *is* the creation rate. A tag producing 4,000 new models a day is not a niche
//! anyone is going to find room in; one producing 80 might be.
//!
//! What matters is the trend in that rate, not its level — a tag going from 80/day to
//! 200/day is closing, whatever the absolute number.
//!
//! Deliberately no reward metric. Downloads and likes accumulate with age, and a
//! cohort's age shrinks as the creation rate rises, so any reward read from the same
//! request would move with the rate and quietly double-count it. One clean signal beats
//! two confounded ones.

use crate::http::Transport;
use crate::timefmt::parse_rfc3339_utc;
use anyhow::{Context, Result};
use hl_core::{EntryCost, Niche, NicheClass, Observation, Source};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HfKind {
    Models,
    Datasets,
}

impl HfKind {
    fn path(self) -> &'static str {
        match self {
            HfKind::Models => "models",
            HfKind::Datasets => "datasets",
        }
    }
}

#[derive(Debug, Clone)]
pub struct HfNiche {
    pub tag: String,
    pub kind: HfKind,
}

impl HfNiche {
    pub fn models(tag: &str) -> Self {
        Self {
            tag: tag.into(),
            kind: HfKind::Models,
        }
    }
    pub fn datasets(tag: &str) -> Self {
        Self {
            tag: tag.into(),
            kind: HfKind::Datasets,
        }
    }
    pub fn id(&self) -> String {
        format!("hf:{}:{}", self.kind.path(), self.tag)
    }
    pub fn url(&self) -> String {
        format!(
            "https://huggingface.co/api/{}?filter={}&sort=createdAt&direction=-1&limit={PAGE}",
            self.kind.path(),
            self.tag
        )
    }
}

/// One page is enough to measure a rate, and keeps each niche to a single request.
const PAGE: usize = 100;

pub struct HuggingFaceSource {
    pub niches: Vec<HfNiche>,
    transport: Box<dyn Transport>,
    id: String,
}

impl HuggingFaceSource {
    pub fn new(niches: Vec<HfNiche>, transport: Box<dyn Transport>) -> Self {
        Self {
            niches,
            transport,
            id: "huggingface".into(),
        }
    }

    pub fn request_cost(&self) -> u32 {
        self.niches.len() as u32
    }
}

#[derive(Debug, Deserialize)]
struct HfItem {
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
}

impl Source for HuggingFaceSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn niches(&self) -> Result<Vec<Niche>> {
        Ok(self
            .niches
            .iter()
            .map(|n| Niche {
                id: n.id(),
                label: format!("hf {} [{}]", n.kind.path(), n.tag),
                class: NicheClass::DataMarket,
                opened_ms: None,
                first_seen_ms: hl_core::now_millis(),
                entry_cost: EntryCost {
                    money_cents: 0,
                    requests: 1,
                    seconds: 5,
                },
                source_url: Some(format!("https://huggingface.co/{}", n.kind.path())),
                notes: "competitor density is new artefacts per day for this tag".into(),
            })
            .collect())
    }

    fn observe(&self, _since_ms: u64) -> Result<Vec<Observation>> {
        let mut out = Vec::new();
        for n in &self.niches {
            let resp = self.transport.get(&n.url(), &[("Accept", "application/json")])?;
            if resp.status != 200 {
                anyhow::bail!("{}: huggingface returned status {}", n.id(), resp.status);
            }
            if let Some(o) = creation_rate(&resp.body, &n.id(), self.id())? {
                out.push(o);
            }
        }
        Ok(out)
    }
}

/// Derive new-artefacts-per-day from one page of newest-first results.
///
/// Stamped at the newest creation time rather than at poll time: the rate is a property
/// of that moment, and it makes repeated polls of a quiet tag deduplicate naturally —
/// if nothing new was created, there is genuinely nothing new to record.
pub fn creation_rate(body: &str, niche_id: &str, source: &str) -> Result<Option<Observation>> {
    let items: Vec<HfItem> =
        serde_json::from_str(body).context("parsing huggingface listing")?;
    let stamps: Vec<u64> = items
        .iter()
        .filter_map(|i| i.created_at.as_deref().and_then(parse_rfc3339_utc))
        .collect();
    // Two points define no interval worth trusting, and one defines none at all.
    if stamps.len() < 3 {
        return Ok(None);
    }
    let newest = *stamps.iter().max().unwrap();
    let oldest = *stamps.iter().min().unwrap();
    let span_days = newest.saturating_sub(oldest) as f64 / hl_core::MS_PER_DAY;
    if span_days <= 0.0 {
        return Ok(None);
    }
    let rate = (stamps.len() - 1) as f64 / span_days;
    Ok(Some(
        Observation::new(niche_id, newest, source).competitors(rate),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::FixtureTransport;

    /// Ten artefacts spread over exactly two days: nine intervals, so 4.5/day.
    fn page(n: usize, span_days: f64) -> String {
        // Newest first, walking backwards in time, exactly as the API returns them.
        let base = 1_787_000_000_000u64;
        let step = (span_days * hl_core::MS_PER_DAY) as u64 / (n as u64 - 1).max(1);
        let items: Vec<String> = (0..n)
            .map(|i| {
                let ms = base - i as u64 * step;
                let secs = ms / 1000;
                let d = time_of(secs);
                format!("{{\"id\":\"m{i}\",\"createdAt\":\"{d}\"}}")
            })
            .collect();
        format!("[{}]", items.join(","))
    }

    fn time_of(secs: u64) -> String {
        // Minimal inverse of the parser, good enough for fixtures.
        let days = secs / 86_400;
        let rem = secs % 86_400;
        let (y, m, d) = civil_from_days(days as i64);
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z",
            rem / 3600,
            (rem % 3600) / 60,
            rem % 60
        )
    }

    fn civil_from_days(z: i64) -> (i64, u32, u32) {
        let z = z + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        (if m <= 2 { y + 1 } else { y }, m, d)
    }

    #[test]
    fn rate_is_intervals_over_span() {
        let o = creation_rate(&page(10, 2.0), "n", "hf").unwrap().unwrap();
        let rate = o.competitors.unwrap();
        assert!((rate - 4.5).abs() < 0.01, "expected 4.5/day, got {rate}");
    }

    #[test]
    fn a_busier_tag_reports_a_higher_rate() {
        let quiet = creation_rate(&page(100, 10.0), "n", "hf").unwrap().unwrap();
        let busy = creation_rate(&page(100, 0.1), "n", "hf").unwrap().unwrap();
        assert!(busy.competitors.unwrap() > quiet.competitors.unwrap() * 50.0);
    }

    #[test]
    fn observation_is_stamped_at_the_newest_artefact() {
        let body = page(10, 2.0);
        let o = creation_rate(&body, "n", "hf").unwrap().unwrap();
        // Newest is the base timestamp used by the fixture builder.
        assert_eq!(o.ts_ms, 1_787_000_000_000);
    }

    #[test]
    fn a_quiet_tag_repolled_produces_nothing_new() {
        // Same page twice yields an identical observation, which the store dedupes.
        let body = page(10, 2.0);
        let a = creation_rate(&body, "n", "hf").unwrap().unwrap();
        let b = creation_rate(&body, "n", "hf").unwrap().unwrap();
        assert_eq!(
            hl_core::observation_key(&a),
            hl_core::observation_key(&b),
            "an unchanged tag must not manufacture a new data point"
        );
    }

    #[test]
    fn too_few_items_yields_nothing_rather_than_a_guess() {
        assert!(creation_rate("[]", "n", "hf").unwrap().is_none());
        assert!(creation_rate(&page(2, 1.0), "n", "hf").unwrap().is_none());
    }

    #[test]
    fn items_without_timestamps_are_skipped() {
        let body = r#"[{"id":"a"},{"id":"b"},{"id":"c"}]"#;
        assert!(creation_rate(body, "n", "hf").unwrap().is_none());
    }

    #[test]
    fn a_poll_costs_one_request_per_tag() {
        let a = HfNiche::models("robotics");
        let b = HfNiche::datasets("audio");
        let t = FixtureTransport::new()
            .with(a.url(), 200, page(10, 2.0))
            .with(b.url(), 200, page(10, 4.0));
        let src = HuggingFaceSource::new(vec![a, b], Box::new(t));
        assert_eq!(src.request_cost(), 2);
        assert_eq!(src.observe(0).unwrap().len(), 2);
    }

    #[test]
    fn urls_are_distinct_per_kind() {
        assert_ne!(HfNiche::models("x").url(), HfNiche::datasets("x").url());
        assert_ne!(HfNiche::models("x").id(), HfNiche::datasets("x").id());
    }
}
