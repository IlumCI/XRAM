//! Rotation policy: turning a runway estimate into enter / hold / exit.
//!
//! The rule the whole system exists to apply: **act on the derivative, not the level.**
//! A niche paying well today with a nine-day half-life and a fourteen-day payback is
//! already a loss; the ledger just hasn't noticed yet.

use crate::crowding::CrowdingReport;
use hl_core::{EntryCost, Signal};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PolicyConfig {
    /// The fraction of today's value at which a niche counts as dead.
    pub floor_fraction: f64,
    /// How long our setup effort takes to pay for itself in this class of niche.
    pub payback_days: f64,
    /// Runway must exceed `payback_days * enter_multiple` before committing effort.
    /// Above 1 because a niche that only just pays back is not worth the switching cost.
    pub enter_multiple: f64,
    /// All the money there is.
    pub budget_cents: u64,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            floor_fraction: 0.5,
            payback_days: 3.0,
            enter_multiple: 3.0,
            budget_cents: 500,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub niche_id: String,
    pub signal: Signal,
    pub reason: String,
    /// Days until the niche falls to the configured floor. `None` means no measured
    /// erosion.
    pub runway_days: Option<f64>,
    /// True when we simply cannot afford to be here, whatever the trend says.
    pub blocked_by_cost: bool,
}

/// Decide what to do about one niche.
pub fn decide(report: &CrowdingReport, cost: &EntryCost, cfg: &PolicyConfig) -> Decision {
    let runway = report.runway_days_conservative(cfg.floor_fraction);
    let mk = |signal: Signal, reason: String, blocked: bool| Decision {
        niche_id: report.niche_id.clone(),
        signal,
        reason,
        runway_days: runway,
        blocked_by_cost: blocked,
    };

    // Affordability is checked first and independently of the trend: with no capital,
    // a perfect window we cannot pay to enter is still not ours.
    if cost.money_cents > cfg.budget_cents {
        return mk(
            Signal::Exit,
            format!(
                "entry costs {}c, budget is {}c",
                cost.money_cents, cfg.budget_cents
            ),
            true,
        );
    }

    if !report.confidence.is_actionable() {
        return mk(
            Signal::Insufficient,
            format!(
                "not enough data: {} samples over {:.2}d (need {} over {:.1}d)",
                report.confidence.samples,
                report.confidence.span_days,
                hl_core::MIN_SAMPLES,
                hl_core::MIN_SPAN_DAYS
            ),
            false,
        );
    }
    if !report.is_determined() {
        return mk(
            Signal::Insufficient,
            format!(
                "erosion undetermined: {:.4} +/- {:.4}/day spans both stable and closing",
                report.pressure_per_day,
                1.96 * report.pressure_stderr
            ),
            false,
        );
    }

    // Exits are judged against the fast end of the interval, entries against it too:
    // being wrong about a runway is much more expensive in one direction.
    let runway = report.runway_days_conservative(cfg.floor_fraction);
    match runway {
        None => mk(
            Signal::Enter,
            format!(
                "no measured erosion (pressure {:.4}/day)",
                report.pressure_per_day
            ),
            false,
        ),
        Some(r) if r < cfg.payback_days => mk(
            Signal::Exit,
            format!(
                "runway {r:.1}d is shorter than {:.1}d payback",
                cfg.payback_days
            ),
            false,
        ),
        Some(r) if r < cfg.payback_days * cfg.enter_multiple => mk(
            Signal::Hold,
            format!(
                "runway {r:.1}d covers payback but not the {:.1}x margin; harvest, do not invest",
                cfg.enter_multiple
            ),
            false,
        ),
        Some(r) => mk(
            Signal::Enter,
            format!("runway {r:.1}d against {:.1}d payback", cfg.payback_days),
            false,
        ),
    }
}

/// Rank niches by how much room is left, best first.
///
/// Open-ended runways sort above every finite one; among finite runways, longer wins.
pub fn rank(decisions: &[Decision]) -> Vec<&Decision> {
    let mut v: Vec<&Decision> = decisions
        .iter()
        .filter(|d| matches!(d.signal, Signal::Enter | Signal::Hold))
        .collect();
    v.sort_by(|a, b| {
        b.runway_days
            .unwrap_or(f64::INFINITY)
            .partial_cmp(&a.runway_days.unwrap_or(f64::INFINITY))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crowding::{days_ms, CrowdingMeter};
    use hl_core::Observation;

    fn report_for(half_life: Option<f64>, days: f64) -> CrowdingReport {
        let per_day = 4;
        let n = (days * per_day as f64) as usize;
        let obs: Vec<Observation> = (0..n)
            .map(|i| {
                let t = i as f64 / per_day as f64;
                let v = match half_life {
                    Some(hl) => 10_000.0 * 0.5_f64.powf(t / hl),
                    None => 10_000.0,
                };
                Observation::new("n", days_ms(t), "sim").reward(v.max(1.0) as u64)
            })
            .collect();
        CrowdingMeter::default().report("n", &obs, days_ms(days))
    }

    #[test]
    fn a_fast_collapsing_niche_is_exited() {
        let d = decide(&report_for(Some(1.0), 6.0), &EntryCost::default(), &PolicyConfig::default());
        assert_eq!(d.signal, Signal::Exit);
        assert!(!d.blocked_by_cost);
    }

    #[test]
    fn a_stable_niche_is_entered_with_a_long_runway() {
        let d = decide(&report_for(None, 6.0), &EntryCost::default(), &PolicyConfig::default());
        assert_eq!(d.signal, Signal::Enter, "reason: {}", d.reason);
        // Not `None`: six days of flat data buys a long runway, not an infinite one,
        // and the conservative estimator is right to say so.
        let r = d.runway_days.expect("a measured-stable niche still reports its bound");
        assert!(r > 100.0, "runway {r:.0}d should be long");
    }

    #[test]
    fn a_slowly_closing_niche_is_held_not_abandoned() {
        // Payback 3d, enter needs >9d runway. A 6-day half-life lands in between.
        let d = decide(&report_for(Some(6.0), 18.0), &EntryCost::default(), &PolicyConfig::default());
        assert_eq!(d.signal, Signal::Hold, "reason: {}", d.reason);
    }

    #[test]
    fn thin_evidence_is_insufficient_not_bearish() {
        let obs = vec![
            Observation::new("n", 0, "sim").reward(100),
            Observation::new("n", days_ms(0.05), "sim").reward(80),
            Observation::new("n", days_ms(0.1), "sim").reward(60),
        ];
        let r = CrowdingMeter::default().report("n", &obs, days_ms(0.1));
        let d = decide(&r, &EntryCost::default(), &PolicyConfig::default());
        assert_eq!(
            d.signal,
            Signal::Insufficient,
            "ignorance must not be reported as a reason to leave"
        );
    }

    #[test]
    fn an_unaffordable_niche_is_blocked_whatever_the_trend() {
        let cost = EntryCost {
            money_cents: 50_000,
            ..Default::default()
        };
        let d = decide(&report_for(None, 6.0), &cost, &PolicyConfig::default());
        assert_eq!(d.signal, Signal::Exit);
        assert!(d.blocked_by_cost);
    }

    #[test]
    fn ranking_prefers_open_ended_then_longer_runways() {
        let mk = |id: &str, runway: Option<f64>, signal: Signal| Decision {
            niche_id: id.into(),
            signal,
            reason: String::new(),
            runway_days: runway,
            blocked_by_cost: false,
        };
        let ds = vec![
            mk("short", Some(4.0), Signal::Hold),
            mk("open", None, Signal::Enter),
            mk("long", Some(30.0), Signal::Enter),
            mk("gone", Some(0.5), Signal::Exit),
        ];
        let ranked = rank(&ds);
        assert_eq!(
            ranked.iter().map(|d| d.niche_id.as_str()).collect::<Vec<_>>(),
            vec!["open", "long", "short"],
            "exited niches must not be ranked at all"
        );
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    use crate::crowding::{days_ms, CrowdingMeter};
    use hl_core::Observation;

    /// A stable niche has almost no variance to explain, so its r² sits near zero
    /// however cleanly it is measured. Gating on r² therefore reported healthy,
    /// well-measured niches as "not enough data" on 168 samples — the exact opposite of
    /// the truth. Sufficiency and precision are now separate questions.
    #[test]
    fn a_flat_niche_is_actionable_despite_near_zero_r2() {
        let mut rng = hl_core::Rng::new(11);
        let obs: Vec<Observation> = (0..84)
            .map(|i| {
                let t = i as f64 / 4.0;
                let v = 5_000.0 * (1.0 + 0.05 * (rng.unit() - 0.5));
                Observation::new("n", days_ms(t), "sim").reward(v as u64)
            })
            .collect();
        let r = CrowdingMeter::default().report("n", &obs, days_ms(21.0));
        assert!(
            r.confidence.r2 < 0.3,
            "a flat series has nothing to explain: r2 {:.3}",
            r.confidence.r2
        );
        assert!(r.confidence.is_actionable(), "168 samples over 21 days is plenty");
        assert!(r.is_stable(), "and it is measurably stable, not merely un-measured");
        let d = decide(&r, &EntryCost::default(), &PolicyConfig::default());
        assert_eq!(d.signal, Signal::Enter, "reason: {}", d.reason);
    }

    /// The complementary case: a slow decline still yields a long runway rather than
    /// being rounded to "closing".
    #[test]
    fn a_shallow_trend_yields_a_long_runway() {
        let hl = 45.0;
        let mut rng = hl_core::Rng::new(4);
        let obs: Vec<Observation> = (0..84)
            .map(|i| {
                let t = i as f64 / 4.0;
                let v = 10_000.0 * 0.5_f64.powf(t / hl) * (1.0 + 0.3 * (rng.unit() - 0.5));
                Observation::new("n", days_ms(t), "sim").reward(v as u64)
            })
            .collect();
        let r = CrowdingMeter::default().report("n", &obs, days_ms(21.0));
        let d = decide(&r, &EntryCost::default(), &PolicyConfig::default());
        assert_eq!(d.signal, Signal::Enter, "reason: {}", d.reason);
        let runway = d.runway_days.unwrap();
        assert!(runway > 9.0, "a 45-day half-life is a long runway, got {runway:.1}d");
    }
}
