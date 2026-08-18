//! Quota governor.
//!
//! The system runs on free tiers. That is only true if it is *structurally incapable*
//! of exceeding them, rather than merely intending not to. Every metered call goes
//! through [`Governor::acquire`], counters are persisted, and the reservation is taken
//! before the call rather than after — so a crash mid-flight costs quota, never money.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Free-tier shape of one provider. Numbers live in config, not in code, because
/// providers change them.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct QuotaLimits {
    pub requests_per_minute: u32,
    pub requests_per_day: u32,
    pub tokens_per_day: u64,
}

impl QuotaLimits {
    pub fn new(requests_per_minute: u32, requests_per_day: u32, tokens_per_day: u64) -> Self {
        Self {
            requests_per_minute,
            requests_per_day,
            tokens_per_day,
        }
    }
    /// A limit set that forbids everything. Used as the default for unknown providers:
    /// an unconfigured provider cannot be called at all.
    pub const DENY_ALL: QuotaLimits = QuotaLimits {
        requests_per_minute: 0,
        requests_per_day: 0,
        tokens_per_day: 0,
    };
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProviderState {
    day: u64,
    minute: u64,
    requests_today: u32,
    requests_this_minute: u32,
    tokens_today: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuotaDenied {
    UnknownProvider(String),
    MinuteLimit { provider: String, limit: u32 },
    DayLimit { provider: String, limit: u32 },
    TokenLimit { provider: String, limit: u64, would_be: u64 },
}

impl std::fmt::Display for QuotaDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuotaDenied::UnknownProvider(p) => {
                write!(f, "provider '{p}' has no configured quota; refusing to call it")
            }
            QuotaDenied::MinuteLimit { provider, limit } => {
                write!(f, "{provider}: per-minute limit of {limit} reached")
            }
            QuotaDenied::DayLimit { provider, limit } => {
                write!(f, "{provider}: daily limit of {limit} requests reached")
            }
            QuotaDenied::TokenLimit {
                provider,
                limit,
                would_be,
            } => write!(
                f,
                "{provider}: daily token limit {limit} would be exceeded ({would_be})"
            ),
        }
    }
}

impl std::error::Error for QuotaDenied {}

/// Proof that quota was reserved for one call.
#[derive(Debug)]
pub struct Permit {
    pub provider: String,
    pub reserved_tokens: u64,
}

type ClockFn = Box<dyn Fn() -> u64 + Send + Sync>;

pub struct Governor {
    limits: HashMap<String, QuotaLimits>,
    state: Mutex<HashMap<String, ProviderState>>,
    path: Option<PathBuf>,
    clock: ClockFn,
}

impl Governor {
    pub fn new(limits: HashMap<String, QuotaLimits>) -> Self {
        Self {
            limits,
            state: Mutex::new(HashMap::new()),
            path: None,
            clock: Box::new(crate::now_millis),
        }
    }

    /// Persist counters to `path`, loading any existing state. Without this, restarting
    /// the process resets the day counter, which is exactly how a free tier turns into
    /// a bill.
    pub fn persisted(mut self, path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(loaded) = serde_json::from_slice::<HashMap<String, ProviderState>>(&bytes) {
                *self.state.get_mut().unwrap() = loaded;
            }
        }
        self.path = Some(path);
        self
    }

    /// Replace the clock. Tests use this to fast-forward without sleeping.
    pub fn with_clock(mut self, clock: ClockFn) -> Self {
        self.clock = clock;
        self
    }

    pub fn limits_for(&self, provider: &str) -> QuotaLimits {
        self.limits
            .get(provider)
            .copied()
            .unwrap_or(QuotaLimits::DENY_ALL)
    }

    /// Reserve one request and `est_tokens` of budget, or refuse.
    pub fn acquire(&self, provider: &str, est_tokens: u64) -> Result<Permit, QuotaDenied> {
        let limits = match self.limits.get(provider) {
            Some(l) => *l,
            None => return Err(QuotaDenied::UnknownProvider(provider.to_string())),
        };
        let now = (self.clock)();
        let day = now / 86_400_000;
        let minute = now / 60_000;

        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let st = guard.entry(provider.to_string()).or_default();
        if st.day != day {
            st.day = day;
            st.requests_today = 0;
            st.tokens_today = 0;
        }
        if st.minute != minute {
            st.minute = minute;
            st.requests_this_minute = 0;
        }

        if st.requests_this_minute >= limits.requests_per_minute {
            return Err(QuotaDenied::MinuteLimit {
                provider: provider.to_string(),
                limit: limits.requests_per_minute,
            });
        }
        if st.requests_today >= limits.requests_per_day {
            return Err(QuotaDenied::DayLimit {
                provider: provider.to_string(),
                limit: limits.requests_per_day,
            });
        }
        let would_be = st.tokens_today.saturating_add(est_tokens);
        if would_be > limits.tokens_per_day {
            return Err(QuotaDenied::TokenLimit {
                provider: provider.to_string(),
                limit: limits.tokens_per_day,
                would_be,
            });
        }

        st.requests_this_minute += 1;
        st.requests_today += 1;
        st.tokens_today = would_be;
        let snapshot = guard.clone();
        drop(guard);
        self.persist(&snapshot);

        Ok(Permit {
            provider: provider.to_string(),
            reserved_tokens: est_tokens,
        })
    }

    /// Reconcile a reservation against what the call actually consumed.
    ///
    /// Only ever adjusts *upwards* beyond the reservation when the call overran, and
    /// releases the unused remainder when it underran. Either way the day counter ends
    /// up reflecting reality.
    pub fn settle(&self, permit: Permit, actual_tokens: u64) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(st) = guard.get_mut(&permit.provider) {
            st.tokens_today = st
                .tokens_today
                .saturating_sub(permit.reserved_tokens)
                .saturating_add(actual_tokens);
        }
        let snapshot = guard.clone();
        drop(guard);
        self.persist(&snapshot);
    }

    pub fn tokens_used_today(&self, provider: &str) -> u64 {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(provider).map(|s| s.tokens_today).unwrap_or(0)
    }

    pub fn requests_today(&self, provider: &str) -> u32 {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(provider).map(|s| s.requests_today).unwrap_or(0)
    }

    fn persist(&self, snapshot: &HashMap<String, ProviderState>) {
        let Some(path) = &self.path else { return };
        let Ok(bytes) = serde_json::to_vec_pretty(snapshot) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Write-then-rename so a crash mid-write cannot corrupt the counters into
        // something that reads as "plenty of quota left".
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    fn gov(limits: QuotaLimits, clock: Arc<AtomicU64>) -> Governor {
        let mut m = HashMap::new();
        m.insert("p".to_string(), limits);
        Governor::new(m).with_clock(Box::new(move || clock.load(Ordering::SeqCst)))
    }

    #[test]
    fn unknown_provider_is_refused() {
        let g = Governor::new(HashMap::new());
        assert!(matches!(
            g.acquire("nope", 1),
            Err(QuotaDenied::UnknownProvider(p)) if p == "nope"
        ));
    }

    #[test]
    fn minute_limit_holds_under_flood() {
        let clock = Arc::new(AtomicU64::new(0));
        let g = gov(QuotaLimits::new(3, 1000, u64::MAX), clock.clone());
        let ok = (0..100).filter(|_| g.acquire("p", 1).is_ok()).count();
        assert_eq!(ok, 3, "per-minute limit must bind regardless of demand");
        clock.store(60_001, Ordering::SeqCst);
        assert!(g.acquire("p", 1).is_ok(), "next minute refills");
    }

    #[test]
    fn day_limit_holds_across_minutes() {
        let clock = Arc::new(AtomicU64::new(0));
        let g = gov(QuotaLimits::new(10, 5, u64::MAX), clock.clone());
        let mut ok = 0;
        for m in 0..20 {
            clock.store(m * 60_001, Ordering::SeqCst);
            if g.acquire("p", 1).is_ok() {
                ok += 1;
            }
        }
        assert_eq!(ok, 5);
    }

    #[test]
    fn token_limit_is_checked_before_the_call() {
        let clock = Arc::new(AtomicU64::new(0));
        let g = gov(QuotaLimits::new(100, 100, 1000), clock);
        assert!(g.acquire("p", 900).is_ok());
        assert!(matches!(
            g.acquire("p", 200),
            Err(QuotaDenied::TokenLimit { .. })
        ));
        assert!(g.acquire("p", 100).is_ok());
    }

    #[test]
    fn settle_reconciles_over_and_under_estimates() {
        let clock = Arc::new(AtomicU64::new(0));
        let g = gov(QuotaLimits::new(100, 100, 10_000), clock);
        let p = g.acquire("p", 500).unwrap();
        g.settle(p, 120);
        assert_eq!(g.tokens_used_today("p"), 120);
        let p = g.acquire("p", 100).unwrap();
        g.settle(p, 400);
        assert_eq!(g.tokens_used_today("p"), 520);
    }

    #[test]
    fn restart_cannot_reset_the_daily_counter() {
        let dir = std::env::temp_dir().join(format!("swarm-gov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("quota.json");
        let mut m = HashMap::new();
        m.insert("p".to_string(), QuotaLimits::new(100, 3, u64::MAX));

        let g = Governor::new(m.clone()).persisted(&path);
        for _ in 0..3 {
            g.acquire("p", 1).unwrap();
        }
        drop(g);

        let g2 = Governor::new(m).persisted(&path);
        assert!(
            matches!(g2.acquire("p", 1), Err(QuotaDenied::DayLimit { .. })),
            "a restart must not hand back a fresh daily allowance"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
