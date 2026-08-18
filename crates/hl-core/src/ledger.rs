//! Append-only, hash-chained ledger.
//!
//! Every stream's economics are only as trustworthy as the record of what we sent and
//! what came back. Records are chained so a silently edited history is detectable —
//! including by us, later, when we are tempted to believe a stream did better than it
//! did.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum LedgerEvent {
    /// A window was spotted. `detection_latency_ms` is how late we were, and it is the
    /// number the scout is judged on.
    NicheDiscovered {
        niche_id: String,
        class: String,
        detection_latency_ms: Option<u64>,
        source: String,
    },
    /// A batch of measurements folded into the estimator.
    Measured {
        niche_id: String,
        samples: usize,
        crowding_index: f64,
        half_life_days: Option<f64>,
    },
    /// The estimator changed its mind. Recorded so we can audit, later, whether we
    /// actually left when we said we would.
    SignalChanged {
        niche_id: String,
        from: String,
        to: String,
        reason: String,
    },
    Entered {
        niche_id: String,
    },
    Exited {
        niche_id: String,
        reason: String,
    },
    /// Effort spent. Money is tracked separately from quota because with no capital
    /// they are not interchangeable.
    Spent {
        niche_id: String,
        money_cents: u64,
        requests: u32,
        seconds: u64,
    },
    /// Money actually received. The only line that counts.
    Earned {
        niche_id: String,
        cents: u64,
        note: String,
    },
}

impl LedgerEvent {
    pub fn niche_id(&self) -> &str {
        match self {
            LedgerEvent::NicheDiscovered { niche_id, .. }
            | LedgerEvent::Measured { niche_id, .. }
            | LedgerEvent::SignalChanged { niche_id, .. }
            | LedgerEvent::Entered { niche_id }
            | LedgerEvent::Exited { niche_id, .. }
            | LedgerEvent::Spent { niche_id, .. }
            | LedgerEvent::Earned { niche_id, .. } => niche_id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerRecord {
    pub seq: u64,
    pub ts_ms: u64,
    /// Hash of the previous record; the genesis record chains from 64 zeroes.
    pub prev: String,
    pub hash: String,
    pub event: LedgerEvent,
}

pub const GENESIS_PREV: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn record_hash(seq: u64, ts_ms: u64, prev: &str, event: &LedgerEvent) -> Result<String> {
    let body = serde_json::to_string(event).context("serialising ledger event")?;
    Ok(crate::sha256_hex(
        format!("{seq}\u{1f}{ts_ms}\u{1f}{prev}\u{1f}{body}").as_bytes(),
    ))
}

pub struct Ledger {
    path: PathBuf,
}

impl Ledger {
    pub fn open(path: impl AsRef<Path>) -> Result<Ledger> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        Ok(Ledger { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, event: LedgerEvent) -> Result<LedgerRecord> {
        let records = self.read_all()?;
        let (seq, prev) = match records.last() {
            Some(r) => (r.seq + 1, r.hash.clone()),
            None => (0, GENESIS_PREV.to_string()),
        };
        let ts_ms = crate::now_millis();
        let hash = record_hash(seq, ts_ms, &prev, &event)?;
        let rec = LedgerRecord {
            seq,
            ts_ms,
            prev,
            hash,
            event,
        };
        let line = serde_json::to_string(&rec)?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening ledger at {}", self.path.display()))?;
        writeln!(f, "{line}")?;
        f.flush()?;
        Ok(rec)
    }

    pub fn read_all(&self) -> Result<Vec<LedgerRecord>> {
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
            let rec: LedgerRecord = serde_json::from_str(&line)
                .with_context(|| format!("parsing ledger line {}", i + 1))?;
            out.push(rec);
        }
        Ok(out)
    }

    /// Recompute the chain. Returns the number of records verified.
    pub fn verify_chain(&self) -> Result<usize> {
        let records = self.read_all()?;
        let mut prev = GENESIS_PREV.to_string();
        for (i, r) in records.iter().enumerate() {
            if r.seq != i as u64 {
                bail!("ledger record {i} has seq {}, expected {i}", r.seq);
            }
            if r.prev != prev {
                bail!("ledger record {i} does not chain to its predecessor");
            }
            let expect = record_hash(r.seq, r.ts_ms, &r.prev, &r.event)?;
            if expect != r.hash {
                bail!("ledger record {i} hash mismatch: content was modified");
            }
            prev = r.hash.clone();
        }
        Ok(records.len())
    }

    /// Realised economics per niche. This table is the whole point: it is what tells
    /// us which streams to keep, which to harvest, and which to abandon.
    pub fn yield_by_niche(&self) -> Result<BTreeMap<String, NicheYield>> {
        let mut out: BTreeMap<String, NicheYield> = BTreeMap::new();
        for r in self.read_all()? {
            let e = out.entry(r.event.niche_id().to_string()).or_default();
            match &r.event {
                LedgerEvent::NicheDiscovered {
                    detection_latency_ms,
                    ..
                } => {
                    e.discovered_ms = Some(r.ts_ms);
                    e.detection_latency_ms = *detection_latency_ms;
                }
                LedgerEvent::Entered { .. } => {
                    e.entered_ms = Some(r.ts_ms);
                }
                LedgerEvent::Exited { .. } => {
                    e.exited_ms = Some(r.ts_ms);
                }
                LedgerEvent::Spent {
                    money_cents,
                    requests,
                    seconds,
                    ..
                } => {
                    e.spent_cents += money_cents;
                    e.spent_requests += *requests as u64;
                    e.spent_seconds += seconds;
                }
                LedgerEvent::Earned { cents, .. } => {
                    e.earned_cents += cents;
                }
                _ => {}
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NicheYield {
    pub discovered_ms: Option<u64>,
    pub detection_latency_ms: Option<u64>,
    pub entered_ms: Option<u64>,
    pub exited_ms: Option<u64>,
    pub spent_cents: u64,
    pub spent_requests: u64,
    pub spent_seconds: u64,
    pub earned_cents: u64,
}

impl NicheYield {
    /// Net cents. Negative means the niche cost more than it returned, which is the
    /// normal case and must stay visible rather than being rounded away.
    pub fn net_cents(&self) -> i64 {
        self.earned_cents as i64 - self.spent_cents as i64
    }

    /// Cents earned per free-tier request spent. `None` when nothing was spent, so a
    /// niche that earned by luck without effort does not read as infinitely efficient.
    pub fn cents_per_request(&self) -> Option<f64> {
        if self.spent_requests == 0 {
            None
        } else {
            Some(self.earned_cents as f64 / self.spent_requests as f64)
        }
    }

    /// How long we stayed, in days.
    pub fn tenure_days(&self, now_ms: u64) -> Option<f64> {
        let start = self.entered_ms?;
        let end = self.exited_ms.unwrap_or(now_ms);
        Some(end.saturating_sub(start) as f64 / crate::MS_PER_DAY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hl-ledger-{}-{name}.jsonl", std::process::id()))
    }

    fn discovered(id: &str) -> LedgerEvent {
        LedgerEvent::NicheDiscovered {
            niche_id: id.into(),
            class: "work_market".into(),
            detection_latency_ms: Some(1200),
            source: "sim".into(),
        }
    }

    #[test]
    fn chain_verifies_and_detects_tampering() {
        let path = tmp("chain");
        let _ = std::fs::remove_file(&path);
        let l = Ledger::open(&path).unwrap();
        for i in 0..5 {
            l.append(discovered(&format!("n{i}"))).unwrap();
        }
        assert_eq!(l.verify_chain().unwrap(), 5);

        let text = std::fs::read_to_string(&path).unwrap();
        let tampered = text.replacen("\"detection_latency_ms\":1200", "\"detection_latency_ms\":1", 1);
        assert_ne!(text, tampered, "test fixture must actually change something");
        std::fs::write(&path, tampered).unwrap();
        assert!(
            l.verify_chain().is_err(),
            "editing a past record must break the chain"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn yield_table_nets_spend_against_earnings() {
        let path = tmp("yield");
        let _ = std::fs::remove_file(&path);
        let l = Ledger::open(&path).unwrap();
        l.append(discovered("n1")).unwrap();
        l.append(LedgerEvent::Entered { niche_id: "n1".into() }).unwrap();
        l.append(LedgerEvent::Spent {
            niche_id: "n1".into(),
            money_cents: 0,
            requests: 40,
            seconds: 600,
        })
        .unwrap();
        l.append(LedgerEvent::Earned {
            niche_id: "n1".into(),
            cents: 250,
            note: "bounty".into(),
        })
        .unwrap();

        let y = l.yield_by_niche().unwrap();
        let n = &y["n1"];
        assert_eq!(n.earned_cents, 250);
        assert_eq!(n.net_cents(), 250);
        assert_eq!(n.cents_per_request(), Some(6.25));
        assert_eq!(n.detection_latency_ms, Some(1200));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_losing_niche_reports_a_loss() {
        let path = tmp("loss");
        let _ = std::fs::remove_file(&path);
        let l = Ledger::open(&path).unwrap();
        l.append(LedgerEvent::Spent {
            niche_id: "n2".into(),
            money_cents: 500,
            requests: 10,
            seconds: 60,
        })
        .unwrap();
        l.append(LedgerEvent::Earned {
            niche_id: "n2".into(),
            cents: 100,
            note: String::new(),
        })
        .unwrap();
        assert_eq!(l.yield_by_niche().unwrap()["n2"].net_cents(), -400);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn effort_without_earnings_is_not_infinitely_efficient() {
        let path = tmp("noearn");
        let _ = std::fs::remove_file(&path);
        let l = Ledger::open(&path).unwrap();
        l.append(LedgerEvent::Earned {
            niche_id: "n3".into(),
            cents: 100,
            note: String::new(),
        })
        .unwrap();
        assert_eq!(l.yield_by_niche().unwrap()["n3"].cents_per_request(), None);
        let _ = std::fs::remove_file(&path);
    }
}
