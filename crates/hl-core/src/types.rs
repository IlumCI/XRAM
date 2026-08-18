//! Domain model for Halflife.
//!
//! The unit of interest is not an opportunity, it is a *niche*: a place where
//! opportunities keep appearing, which has a finite and measurable lifespan. We never
//! try to own a niche. We measure how fast it is closing and leave before it does.

use serde::{Deserialize, Serialize};

/// A place where paid opportunities recur. One niche is one income stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Niche {
    pub id: String,
    pub label: String,
    pub class: NicheClass,
    /// When the niche itself came into existence, as best we can tell — a program
    /// launch, a protocol deployment, a policy change. The clock that matters.
    pub opened_ms: Option<u64>,
    /// When we first noticed it. `first_seen_ms - opened_ms` is our detection latency,
    /// and detection latency is the entire product.
    pub first_seen_ms: u64,
    pub entry_cost: EntryCost,
    /// A known hard expiry, where the venue publishes one — a competition deadline, a
    /// programme end date.
    ///
    /// This is categorically different from the statistical runway the meter estimates:
    /// no amount of stable measurement tells you a contest closes on Tuesday. Without
    /// it, a freshly-launched niche with three days left reads as "no measured erosion,
    /// enter" — the most avoidable mistake this system could make.
    pub closes_ms: Option<u64>,
    pub source_url: Option<String>,
    pub notes: String,
}

impl Niche {
    /// How late we were to the window, in milliseconds.
    pub fn detection_latency_ms(&self) -> Option<u64> {
        self.opened_ms.map(|o| self.first_seen_ms.saturating_sub(o))
    }

    /// Days remaining until the published expiry, if there is one. Negative values are
    /// clamped to zero: a closed window is closed, not retroactively open.
    pub fn days_until_close(&self, now_ms: u64) -> Option<f64> {
        self.closes_ms
            .map(|c| c.saturating_sub(now_ms) as f64 / crate::MS_PER_DAY)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NicheClass {
    /// Work is published and paid per unit delivered.
    WorkMarket,
    /// A program pays for participation over a window.
    IncentiveProgram,
    /// Data or content is bought.
    DataMarket,
    /// Metered access with a free allowance worth arbitraging.
    ApiQuota,
    Other(String),
}

/// What it costs us to be present in a niche at all. With no capital, anything with a
/// non-trivial monetary floor is out of reach, and the policy needs to know that rather
/// than discover it at submission time.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct EntryCost {
    pub money_cents: u64,
    /// Requests against a metered free tier, per attempt.
    pub requests: u32,
    /// Wall-clock we must spend per attempt.
    pub seconds: u64,
}

impl EntryCost {
    pub fn is_free(&self) -> bool {
        self.money_cents == 0
    }
}

/// One measurement of a niche at a point in time.
///
/// Every field is optional because venues differ in what they expose, and a partially
/// observable niche is still worth tracking. The estimator refuses to emit a signal it
/// cannot support rather than filling gaps with assumptions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub niche_id: String,
    pub ts_ms: u64,
    /// Time between an opportunity appearing and someone claiming it. The primary
    /// crowding signal: when this collapses, machines have arrived.
    pub claim_latency_ms: Option<u64>,
    /// Competitor density. Either a count on a single opportunity (comments on an
    /// issue) or a rate (new entrants per day in a category) — both answer "how many
    /// others are chasing this", and the meter only ever fits the trend, never the
    /// absolute level, so the two are interchangeable within a niche.
    pub competitors: Option<f64>,
    /// Advertised or realised reward, in cents.
    pub reward_cents: Option<u64>,
    /// Realised acceptance rate, when the venue tells us.
    pub acceptance: Option<f64>,
    pub source: String,
}

impl Observation {
    pub fn new(niche_id: impl Into<String>, ts_ms: u64, source: impl Into<String>) -> Self {
        Self {
            niche_id: niche_id.into(),
            ts_ms,
            claim_latency_ms: None,
            competitors: None,
            reward_cents: None,
            acceptance: None,
            source: source.into(),
        }
    }
    pub fn claim_latency(mut self, ms: u64) -> Self {
        self.claim_latency_ms = Some(ms);
        self
    }
    pub fn competitors(mut self, n: f64) -> Self {
        self.competitors = Some(n);
        self
    }
    pub fn reward(mut self, cents: u64) -> Self {
        self.reward_cents = Some(cents);
        self
    }
    pub fn acceptance(mut self, rate: f64) -> Self {
        self.acceptance = Some(rate);
        self
    }
}

/// What the crowding meter says to do about a niche.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// Window is open and not yet closing fast. Spend effort here.
    Enter,
    /// Still paying, but the derivative is negative. Harvest, do not invest further.
    Hold,
    /// Projected margin falls below the floor inside the payback horizon. Leave now.
    Exit,
    /// Not enough evidence to say anything. Deliberately distinct from `Exit`:
    /// ignorance is not a bearish signal, it is a reason to keep measuring.
    Insufficient,
}

/// How much the estimator trusts its own output.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Confidence {
    /// Goodness of fit, 0..1.
    pub r2: f64,
    pub samples: usize,
    /// Observation window covered, in days. A tight fit over two hours says nothing
    /// about a niche's half-life in weeks.
    pub span_days: f64,
}

impl Confidence {
    /// Whether there is *enough data* to say anything. Sufficiency only — whether the
    /// trend is precise enough to act on is a separate question, answered by the
    /// interval around the estimate.
    ///
    /// Note what is deliberately absent: `r2`. A shallow trend measured cleanly over
    /// weeks scores a low r2 while being perfectly well determined, and gating on it
    /// discards exactly the slow-closing niches that are worth the most. It is kept as
    /// a reported diagnostic and given no vote.
    pub fn is_actionable(&self) -> bool {
        self.samples >= MIN_SAMPLES && self.span_days >= MIN_SPAN_DAYS
    }
}

pub const MIN_SAMPLES: usize = 6;
pub const MIN_SPAN_DAYS: f64 = 0.5;

/// A venue supplies observations about one or more niches, for free.
pub trait Source: Send + Sync {
    fn id(&self) -> &str;
    /// Niches this source currently knows about.
    fn niches(&self) -> anyhow::Result<Vec<Niche>>;
    /// Fresh observations since `since_ms`.
    fn observe(&self, since_ms: u64) -> anyhow::Result<Vec<Observation>>;
}
