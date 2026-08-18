//! Window detection.
//!
//! Niches open on rule changes — a program launches, a protocol deploys, an API opens a
//! free tier. The gap between "announced" and "crowded" is the entire opportunity, so
//! the scout's only job is to notice first and to keep an honest record of how late it
//! was when it didn't.
//!
//! Detection latency is the number this component is judged on, which is why it is
//! measured and persisted rather than assumed.

use anyhow::{Context, Result};
use hl_core::{Niche, Source};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sighting {
    pub niche_id: String,
    /// When we first saw it.
    pub first_seen_ms: u64,
    /// When it actually opened, when the source knows.
    pub opened_ms: Option<u64>,
    /// Published expiry, where the venue states one. Carried here because the report is
    /// generated from stored observations, which have no idea a deadline exists.
    #[serde(default)]
    pub closes_ms: Option<u64>,
    /// Human label, so the report can name a niche rather than print its id.
    #[serde(default)]
    pub label: String,
    pub source: String,
}

impl Sighting {
    pub fn detection_latency_ms(&self) -> Option<u64> {
        self.opened_ms.map(|o| self.first_seen_ms.saturating_sub(o))
    }
}

/// Persistent record of everything the scout has ever seen.
///
/// Without this, every restart rediscovers the whole world as "new", and detection
/// latency — the one metric that matters here — becomes meaningless.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SightingIndex {
    sightings: BTreeMap<String, Sighting>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl SightingIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut idx = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<SightingIndex>(&bytes)
                .with_context(|| format!("parsing sighting index at {}", path.display()))?,
            Err(_) => SightingIndex::default(),
        };
        idx.path = Some(path);
        Ok(idx)
    }

    pub fn save(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn known(&self, niche_id: &str) -> bool {
        self.sightings.contains_key(niche_id)
    }

    pub fn get(&self, niche_id: &str) -> Option<&Sighting> {
        self.sightings.get(niche_id)
    }

    pub fn len(&self) -> usize {
        self.sightings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sightings.is_empty()
    }

    pub fn all(&self) -> impl Iterator<Item = &Sighting> {
        self.sightings.values()
    }

    /// Record a first sighting. Returns `None` if we already knew about it — a niche is
    /// only ever "new" once, however many times it shows up in a feed.
    pub fn record(&mut self, niche: &Niche, source: &str, now_ms: u64) -> Option<Sighting> {
        if self.known(&niche.id) {
            return None;
        }
        let s = Sighting {
            niche_id: niche.id.clone(),
            first_seen_ms: now_ms,
            opened_ms: niche.opened_ms,
            closes_ms: niche.closes_ms,
            label: niche.label.clone(),
            source: source.to_string(),
        };
        self.sightings.insert(niche.id.clone(), s.clone());
        Some(s)
    }

    /// Refresh the mutable facts about a niche we have already seen.
    ///
    /// Deadlines get extended and labels get renamed; first-sighting time never
    /// changes, because that is the measurement this component is judged on.
    pub fn refresh(&mut self, niche: &Niche) {
        if let Some(s) = self.sightings.get_mut(&niche.id) {
            s.closes_ms = niche.closes_ms;
            if !niche.label.is_empty() {
                s.label = niche.label.clone();
            }
        }
    }

    /// Median detection latency across everything we have dated, in milliseconds.
    ///
    /// Median rather than mean: one niche discovered years after it opened would
    /// otherwise swamp the statistic that is supposed to tell us how fast we are.
    pub fn median_detection_latency_ms(&self) -> Option<u64> {
        let mut v: Vec<u64> = self
            .sightings
            .values()
            .filter_map(|s| s.detection_latency_ms())
            .collect();
        if v.is_empty() {
            return None;
        }
        v.sort_unstable();
        Some(v[v.len() / 2])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepReport {
    pub checked: usize,
    pub new_niches: Vec<Sighting>,
    pub errors: Vec<String>,
}

pub struct Scout {
    pub index: SightingIndex,
}

impl Scout {
    pub fn new(index: SightingIndex) -> Self {
        Self { index }
    }

    /// Ask every source what it knows, and record anything unseen.
    ///
    /// A failing source is recorded and stepped over rather than aborting the sweep:
    /// one rate-limited API must not blind us to every other window.
    pub fn sweep(&mut self, sources: &[&dyn Source], now_ms: u64) -> SweepReport {
        let mut report = SweepReport {
            checked: 0,
            new_niches: Vec::new(),
            errors: Vec::new(),
        };
        for src in sources {
            match src.niches() {
                Ok(niches) => {
                    report.checked += niches.len();
                    for n in niches {
                        if let Some(s) = self.index.record(&n, src.id(), now_ms) {
                            report.new_niches.push(s);
                        }
                    }
                }
                Err(e) => report.errors.push(format!("{}: {e}", src.id())),
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_core::{EntryCost, NicheClass, Observation};

    struct StubSource {
        id: String,
        niches: Vec<Niche>,
        fail: bool,
    }

    impl Source for StubSource {
        fn id(&self) -> &str {
            &self.id
        }
        fn niches(&self) -> Result<Vec<Niche>> {
            if self.fail {
                anyhow::bail!("source unavailable");
            }
            Ok(self.niches.clone())
        }
        fn observe(&self, _since_ms: u64) -> Result<Vec<Observation>> {
            Ok(vec![])
        }
    }

    fn niche(id: &str, opened_ms: Option<u64>) -> Niche {
        Niche {
            id: id.into(),
            label: id.into(),
            class: NicheClass::WorkMarket,
            opened_ms,
            first_seen_ms: 0,
            entry_cost: EntryCost::default(),
            closes_ms: None,
            source_url: None,
            notes: String::new(),
        }
    }

    fn stub(id: &str, niches: Vec<Niche>) -> StubSource {
        StubSource {
            id: id.into(),
            niches,
            fail: false,
        }
    }

    #[test]
    fn a_niche_is_new_exactly_once() {
        let mut scout = Scout::new(SightingIndex::new());
        let s = stub("a", vec![niche("n1", Some(0)), niche("n2", None)]);
        let first = scout.sweep(&[&s], 1000);
        assert_eq!(first.new_niches.len(), 2);
        let second = scout.sweep(&[&s], 2000);
        assert_eq!(second.new_niches.len(), 0, "re-seeing is not re-discovering");
        assert_eq!(second.checked, 2);
    }

    #[test]
    fn refreshing_updates_the_deadline_but_never_the_first_sighting() {
        let mut scout = Scout::new(SightingIndex::new());
        let mut n = niche("n1", Some(1_000));
        n.closes_ms = Some(5_000);
        scout.sweep(&[&stub("a", vec![n.clone()])], 2_000);
        assert_eq!(scout.index.get("n1").unwrap().closes_ms, Some(5_000));

        n.closes_ms = Some(9_000);
        scout.index.refresh(&n);
        let s = scout.index.get("n1").unwrap();
        assert_eq!(s.closes_ms, Some(9_000), "an extended deadline must be picked up");
        assert_eq!(s.first_seen_ms, 2_000, "but lateness is not retroactively forgiven");
    }

    #[test]
    fn detection_latency_is_measured_against_the_opening() {
        let mut scout = Scout::new(SightingIndex::new());
        let s = stub("a", vec![niche("n1", Some(1_000))]);
        scout.sweep(&[&s], 4_000);
        assert_eq!(
            scout.index.get("n1").unwrap().detection_latency_ms(),
            Some(3_000)
        );
    }

    #[test]
    fn undated_niches_report_no_latency_rather_than_zero() {
        let mut scout = Scout::new(SightingIndex::new());
        let s = stub("a", vec![niche("n1", None)]);
        scout.sweep(&[&s], 4_000);
        assert_eq!(scout.index.get("n1").unwrap().detection_latency_ms(), None);
        assert_eq!(scout.index.median_detection_latency_ms(), None);
    }

    #[test]
    fn one_broken_source_does_not_blind_the_sweep() {
        let mut scout = Scout::new(SightingIndex::new());
        let good = stub("good", vec![niche("n1", Some(0))]);
        let bad = StubSource {
            id: "bad".into(),
            niches: vec![],
            fail: true,
        };
        let r = scout.sweep(&[&bad, &good], 500);
        assert_eq!(r.new_niches.len(), 1);
        assert_eq!(r.errors.len(), 1);
        assert!(r.errors[0].contains("bad"));
    }

    #[test]
    fn median_latency_ignores_one_ancient_outlier() {
        let mut scout = Scout::new(SightingIndex::new());
        let s = stub(
            "a",
            vec![
                niche("n1", Some(900)),
                niche("n2", Some(800)),
                niche("n3", Some(0)),
            ],
        );
        scout.sweep(&[&s], 1_000);
        // Latencies are 100, 200 and 1000; the median must not be dragged by the last.
        assert_eq!(scout.index.median_detection_latency_ms(), Some(200));
    }

    #[test]
    fn the_index_survives_a_restart() {
        let path = std::env::temp_dir().join(format!("hl-scout-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut scout = Scout::new(SightingIndex::load(&path).unwrap());
        let s = stub("a", vec![niche("n1", Some(0))]);
        scout.sweep(&[&s], 1_000);
        scout.index.save().unwrap();

        let mut reloaded = Scout::new(SightingIndex::load(&path).unwrap());
        let r = reloaded.sweep(&[&s], 2_000);
        assert!(
            r.new_niches.is_empty(),
            "a restart must not rediscover the world as brand new"
        );
        let _ = std::fs::remove_file(&path);
    }
}
