//! The crowding meter.
//!
//! Four observable metrics say the same thing from different angles: claim latency,
//! reward, acceptance rate and competitor count. Each is fitted independently, turned
//! into a *pressure* — the rate at which the niche is losing value to us, per day — and
//! combined by how much each fit deserves to be trusted.
//!
//! The output is deliberately expressed as a runway in days rather than a score. "This
//! niche halves in eleven days" is a decision; "crowding: 0.72" is not.

use crate::fit::{bucket_median, fit, DecayFit, FitMethod, Point};
use hl_core::{Confidence, Observation, MS_PER_DAY};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    /// Time between an opportunity appearing and someone taking it.
    ClaimLatency,
    Reward,
    Acceptance,
    Competitors,
}

impl Metric {
    /// Whether a *rising* value is good for us. Competitors is the odd one out, and
    /// this is the only place that asymmetry is encoded.
    fn higher_is_better(self) -> bool {
        !matches!(self, Metric::Competitors)
    }

    pub fn label(self) -> &'static str {
        match self {
            Metric::ClaimLatency => "claim_latency",
            Metric::Reward => "reward",
            Metric::Acceptance => "acceptance",
            Metric::Competitors => "competitors",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricFit {
    pub metric: Metric,
    pub fit: DecayFit,
    /// Rate at which this metric says the niche is eroding, per day. Positive means
    /// closing; negative means the window is still widening.
    pub pressure_per_day: f64,
    /// Inverse-variance weight: how much this fit contributes to the aggregate.
    pub weight: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrowdingReport {
    pub niche_id: String,
    pub metrics: Vec<MetricFit>,
    /// Inverse-variance weighted erosion rate, per day.
    pub pressure_per_day: f64,
    /// Standard error of `pressure_per_day`.
    pub pressure_stderr: f64,
    /// Share of the niche's current value expected to be gone in a week. Reported
    /// because it is the number a human can sanity-check against their own eyes.
    pub weekly_decay: f64,
    pub confidence: Confidence,
    pub as_of_ms: u64,
}

impl CrowdingReport {
    /// Days until the niche's value falls to `floor_fraction` of today's.
    ///
    /// `None` means no measured erosion — an open-ended runway, which is a genuine
    /// answer and must never be silently rendered as zero.
    pub fn runway_days(&self, floor_fraction: f64) -> Option<f64> {
        if self.pressure_per_day <= 1e-9 || !(0.0..1.0).contains(&floor_fraction) {
            return None;
        }
        Some(-floor_fraction.ln() / self.pressure_per_day)
    }

    /// Days until the niche is worth half what it is worth now.
    pub fn half_life_days(&self) -> Option<f64> {
        self.runway_days(0.5)
    }

    /// Pessimistic erosion rate: the fast end of the 95% interval.
    ///
    /// Rotation is asymmetric — leaving a niche early costs a little foregone yield,
    /// leaving late costs the whole position — so every exit decision is taken against
    /// the worst plausible closure rate rather than the central estimate.
    pub fn pressure_pessimistic(&self) -> f64 {
        if !self.pressure_stderr.is_finite() {
            return f64::INFINITY;
        }
        self.pressure_per_day + 1.96 * self.pressure_stderr
    }

    /// Runway computed against [`Self::pressure_pessimistic`].
    ///
    /// Returns `None` when the erosion rate is unmeasured. Infinite uncertainty must
    /// not collapse into a zero-day runway: "we do not know" and "it is already gone"
    /// call for opposite actions.
    pub fn runway_days_conservative(&self, floor_fraction: f64) -> Option<f64> {
        if !self.pressure_stderr.is_finite() {
            return None;
        }
        let p = self.pressure_pessimistic();
        if p <= 1e-9 || !(0.0..1.0).contains(&floor_fraction) || floor_fraction <= 0.0 {
            return None;
        }
        Some(-floor_fraction.ln() / p)
    }

    /// Whether the estimate can pick a side.
    ///
    /// An absolute bound on the standard error is the wrong test — precision only
    /// matters relative to the decision being made. So this asks whether the 95%
    /// interval lands cleanly in one of two camps: *measurably closing* (the interval
    /// excludes zero) or *measurably stable* (the interval sits entirely below a rate
    /// too slow to plan around). An interval covering both is a reason to keep
    /// measuring, not to act.
    pub fn is_determined(&self) -> bool {
        if !self.pressure_stderr.is_finite() {
            return false;
        }
        let d = 1.96 * self.pressure_stderr;
        let lo = self.pressure_per_day - d;
        let hi = self.pressure_per_day + d;
        lo > 0.0 || hi < NEGLIGIBLE_PRESSURE
    }

    /// True when the niche is measurably stable rather than merely un-measured.
    pub fn is_stable(&self) -> bool {
        self.is_determined() && self.pressure_per_day + 1.96 * self.pressure_stderr < NEGLIGIBLE_PRESSURE
    }

    pub fn metric(&self, m: Metric) -> Option<&MetricFit> {
        self.metrics.iter().find(|f| f.metric == m)
    }
}

/// Erosion slower than this counts as no erosion: a half-life beyond two months is
/// longer than any window we would plan around anyway.
pub const NEGLIGIBLE_PRESSURE: f64 = std::f64::consts::LN_2 / 60.0;

/// How long a bucket is when collapsing many same-period samples. Six hours keeps
/// intraday structure while stopping one busy afternoon from setting a weekly trend.
const BUCKET_MS: u64 = 6 * 60 * 60 * 1000;

pub struct CrowdingMeter {
    pub method: FitMethod,
    pub bucket_ms: u64,
}

impl Default for CrowdingMeter {
    fn default() -> Self {
        Self {
            method: FitMethod::TheilSen,
            bucket_ms: BUCKET_MS,
        }
    }
}

impl CrowdingMeter {
    /// Build a report from raw observations. Metrics with too little data are simply
    /// absent from the result rather than guessed at.
    pub fn report(&self, niche_id: &str, obs: &[Observation], now_ms: u64) -> CrowdingReport {
        let mut metrics = Vec::new();
        for metric in [
            Metric::ClaimLatency,
            Metric::Reward,
            Metric::Acceptance,
            Metric::Competitors,
        ] {
            let raw: Vec<Point> = obs
                .iter()
                .filter(|o| o.niche_id == niche_id)
                .filter_map(|o| extract(metric, o))
                .collect();
            if raw.is_empty() {
                continue;
            }
            let pts = bucket_median(&raw, self.bucket_ms);
            let Ok(f) = fit(&pts, self.method) else { continue };

            // A falling metric has positive lambda. For metrics where higher is better,
            // falling is bad, so pressure is +lambda; for competitors it is inverted.
            let pressure = if metric.higher_is_better() {
                f.lambda_per_day
            } else {
                -f.lambda_per_day
            };
            // Inverse-variance weighting: precisely measured metrics dominate, whether
            // what they measure precisely is a collapse or a flat line.
            let weight = f.precision();
            metrics.push(MetricFit {
                metric,
                fit: f,
                pressure_per_day: pressure,
                weight,
            });
        }

        let total_w: f64 = metrics.iter().map(|m| m.weight).sum();
        let (pressure, pressure_stderr, r2) = if total_w > 0.0 {
            (
                metrics.iter().map(|m| m.pressure_per_day * m.weight).sum::<f64>() / total_w,
                // Standard error of an inverse-variance weighted mean.
                (1.0 / total_w).sqrt(),
                metrics.iter().map(|m| m.fit.r2 * m.weight).sum::<f64>() / total_w,
            )
        } else {
            (0.0, f64::INFINITY, 0.0)
        };
        let confidence = Confidence {
            r2,
            samples: metrics.iter().map(|m| m.fit.n).sum(),
            span_days: metrics.iter().map(|m| m.fit.span_days).fold(0.0, f64::max),
        };

        CrowdingReport {
            niche_id: niche_id.to_string(),
            metrics,
            pressure_per_day: pressure,
            pressure_stderr,
            weekly_decay: 1.0 - (-pressure.max(0.0) * 7.0).exp(),
            confidence,
            as_of_ms: now_ms,
        }
    }
}

fn extract(metric: Metric, o: &Observation) -> Option<Point> {
    let v = match metric {
        Metric::ClaimLatency => o.claim_latency_ms? as f64,
        Metric::Reward => o.reward_cents? as f64,
        Metric::Acceptance => o.acceptance?,
        Metric::Competitors => o.competitors?,
    };
    (v > 0.0).then(|| Point::new(o.ts_ms, v))
}

/// Convenience for callers that think in days.
pub fn days_ms(days: f64) -> u64 {
    (days * MS_PER_DAY) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic niche whose reward halves every `hl` days and whose claim
    /// latency halves every `latency_hl` days.
    fn synth(hl: Option<f64>, latency_hl: Option<f64>, days: f64, per_day: usize) -> Vec<Observation> {
        let n = (days * per_day as f64) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / per_day as f64;
                let mut o = Observation::new("n", days_ms(t), "sim");
                if let Some(hl) = hl {
                    o = o.reward(
                        (10_000.0 * 0.5_f64.powf(t / hl)).max(1.0) as u64,
                    );
                }
                if let Some(lhl) = latency_hl {
                    o = o.claim_latency(
                        (3_600_000.0 * 0.5_f64.powf(t / lhl)).max(1.0) as u64,
                    );
                }
                o
            })
            .collect()
    }

    #[test]
    fn measures_a_collapsing_niche() {
        let obs = synth(Some(7.0), None, 21.0, 4);
        let r = CrowdingMeter::default().report("n", &obs, days_ms(21.0));
        let hl = r.half_life_days().expect("a collapsing niche must have a half-life");
        assert!((hl - 7.0).abs() < 0.7, "half-life measured as {hl}");
        assert!(r.weekly_decay > 0.45 && r.weekly_decay < 0.55);
        assert!(r.confidence.is_actionable());
    }

    #[test]
    fn collapsing_claim_latency_counts_as_crowding() {
        // Reward is untouched; only latency collapses. Crowding must still register.
        let obs = synth(None, Some(4.0), 12.0, 4);
        let r = CrowdingMeter::default().report("n", &obs, days_ms(12.0));
        assert!(r.pressure_per_day > 0.0);
        let hl = r.half_life_days().unwrap();
        assert!((hl - 4.0).abs() < 0.5, "half-life {hl}");
    }

    #[test]
    fn rising_competitor_count_is_crowding_not_growth() {
        let obs: Vec<Observation> = (0..40)
            .map(|i| {
                let t = i as f64 / 4.0;
                Observation::new("n", days_ms(t), "sim")
                    .competitors(2.0 * 2.0_f64.powf(t / 5.0) + 1.0)
            })
            .collect();
        let r = CrowdingMeter::default().report("n", &obs, days_ms(10.0));
        assert!(
            r.pressure_per_day > 0.0,
            "more competitors must read as more crowded, not less"
        );
    }

    #[test]
    fn a_widening_window_has_no_runway() {
        let obs: Vec<Observation> = (0..40)
            .map(|i| {
                let t = i as f64 / 4.0;
                Observation::new("n", days_ms(t), "sim").reward((1000.0 * 1.05_f64.powf(t)) as u64)
            })
            .collect();
        let r = CrowdingMeter::default().report("n", &obs, days_ms(10.0));
        assert!(r.pressure_per_day < 0.0);
        assert_eq!(r.runway_days(0.5), None);
        assert_eq!(r.weekly_decay, 0.0);
    }

    #[test]
    fn a_precisely_flat_metric_tempers_a_collapsing_one() {
        // Reward collapsing fast, acceptance flat and tightly measured. Under r²
        // weighting the flat metric scored ~0 and was silently discarded; under
        // inverse-variance weighting its precision earns it a vote.
        let mut obs = synth(Some(3.0), None, 12.0, 4);
        for (i, o) in obs.iter_mut().enumerate() {
            o.acceptance = Some(0.2 + (i % 2) as f64 * 0.001);
        }
        let r = CrowdingMeter::default().report("n", &obs, days_ms(12.0));
        let reward_p = r.metric(Metric::Reward).unwrap().pressure_per_day;
        assert!(r.pressure_per_day > 0.0, "the collapse must still show through");
        assert!(
            r.pressure_per_day < reward_p,
            "flat acceptance ({:.4}/day) must temper reward ({reward_p:.4}/day), got {:.4}",
            r.metric(Metric::Acceptance).unwrap().pressure_per_day,
            r.pressure_per_day
        );
    }

    #[test]
    fn conservative_runway_is_never_longer_than_the_central_one() {
        let obs = synth(Some(7.0), None, 21.0, 4);
        let r = CrowdingMeter::default().report("n", &obs, days_ms(21.0));
        let central = r.runway_days(0.5).unwrap();
        let conservative = r.runway_days_conservative(0.5).unwrap();
        assert!(
            conservative <= central,
            "exits must be taken against the worst plausible rate: {conservative} > {central}"
        );
    }

    #[test]
    fn wide_intervals_are_not_determined() {
        let mut rng = hl_core::Rng::new(3);
        let obs: Vec<Observation> = (0..40)
            .map(|i| {
                Observation::new("n", days_ms(i as f64 / 4.0), "sim")
                    .reward((100.0 * (0.05 + 4.0 * rng.unit())) as u64 + 1)
            })
            .collect();
        let r = CrowdingMeter::default().report("n", &obs, days_ms(10.0));
        assert!(!r.is_determined(), "pure noise must not yield a determined trend");
    }

    #[test]
    fn no_data_means_unknown_runway_not_zero_runway() {
        let r = CrowdingMeter::default().report("n", &[], 0);
        assert!(r.metrics.is_empty());
        assert_eq!(
            r.runway_days_conservative(0.5),
            None,
            "an unmeasured niche is not an expired one"
        );
        assert!(!r.is_determined());
    }

    #[test]
    fn sparse_data_yields_no_confidence_rather_than_a_guess() {
        let obs = vec![
            Observation::new("n", 0, "sim").reward(100),
            Observation::new("n", days_ms(0.1), "sim").reward(90),
        ];
        let r = CrowdingMeter::default().report("n", &obs, days_ms(0.1));
        assert!(!r.confidence.is_actionable());
    }

    #[test]
    fn observations_for_other_niches_are_ignored() {
        let mut obs = synth(Some(5.0), None, 12.0, 4);
        obs.extend((0..40).map(|i| {
            Observation::new("other", days_ms(i as f64 / 4.0), "sim").reward(1)
        }));
        let r = CrowdingMeter::default().report("n", &obs, days_ms(12.0));
        assert!((r.half_life_days().unwrap() - 5.0).abs() < 0.6);
    }
}
