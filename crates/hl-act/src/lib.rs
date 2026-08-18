//! Actuation.
//!
//! Everything below this crate finds where money might be. This is the layer that would
//! collect it, and it is deliberately the most conservative code in the repository:
//! every path is refused by default, and the interesting work is in being honest about
//! what an attempt is really worth.
//!
//! Two things are worth stating plainly, because they are what the layer actually
//! discovered rather than what it was hoped to do.
//!
//! **Expected value is zero until measured.** Prize divided by entrants is not an
//! expectation; prizes go to the top few, not the field. See [`appraise`].
//!
//! **Identity gates income.** Every real payout route — competitions, bounties,
//! marketplaces — requires an account that has accepted terms in a person's name. That
//! is the same non-replicable input the whole project started from, reappearing at the
//! last step. No amount of automation removes it, and nothing here tries to.

pub mod appraise;
pub mod gate;
pub mod kaggle_act;

pub use appraise::{Appraisal, LeaderboardShape};
pub use gate::{check, ActMode, Authorization, Consent, Refusal};
pub use kaggle_act::KaggleActuator;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    /// Authorised and understood, but the work itself still has to be done.
    Prepared,
    Submitted,
    Placed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attempt {
    pub niche_id: String,
    pub actuator: String,
    pub mode: ActMode,
    /// What would happen, in plain terms, so a dry run is auditable.
    pub plan: String,
    pub outcome: AttemptOutcome,
    pub spent_seconds: u64,
    pub earned_cents: u64,
}

pub trait Actuator: Send + Sync {
    fn id(&self) -> &str;
    fn authorization(&self) -> &Authorization;
    /// Attempt a niche. Implementations must call [`gate::check`] first and must honour
    /// [`ActMode::DryRun`].
    fn attempt(
        &self,
        appraisal: &Appraisal,
        consent: &Consent,
        mode: ActMode,
    ) -> Result<Attempt, Refusal>;
}

/// Place rate measured from our own history: how often an attempt reached the money.
///
/// Returns `None` rather than zero when nothing has been attempted, because "never
/// tried" and "tried and never placed" justify very different decisions.
pub fn measured_place_rate(attempts: u64, placements: u64) -> Option<f64> {
    (attempts > 0).then(|| placements as f64 / attempts as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_having_tried_is_not_a_zero_rate() {
        assert_eq!(measured_place_rate(0, 0), None);
        assert_eq!(measured_place_rate(10, 0), Some(0.0));
        assert_eq!(measured_place_rate(4, 1), Some(0.25));
    }
}
