//! Append-only observation store.
//!
//! The estimator needs history, and history only exists if it survives the process.
//! Every sweep appends here and the next one resumes from where it stopped.
//!
//! Deduplication is the load-bearing part. Polls overlap by design — a source is asked
//! for everything since the last timestamp, and boundaries are inclusive so nothing is
//! missed — which means the same observation arrives repeatedly. Stacking duplicates at
//! one timestamp would quietly weight that moment more heavily than the rest of the
//! series and bias every fit that follows.

use crate::Observation;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashSet};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

pub struct ObservationStore {
    path: PathBuf,
    seen: HashSet<String>,
}

/// Stable identity of an observation: same measurement, same key, regardless of when
/// it was collected or how many times it has been seen.
///
/// Metric values are part of the key. A source that legitimately re-reports a changed
/// value for the same instant (an issue gaining a comment) is a genuinely new
/// measurement, and dropping it would lose real signal.
pub fn observation_key(o: &Observation) -> String {
    let f = |v: Option<f64>| v.map(|x| format!("{x:.6}")).unwrap_or_default();
    crate::sha256_hex(
        format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            o.niche_id,
            o.ts_ms,
            o.source,
            o.claim_latency_ms.map(|v| v.to_string()).unwrap_or_default(),
            f(o.competitors),
            o.reward_cents.map(|v| v.to_string()).unwrap_or_default(),
            f(o.acceptance),
        )
        .as_bytes(),
    )
}

impl ObservationStore {
    /// Open the store, loading existing keys so appends can deduplicate.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut store = Self {
            path,
            seen: HashSet::new(),
        };
        for o in store.read_all()? {
            store.seen.insert(observation_key(&o));
        }
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    /// Append observations that are not already stored. Returns how many were new.
    pub fn append(&mut self, observations: &[Observation]) -> Result<usize> {
        let fresh: Vec<&Observation> = observations
            .iter()
            .filter(|o| !self.seen.contains(&observation_key(o)))
            .collect();
        if fresh.is_empty() {
            return Ok(0);
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening store at {}", self.path.display()))?;
        for o in &fresh {
            writeln!(f, "{}", serde_json::to_string(o)?)?;
        }
        f.flush()?;
        for o in &fresh {
            self.seen.insert(observation_key(o));
        }
        Ok(fresh.len())
    }

    pub fn read_all(&self) -> Result<Vec<Observation>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let f = std::fs::File::open(&self.path)?;
        let mut out = Vec::new();
        for (i, line) in BufReader::new(f).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            // One corrupt line — a truncated write from a killed job — must not discard
            // every observation collected before it.
            match serde_json::from_str::<Observation>(&line) {
                Ok(o) => out.push(o),
                Err(e) => eprintln!("store: skipping unreadable line {}: {e}", i + 1),
            }
        }
        Ok(out)
    }

    pub fn read_for(&self, niche_id: &str) -> Result<Vec<Observation>> {
        Ok(self
            .read_all()?
            .into_iter()
            .filter(|o| o.niche_id == niche_id)
            .collect())
    }

    /// Latest timestamp seen per source, for resuming a poll.
    pub fn resume_points(&self) -> Result<BTreeMap<String, u64>> {
        let mut out: BTreeMap<String, u64> = BTreeMap::new();
        for o in self.read_all()? {
            let e = out.entry(o.source.clone()).or_insert(0);
            *e = (*e).max(o.ts_ms);
        }
        Ok(out)
    }

    /// Every niche the store has data for.
    pub fn niche_ids(&self) -> Result<Vec<String>> {
        let mut v: Vec<String> = self
            .read_all()?
            .into_iter()
            .map(|o| o.niche_id)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        v.sort();
        Ok(v)
    }

    /// Drop observations older than `cutoff_ms`, rewriting the file.
    ///
    /// Unbounded growth is a real failure mode for something that appends hourly
    /// forever, and old points cannot influence a trend we care about anyway.
    pub fn prune(&mut self, cutoff_ms: u64) -> Result<usize> {
        let all = self.read_all()?;
        let keep: Vec<Observation> = all.iter().filter(|o| o.ts_ms >= cutoff_ms).cloned().collect();
        let dropped = all.len() - keep.len();
        if dropped == 0 {
            return Ok(0);
        }
        let tmp = self.path.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            for o in &keep {
                writeln!(f, "{}", serde_json::to_string(o)?)?;
            }
            f.flush()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        self.seen = keep.iter().map(observation_key).collect();
        Ok(dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hl-store-{}-{name}.jsonl", std::process::id()))
    }

    fn obs(niche: &str, ts: u64, reward: u64) -> Observation {
        Observation::new(niche, ts, "src").reward(reward)
    }

    #[test]
    fn appends_and_reads_back() {
        let path = tmp("basic");
        let _ = std::fs::remove_file(&path);
        let mut s = ObservationStore::open(&path).unwrap();
        assert_eq!(s.append(&[obs("n", 1, 10), obs("n", 2, 20)]).unwrap(), 2);
        assert_eq!(s.read_all().unwrap().len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn overlapping_polls_do_not_stack_duplicates() {
        let path = tmp("dedupe");
        let _ = std::fs::remove_file(&path);
        let mut s = ObservationStore::open(&path).unwrap();
        let batch = vec![obs("n", 1, 10), obs("n", 2, 20)];
        assert_eq!(s.append(&batch).unwrap(), 2);
        // The same poll again, as happens on every inclusive-boundary resume.
        assert_eq!(s.append(&batch).unwrap(), 0);
        assert_eq!(s.append(&[obs("n", 3, 30)]).unwrap(), 1);
        assert_eq!(s.read_all().unwrap().len(), 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_changed_value_at_the_same_instant_is_a_new_measurement() {
        let path = tmp("changed");
        let _ = std::fs::remove_file(&path);
        let mut s = ObservationStore::open(&path).unwrap();
        s.append(&[obs("n", 1, 10)]).unwrap();
        assert_eq!(
            s.append(&[obs("n", 1, 99)]).unwrap(),
            1,
            "same instant, different reading: real signal, not a duplicate"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn dedupe_survives_reopening() {
        let path = tmp("reopen");
        let _ = std::fs::remove_file(&path);
        let mut s = ObservationStore::open(&path).unwrap();
        s.append(&[obs("n", 1, 10)]).unwrap();
        drop(s);
        let mut s2 = ObservationStore::open(&path).unwrap();
        assert_eq!(
            s2.append(&[obs("n", 1, 10)]).unwrap(),
            0,
            "a scheduled job is a fresh process every time"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn resume_points_are_per_source() {
        let path = tmp("resume");
        let _ = std::fs::remove_file(&path);
        let mut s = ObservationStore::open(&path).unwrap();
        s.append(&[
            Observation::new("a", 100, "github").reward(1),
            Observation::new("b", 500, "hf").reward(1),
            Observation::new("c", 300, "github").reward(1),
        ])
        .unwrap();
        let r = s.resume_points().unwrap();
        assert_eq!(r["github"], 300);
        assert_eq!(r["hf"], 500);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_corrupt_line_does_not_destroy_the_series() {
        let path = tmp("corrupt");
        let _ = std::fs::remove_file(&path);
        let mut s = ObservationStore::open(&path).unwrap();
        s.append(&[obs("n", 1, 10), obs("n", 2, 20)]).unwrap();
        // Simulate a job killed mid-write.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{{\"niche_id\":\"n\",\"ts_ms\":").unwrap();
        drop(f);
        assert_eq!(ObservationStore::open(&path).unwrap().read_all().unwrap().len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pruning_bounds_growth_and_resets_dedupe() {
        let path = tmp("prune");
        let _ = std::fs::remove_file(&path);
        let mut s = ObservationStore::open(&path).unwrap();
        s.append(&[obs("n", 1, 10), obs("n", 100, 20), obs("n", 200, 30)])
            .unwrap();
        assert_eq!(s.prune(100).unwrap(), 1);
        assert_eq!(s.read_all().unwrap().len(), 2);
        // The pruned observation is forgotten, so it can be re-collected if it reappears.
        assert_eq!(s.append(&[obs("n", 1, 10)]).unwrap(), 1);
        let _ = std::fs::remove_file(&path);
    }
}
