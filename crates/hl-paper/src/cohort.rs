//! Does crowding actually depress reward in this market?
//!
//! Every strategy this project has built rests on one assumption: capital arrives, and
//! the rate falls. That is an empirical claim about a market, and it is cheaper to test
//! directly than to discover through a losing backtest.
//!
//! The test aligns each niche by its own age rather than by calendar date, normalises
//! to its own first reading so niches of different scale are comparable, and asks two
//! questions:
//!
//! 1. Does the median reward fall as niches age?
//! 2. **Within** a niche, does reward fall when competitor density rises?
//!
//! The second is the one that matters. A market-wide drift can come from anything; a
//! within-niche relationship between crowd and reward is the mechanism itself.

use hl_core::{Observation, MS_PER_DAY};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy)]
pub struct CohortStudy {
    /// How far into a niche's life to follow it.
    pub max_days: usize,
    /// Minimum readings before a niche is included at all.
    pub min_readings: usize,
}

impl Default for CohortStudy {
    fn default() -> Self {
        Self {
            max_days: 90,
            min_readings: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CohortResult {
    pub niches: usize,
    /// `(day, median reward relative to that niche's first reading)`.
    pub reward_by_age: Vec<(usize, f64)>,
    /// `(day, median competitor density relative to the first reading)`.
    pub crowd_by_age: Vec<(usize, f64)>,
    /// Median within-niche correlation of log(crowd) against log(reward).
    ///
    /// This is the premise, as a number. Strongly negative means crowding really does
    /// close the window. Near zero means it does not, whatever the theory says.
    pub median_correlation: f64,
    /// Share of niches where that correlation is negative at all.
    pub share_negative: f64,
    pub verdict: String,
}

/// Correlation strong enough that a strategy could survive fees on it.
pub const STRONG_CORRELATION: f64 = -0.5;
/// Below this in magnitude, the mechanism is not present in any usable form.
pub const WEAK_CORRELATION: f64 = -0.2;

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

fn pearson(xs: &[f64], ys: &[f64]) -> Option<f64> {
    let n = xs.len();
    if n < 5 || ys.len() != n {
        return None;
    }
    let mx = xs.iter().sum::<f64>() / n as f64;
    let my = ys.iter().sum::<f64>() / n as f64;
    let mut num = 0.0;
    let mut dx = 0.0;
    let mut dy = 0.0;
    for i in 0..n {
        let a = xs[i] - mx;
        let b = ys[i] - my;
        num += a * b;
        dx += a * a;
        dy += b * b;
    }
    (dx > 0.0 && dy > 0.0).then(|| num / (dx.sqrt() * dy.sqrt()))
}

impl CohortStudy {
    pub fn run(&self, observations: &[Observation]) -> CohortResult {
        let mut by_niche: BTreeMap<&str, Vec<&Observation>> = BTreeMap::new();
        for o in observations {
            if o.reward_cents.is_some() {
                by_niche.entry(o.niche_id.as_str()).or_default().push(o);
            }
        }

        // day -> the relative readings contributed by each niche on that day
        let mut reward_days: BTreeMap<usize, Vec<f64>> = BTreeMap::new();
        let mut crowd_days: BTreeMap<usize, Vec<f64>> = BTreeMap::new();
        let mut correlations: Vec<f64> = Vec::new();
        let mut counted = 0usize;

        for (_, mut rows) in by_niche {
            rows.sort_by_key(|o| o.ts_ms);
            if rows.len() < self.min_readings {
                continue;
            }
            let t0 = rows[0].ts_ms;
            let Some(r0) = rows[0].reward_cents.filter(|v| *v > 0).map(|v| v as f64) else {
                continue;
            };
            let c0 = rows[0].competitors.filter(|v| *v > 0.0);
            counted += 1;

            let (mut xs, mut ys) = (Vec::new(), Vec::new());
            for o in &rows {
                let age = ((o.ts_ms.saturating_sub(t0)) as f64 / MS_PER_DAY).round() as usize;
                if age > self.max_days {
                    break;
                }
                if let Some(r) = o.reward_cents.filter(|v| *v > 0) {
                    reward_days.entry(age).or_default().push(r as f64 / r0);
                    if let Some(c) = o.competitors.filter(|v| *v > 0.0) {
                        xs.push(c.ln());
                        ys.push((r as f64).ln());
                        if let Some(c0) = c0 {
                            crowd_days.entry(age).or_default().push(c / c0);
                        }
                    }
                }
            }
            if let Some(r) = pearson(&xs, &ys) {
                correlations.push(r);
            }
        }

        let series = |m: BTreeMap<usize, Vec<f64>>| -> Vec<(usize, f64)> {
            m.into_iter()
                .filter(|(_, v)| v.len() >= 5)
                .map(|(d, mut v)| (d, median(&mut v)))
                .collect()
        };

        let median_correlation = median(&mut correlations.clone());
        let share_negative = if correlations.is_empty() {
            f64::NAN
        } else {
            correlations.iter().filter(|c| **c < 0.0).count() as f64 / correlations.len() as f64
        };

        let verdict = if correlations.is_empty() {
            "no niche has both reward and crowd readings; the premise cannot be tested here"
                .to_string()
        } else if median_correlation <= STRONG_CORRELATION {
            format!(
                "crowding depresses reward strongly (r={median_correlation:.3}); a rotation \
                 strategy has something real to work with"
            )
        } else if median_correlation <= WEAK_CORRELATION {
            format!(
                "crowding depresses reward weakly (r={median_correlation:.3}). The mechanism \
                 exists but is small next to the noise, and fees will most likely eat it"
            )
        } else {
            format!(
                "no usable relationship between crowd and reward (r={median_correlation:.3}); \
                 the premise does not hold in this market"
            )
        };

        CohortResult {
            niches: counted,
            reward_by_age: series(reward_days),
            crowd_by_age: series(crowd_days),
            median_correlation,
            share_negative,
            verdict,
        }
    }
}

/// How long an unusually high reward stays unusually high.
///
/// The cohort study needs both reward and crowd readings, and some venues publish only
/// the reward. This asks the same question from the other side: if the mechanism is
/// real, an elevated rate should decay back toward normal on a measurable schedule —
/// and if it does not decay, or reverts instantly, there is no window to rotate into.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Persistence {
    /// Reward level defining "elevated", as a quantile of all readings.
    pub threshold: f64,
    pub baseline: f64,
    /// `(hours ahead, median reward given it was elevated at hour 0)`.
    pub decay: Vec<(u64, f64)>,
    /// Hours for the *excess* over baseline to halve. `None` when no decay is visible.
    pub half_life_hours: Option<f64>,
    pub episodes: usize,
    /// Median length of a continuous elevated episode, in hours.
    pub median_episode_hours: f64,
    pub verdict: String,
}

/// Measure how an elevated reward decays, sampling at the given hour offsets.
pub fn persistence(
    observations: &[Observation],
    quantile: f64,
    horizons: &[u64],
) -> Option<Persistence> {
    let mut by_niche: BTreeMap<&str, Vec<(u64, f64)>> = BTreeMap::new();
    for o in observations {
        if let Some(r) = o.reward_cents.filter(|v| *v > 0) {
            by_niche
                .entry(o.niche_id.as_str())
                .or_default()
                .push((o.ts_ms, r as f64));
        }
    }
    for v in by_niche.values_mut() {
        v.sort_by_key(|(t, _)| *t);
    }

    let mut all: Vec<f64> = by_niche.values().flatten().map(|(_, r)| *r).collect();
    if all.len() < 100 {
        return None;
    }
    all.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = (((all.len() as f64) * quantile) as usize).min(all.len() - 1);
    let threshold = all[idx];
    let baseline = median(&mut all.clone());
    if baseline <= 0.0 {
        return None;
    }

    // Readings are hourly at best, so a horizon is an index offset.
    let mut decay = Vec::new();
    for &h in horizons {
        let mut after = Vec::new();
        for v in by_niche.values() {
            for (i, (_, r)) in v.iter().enumerate() {
                if *r < threshold {
                    continue;
                }
                if let Some((_, later)) = v.get(i + h as usize) {
                    after.push(*later);
                }
            }
        }
        if after.len() >= 20 {
            decay.push((h, median(&mut after)));
        }
    }

    let mut episodes = Vec::new();
    for v in by_niche.values() {
        let mut run = 0u64;
        for (_, r) in v {
            if *r >= threshold {
                run += 1;
            } else {
                if run > 0 {
                    episodes.push(run as f64);
                }
                run = 0;
            }
        }
        if run > 0 {
            episodes.push(run as f64);
        }
    }
    if episodes.is_empty() || decay.len() < 2 {
        return None;
    }

    // Fit the half-life on the excess over baseline, not on the level: a rate that
    // settles at twice normal has not decayed to nothing, and treating it as though it
    // had would invent a window that never closes.
    let half_life_hours = {
        let pts: Vec<(f64, f64)> = decay
            .iter()
            .filter_map(|(h, v)| {
                let excess = v - baseline;
                (excess > 0.0).then_some((*h as f64, excess.ln()))
            })
            .collect();
        if pts.len() < 2 {
            None
        } else {
            let n = pts.len() as f64;
            let mx = pts.iter().map(|(x, _)| x).sum::<f64>() / n;
            let my = pts.iter().map(|(_, y)| y).sum::<f64>() / n;
            let num: f64 = pts.iter().map(|(x, y)| (x - mx) * (y - my)).sum();
            let den: f64 = pts.iter().map(|(x, _)| (x - mx) * (x - mx)).sum();
            (den > 0.0 && num < 0.0).then(|| std::f64::consts::LN_2 / (-num / den))
        }
    };

    let verdict = match half_life_hours {
        Some(h) if h < 12.0 => format!(
            "elevated rates decay with a half-life of {h:.1}h. The mechanism is real, but              the window closes in hours — far faster than an hourly sweep can follow, and              capturing it needs a position held through that window"
        ),
        Some(h) => format!(
            "elevated rates decay with a half-life of {h:.1}h, slow enough to act on"
        ),
        None => "elevated rates do not decay measurably; there is no closing window here".into(),
    };

    Some(Persistence {
        threshold,
        baseline,
        decay,
        half_life_hours,
        episodes: episodes.len(),
        median_episode_hours: median(&mut episodes),
        verdict,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth(id: &str, days: usize, reward: impl Fn(f64) -> f64, crowd: impl Fn(f64) -> f64) -> Vec<Observation> {
        (0..days)
            .map(|i| {
                let t = i as f64;
                Observation::new(id, (t * MS_PER_DAY) as u64, "s")
                    .reward(reward(t).max(1.0) as u64)
                    .competitors(crowd(t).max(1.0))
            })
            .collect()
    }

    #[test]
    fn a_market_where_crowding_kills_the_window_is_recognised() {
        let mut obs = Vec::new();
        for k in 0..8 {
            let id = format!("n{k}");
            // Reward halves as crowd doubles: the mechanism, cleanly.
            obs.extend(synth(&id, 60, |t| 2000.0 * 0.5_f64.powf(t / 20.0), |t| 100.0 * 2.0_f64.powf(t / 20.0)));
        }
        let r = CohortStudy::default().run(&obs);
        assert_eq!(r.niches, 8);
        assert!(r.median_correlation < STRONG_CORRELATION, "r={}", r.median_correlation);
        assert_eq!(r.share_negative, 1.0);
        assert!(r.verdict.contains("strongly"));
        // And the age curve should show the decay.
        let day50 = r.reward_by_age.iter().find(|(d, _)| *d == 50).unwrap().1;
        assert!(day50 < 0.3, "reward at day 50 relative to day 0: {day50}");
    }

    #[test]
    fn a_market_where_reward_ignores_the_crowd_is_reported_as_such() {
        let mut obs = Vec::new();
        let mut rng = hl_core::Rng::new(4);
        for k in 0..8 {
            let id = format!("n{k}");
            let jitter: Vec<f64> = (0..60).map(|_| 0.9 + 0.2 * rng.unit()).collect();
            obs.extend(synth(
                &id,
                60,
                |t| 1000.0 * jitter[t as usize],
                |t| 100.0 * 2.0_f64.powf(t / 20.0),
            ));
        }
        let r = CohortStudy::default().run(&obs);
        assert!(r.median_correlation > WEAK_CORRELATION, "r={}", r.median_correlation);
        assert!(r.verdict.contains("does not hold"));
    }

    #[test]
    fn niches_are_aligned_by_their_own_age_not_by_calendar_date() {
        // Identical decays starting months apart must stack on age, not smear across
        // the calendar. Six niches, because a day-level median needs contributors.
        let mut obs = Vec::new();
        for k in 0..6 {
            let id = format!("n{k}");
            let offset = (k as f64 * 60.0 * MS_PER_DAY) as u64;
            obs.extend(
                synth(&id, 60, |t| 1000.0 * 0.5_f64.powf(t / 20.0), |_| 10.0)
                    .into_iter()
                    .map(|o| {
                        Observation::new(&id, o.ts_ms + offset, "s")
                            .reward(o.reward_cents.unwrap())
                            .competitors(o.competitors.unwrap())
                    }),
            );
        }
        let r = CohortStudy::default().run(&obs);
        let day20 = r.reward_by_age.iter().find(|(d, _)| *d == 20).unwrap().1;
        assert!((day20 - 0.5).abs() < 0.05, "one half-life in, expected ~0.5, got {day20}");
    }

    #[test]
    fn niches_without_crowd_readings_cannot_test_the_premise() {
        let obs: Vec<Observation> = (0..60)
            .map(|i| Observation::new("n", (i as f64 * MS_PER_DAY) as u64, "s").reward(500))
            .collect();
        let r = CohortStudy::default().run(&obs);
        assert!(r.median_correlation.is_nan());
        assert!(r.verdict.contains("cannot be tested"));
    }

    #[test]
    fn a_spike_that_reverts_quickly_shows_a_short_half_life() {
        // Baseline 100 with a spike to 800 decaying 20% an hour — a true half-life of
        // about 3.1h. The elevated stretch has to be a real slice of the sample, or the
        // top decile lands on the baseline and nothing counts as elevated at all.
        let mut obs = Vec::new();
        for k in 0..6 {
            let id = format!("n{k}");
            for i in 0..600u64 {
                let phase = i % 60;
                let v = if phase < 20 {
                    (800.0 * 0.8_f64.powi(phase as i32)).max(100.0)
                } else {
                    100.0
                };
                obs.push(Observation::new(&id, i * 3_600_000, "s").reward(v as u64));
            }
        }
        let p = persistence(&obs, 0.90, &[1, 2, 4, 8]).expect("enough data");
        assert!(p.threshold > p.baseline, "the top decile must actually be elevated");
        let h = p.half_life_hours.expect("a decaying spike has a half-life");
        assert!(
            (0.5..6.0).contains(&h),
            "true half-life is ~3.1h; estimate landed at {h:.2}"
        );
        assert!(p.verdict.contains("closes in hours"));
    }

    #[test]
    fn a_rate_that_stays_elevated_reports_no_closing_window() {
        // A regime shift rather than a spike: high stays high.
        let mut obs = Vec::new();
        for k in 0..6 {
            let id = format!("n{k}");
            for i in 0..400u64 {
                let v = if i < 200 { 100.0 } else { 900.0 };
                obs.push(Observation::new(&id, i * 3_600_000, "s").reward(v as u64));
            }
        }
        let p = persistence(&obs, 0.90, &[1, 6, 24]).expect("enough data");
        assert!(p.half_life_hours.is_none(), "nothing decayed");
        assert!(p.verdict.contains("no closing window"));
    }

    #[test]
    fn too_little_data_declines_to_answer() {
        let obs: Vec<Observation> = (0..20)
            .map(|i| Observation::new("n", i * 3_600_000, "s").reward(100))
            .collect();
        assert!(persistence(&obs, 0.9, &[1, 6]).is_none());
    }

    #[test]
    fn thin_niches_are_excluded() {
        let obs = synth("n", 5, |_| 500.0, |_| 10.0);
        assert_eq!(CohortStudy::default().run(&obs).niches, 0);
    }
}
