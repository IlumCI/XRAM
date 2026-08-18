//! Honest appraisal of what a niche would actually pay.
//!
//! The naive figure — prize divided by entrants — is not an expected value and this
//! module exists largely to refuse to report it. Prizes go to the top few finishers,
//! not to the field, so for a median entrant the true expectation is approximately
//! zero. A competition advertising $450,000 across 142 teams is not "$3,169 a team"; it
//! is $150,000 each to three teams and nothing to the other 139.
//!
//! What can be stated honestly is: the prize, how many places pay, what score the money
//! currently starts at, how tightly the top of the field is packed, and — crucially —
//! whether we have any evidence at all that we could reach it. Absent that evidence the
//! expectation is zero, and it stays zero until the ledger says otherwise.

use serde::{Deserialize, Serialize};

/// Whether a niche can be acted on without a person in the loop, and if not, why.
///
/// This is the classification that killed automated competition entry as a strategy.
/// Surveying twenty live Kaggle competitions: every one carrying a large cash prize was
/// notebook-only, which the API cannot enter at all. The single genuinely automatable
/// cash competition was also the most crowded on the board.
///
/// That is not an accident of one venue. Notebook-only submission is a venue defending
/// itself against automated entry, which is the project's own thesis reappearing one
/// layer down: what can be automated gets saturated, so whatever still pays has built a
/// wall against automation. Keeping this as a live classification means the rare
/// exception — real money, file submission, thin field — gets flagged the day it
/// appears instead of being rediscovered by hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Automatability {
    /// Entries are files; the whole loop can run unattended once joined.
    FileSubmission,
    /// Entries must be notebooks run on the venue's infrastructure. No API path exists.
    NotebookOnly,
    /// Judged by people, on prose. Automation is beside the point.
    HumanJudged,
    /// Nothing to win.
    NoCashPrize,
}

impl Automatability {
    pub fn is_automatable(&self) -> bool {
        matches!(self, Automatability::FileSubmission)
    }
    pub fn label(&self) -> &'static str {
        match self {
            Automatability::FileSubmission => "file",
            Automatability::NotebookOnly => "notebook-only",
            Automatability::HumanJudged => "human-judged",
            Automatability::NoCashPrize => "no cash",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Appraisal {
    pub niche_id: String,
    pub label: String,
    /// Total advertised prize, in cents. `None` for non-cash.
    pub prize_cents: Option<u64>,
    /// How many are already competing.
    pub competitors: u64,
    /// Places that actually pay, where the venue states it.
    pub paying_places: Option<u32>,
    /// The score currently sitting at the bottom of the money, where a leaderboard is
    /// visible. This is the bar, in the venue's own units.
    pub score_to_place: Option<f64>,
    /// Gap between the best score and the worst visible one. A tight spread means the
    /// field is saturated and marginal effort buys nothing.
    pub top_spread: Option<f64>,
    /// Days until the window shuts.
    pub days_left: Option<f64>,
    /// Measured probability of reaching the money, from our own history at this venue.
    /// `None` means we have never placed and have no basis for a number.
    pub measured_place_rate: Option<f64>,
    /// Expected return in cents. Zero unless there is evidence behind it.
    pub expected_cents: f64,
    /// Whether this can be acted on without a person in the loop.
    pub automatable: Automatability,
    pub basis: String,
}

impl Appraisal {
    /// The number the naive reading would produce. Reported only so the report can show
    /// how far it is from the honest figure, never as a decision input.
    pub fn naive_per_competitor_cents(&self) -> Option<f64> {
        self.prize_cents
            .map(|p| p as f64 / self.competitors.max(1) as f64)
    }

    /// Expected value, computed from evidence rather than arithmetic convenience.
    ///
    /// With no measured place rate this is zero — not "unknown but probably something".
    /// Every entrant who has ever lost money to a competition believed otherwise.
    pub fn compute_expected_cents(&mut self) {
        self.expected_cents = match (self.prize_cents, self.measured_place_rate) {
            (Some(prize), Some(rate)) => {
                // Split the pot evenly across paying places as a first approximation:
                // top-heavy distributions make this optimistic, which is the right
                // direction to be wrong in only if we say so, and we do.
                let places = self.paying_places.unwrap_or(3).max(1) as f64;
                (prize as f64 / places) * rate
            }
            _ => 0.0,
        };
    }

    /// Whether this is worth a real attempt: money, time, evidence, and a path in.
    pub fn is_worth_attempting(&self) -> bool {
        self.automatable.is_automatable()
            && self.prize_cents.is_some_and(|p| p > 0)
            && self.days_left.map_or(true, |d| d > 1.0)
            && self.expected_cents > 0.0
    }
}

/// Summary statistics of a visible leaderboard.
#[derive(Debug, Clone, Copy, Default)]
pub struct LeaderboardShape {
    pub visible_entries: usize,
    pub best: Option<f64>,
    pub worst_visible: Option<f64>,
}

impl LeaderboardShape {
    pub fn from_scores(mut scores: Vec<f64>) -> Self {
        scores.retain(|s| s.is_finite());
        if scores.is_empty() {
            return Self::default();
        }
        // Higher-is-better is the common case, but not universal; ordering by the
        // venue's own ranking is what matters, and the API returns rank order.
        Self {
            visible_entries: scores.len(),
            best: scores.first().copied(),
            worst_visible: scores.last().copied(),
        }
    }

    /// How tightly the visible top of the field is packed.
    pub fn spread(&self) -> Option<f64> {
        match (self.best, self.worst_visible) {
            (Some(b), Some(w)) => Some((b - w).abs()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn appraisal(prize: Option<u64>, competitors: u64, rate: Option<f64>) -> Appraisal {
        let mut a = Appraisal {
            niche_id: "n".into(),
            label: "n".into(),
            prize_cents: prize,
            competitors,
            paying_places: Some(3),
            score_to_place: None,
            top_spread: None,
            days_left: Some(30.0),
            measured_place_rate: rate,
            expected_cents: 0.0,
            automatable: Automatability::FileSubmission,
            basis: String::new(),
        };
        a.compute_expected_cents();
        a
    }

    #[test]
    fn no_track_record_means_zero_not_optimism() {
        // The correction this module exists for: $450k across 142 teams is not $3,169
        // a team, it is nothing at all until we have placed in something.
        let a = appraisal(Some(45_000_000), 142, None);
        assert_eq!(a.expected_cents, 0.0);
        assert!(!a.is_worth_attempting());
        // The naive figure is still computable, for contrast only.
        assert!((a.naive_per_competitor_cents().unwrap() - 316_901.4).abs() < 1.0);
    }

    #[test]
    fn a_measured_place_rate_produces_a_real_number() {
        let a = appraisal(Some(45_000_000), 142, Some(0.02));
        // One third of the pot, twice in a hundred attempts.
        assert!((a.expected_cents - (45_000_000.0 / 3.0) * 0.02).abs() < 1.0);
        assert!(a.is_worth_attempting());
    }

    #[test]
    fn non_cash_prizes_are_never_worth_attempting_for_money() {
        let a = appraisal(None, 10_000, Some(0.5));
        assert_eq!(a.expected_cents, 0.0);
        assert!(!a.is_worth_attempting());
    }

    #[test]
    fn a_closing_window_is_not_worth_attempting() {
        let mut a = appraisal(Some(1_000_000), 10, Some(0.1));
        assert!(a.is_worth_attempting());
        a.days_left = Some(0.5);
        assert!(!a.is_worth_attempting(), "no time to do the work");
    }

    #[test]
    fn a_notebook_only_competition_is_never_worth_attempting() {
        // The finding that closed this route: the money sits behind notebook-only
        // entry, which has no API path however good a model we had.
        let mut a = appraisal(Some(85_000_000), 2382, Some(0.5));
        assert!(a.is_worth_attempting());
        a.automatable = Automatability::NotebookOnly;
        assert!(!a.is_worth_attempting());
        assert!(!a.automatable.is_automatable());
    }

    #[test]
    fn human_judged_tracks_are_excluded_too() {
        let mut a = appraisal(Some(45_000_000), 142, Some(0.5));
        a.automatable = Automatability::HumanJudged;
        assert!(!a.is_worth_attempting());
    }

    #[test]
    fn leaderboard_shape_describes_how_tight_the_top_is() {
        let s = LeaderboardShape::from_scores(vec![0.951, 0.948, 0.94, 0.937]);
        assert_eq!(s.visible_entries, 4);
        assert!((s.spread().unwrap() - 0.014).abs() < 1e-9);

        let empty = LeaderboardShape::from_scores(vec![]);
        assert_eq!(empty.visible_entries, 0);
        assert_eq!(empty.spread(), None);
    }

    #[test]
    fn non_finite_scores_are_discarded() {
        let s = LeaderboardShape::from_scores(vec![1.0, f64::NAN, 0.5]);
        assert_eq!(s.visible_entries, 2);
    }
}
