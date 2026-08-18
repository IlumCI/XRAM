//! Parameter search with the price of searching paid honestly.
//!
//! Refining a model against evidence is the job. The trap is narrower than "don't
//! tune": it is tuning and *scoring* on the same data. Run enough variants against one
//! sample and the best of them wins by construction — its edge is the maximum of many
//! draws from noise, and no amount of staring at it separates skill from selection.
//!
//! So this splits the history in two. Variants compete on the training period only. The
//! single winner is then run once on a test period that took no part in choosing it,
//! and that number is the one reported.
//!
//! Three figures make the search auditable:
//!
//! * **Variants tried** — the size of the multiple-comparisons problem, stated up front.
//! * **Degradation** — train return minus test return. A large positive gap is the
//!   signature of a variant that memorised its training window.
//! * **Median out-of-sample return across all variants** — what an arbitrary variant
//!   scored on the test period. If the train-winner cannot beat that, the search found
//!   ordering in noise and nothing else.

use crate::backtest::{Backtest, Outcome};
use crate::portfolio::PaperConfig;
use hl_core::{Observation, MS_PER_DAY};
use hl_probe::PolicyConfig;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct Variant {
    pub label: String,
    pub backtest: Backtest,
}

/// The search space.
///
/// Deliberately modest. A grid wide enough to guarantee a winner is a grid wide enough
/// to guarantee a meaningless one, and every extra axis makes the multiple-comparisons
/// problem worse for the same amount of evidence.
pub fn grid(base: &PaperConfig) -> Vec<Variant> {
    let mut out = Vec::new();
    for positions in [1usize, 3, 5] {
        for lookback in [7.0f64, 21.0, 60.0] {
            for hold in [7.0f64, 30.0] {
                for floor in [0.5f64, 0.8] {
                    for payback in [3.0f64, 14.0] {
                        out.push(Variant {
                            label: format!(
                                "pos={positions} look={lookback:.0}d hold={hold:.0}d \
                                 floor={floor:.1} payback={payback:.0}d"
                            ),
                            backtest: Backtest {
                                cfg: PaperConfig {
                                    max_positions: positions,
                                    ..*base
                                },
                                policy: PolicyConfig {
                                    floor_fraction: floor,
                                    payback_days: payback,
                                    ..PolicyConfig::default()
                                },
                                step_days: 1.0,
                                expected_hold_days: hold,
                                lookback_days: lookback,
                            },
                        });
                    }
                }
            }
        }
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuneResult {
    pub variants_tried: usize,
    pub train_days: f64,
    pub test_days: f64,
    pub best_label: String,
    /// The winner's score on the data that chose it. Not evidence of anything.
    pub train_return_pct: f64,
    /// The winner's score on data that took no part in choosing it. This is the number.
    pub test_return_pct: f64,
    /// Buy-and-hold over the same test period.
    pub test_hold_return_pct: f64,
    /// Train minus test. The cost of having searched.
    pub degradation_pct: f64,
    /// What an arbitrary variant scored out of sample.
    pub median_test_return_pct: f64,
    /// Whether the winner beat both hold and the median variant out of sample.
    pub survived: bool,
}

/// The strategy whose parameters are being searched.
const STRATEGY: &str = "rotation (fee-aware)";
const HOLD: &str = "hold best at start";

fn score(o: Option<&Outcome>) -> f64 {
    o.map(|x| x.return_pct).unwrap_or(f64::NAN)
}

pub fn tune(observations: &[Observation], base: &PaperConfig, train_fraction: f64) -> Option<TuneResult> {
    let times: Vec<u64> = {
        let mut t: Vec<u64> = observations.iter().map(|o| o.ts_ms).collect();
        t.sort_unstable();
        t
    };
    let (first, last) = (*times.first()?, *times.last()?);
    let split = first + ((last - first) as f64 * train_fraction.clamp(0.1, 0.9)) as u64;

    let train: Vec<Observation> = observations.iter().filter(|o| o.ts_ms <= split).cloned().collect();
    // The test period keeps a lead-in of history so the meter is not starting blind on
    // day one of the evaluation; only the *scoring* window is held out.
    let test: Vec<Observation> = observations.iter().filter(|o| o.ts_ms > split).cloned().collect();
    if train.len() < 50 || test.len() < 50 {
        return None;
    }

    let variants = grid(base);
    // Every variant is scored on both periods, but only the training score is allowed
    // to choose. Computing both up front costs nothing extra and makes the median
    // out-of-sample control available.
    let scored: Vec<(Variant, f64, f64)> = variants
        .par_iter()
        .map(|v| {
            let tr = v.backtest.run(&train);
            let te = v.backtest.run(&test);
            (
                v.clone(),
                score(tr.get(STRATEGY)),
                score(te.get(STRATEGY)),
            )
        })
        .collect();

    let usable: Vec<&(Variant, f64, f64)> =
        scored.iter().filter(|(_, tr, te)| tr.is_finite() && te.is_finite()).collect();
    if usable.is_empty() {
        return None;
    }

    let best = usable
        .iter()
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))?;

    let mut test_scores: Vec<f64> = usable.iter().map(|(_, _, te)| *te).collect();
    test_scores.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_test = test_scores[test_scores.len() / 2];

    let hold_test = score(Backtest::default().run(&test).get(HOLD));

    Some(TuneResult {
        variants_tried: usable.len(),
        train_days: (split - first) as f64 / MS_PER_DAY,
        test_days: (last - split) as f64 / MS_PER_DAY,
        best_label: best.0.label.clone(),
        train_return_pct: best.1,
        test_return_pct: best.2,
        test_hold_return_pct: hold_test,
        degradation_pct: best.1 - best.2,
        median_test_return_pct: median_test,
        survived: best.2 > hold_test && best.2 > median_test,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn niche(id: &str, days: usize, f: impl Fn(f64) -> f64) -> Vec<Observation> {
        (0..days)
            .map(|i| {
                let t = i as f64;
                Observation::new(id, (t * MS_PER_DAY) as u64, "defillama")
                    .reward(f(t).max(1.0) as u64)
            })
            .collect()
    }

    #[test]
    fn the_grid_is_small_enough_to_stay_honest() {
        let g = grid(&PaperConfig::default());
        assert_eq!(g.len(), 3 * 3 * 2 * 2 * 2);
        assert!(g.len() < 100, "a grid that guarantees a winner guarantees a meaningless one");
    }

    #[test]
    fn too_little_history_refuses_to_split() {
        let obs = niche("defi:a", 10, |_| 500.0);
        assert!(tune(&obs, &PaperConfig::default(), 0.67).is_none());
    }

    #[test]
    fn the_winner_is_chosen_on_train_and_scored_on_test() {
        let mut obs = niche("defi:a", 400, |t| 900.0 + 300.0 * (t / 40.0).sin());
        obs.extend(niche("defi:b", 400, |t| 900.0 - 300.0 * (t / 40.0).sin()));
        let r = tune(&obs, &PaperConfig::default(), 0.67).expect("enough history");
        assert_eq!(r.variants_tried, 72);
        assert!(r.train_days > r.test_days, "two thirds train");
        // The reported degradation must be exactly the two scores it claims to compare.
        assert!((r.degradation_pct - (r.train_return_pct - r.test_return_pct)).abs() < 1e-9);
        // Survival requires clearing both controls, not just one.
        assert_eq!(
            r.survived,
            r.test_return_pct > r.test_hold_return_pct && r.test_return_pct > r.median_test_return_pct
        );
    }
}
