//! Replay stored observations and ask whether the signal was worth following.
//!
//! Two rules make the answer trustworthy, and both are easy to get wrong:
//!
//! **No lookahead.** At each step the meter is shown only observations timestamped at
//! or before that moment. A backtest that peeks is not a test, it is a story.
//!
//! **A real adversary.** Rotation is compared against naively chasing the top rate with
//! no meter at all. If chasing wins, the meter is decoration — and that is the finding
//! this whole project would need to hear.

use crate::portfolio::{PaperConfig, Portfolio, RateSeries};
use hl_core::{EntryCost, Observation, Signal, MS_PER_DAY};
use hl_probe::{crowding::CrowdingMeter, policy, PolicyConfig};
use serde::{Deserialize, Serialize};

/// Which niches the paper portfolio is allowed to hold.
///
/// Yield pools only. A perp's funding rate is a small term next to the price movement
/// of the thing being held, so "collecting funding" on paper while ignoring that price
/// would produce a number with no relationship to what the position would have done.
/// Simulating it honestly needs spot prices and a delta-neutral second leg; until that
/// exists, funding niches are measured but never traded here.
pub fn is_paper_eligible(niche_id: &str) -> bool {
    niche_id.starts_with("defi:")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub name: String,
    pub final_cents: f64,
    pub return_pct: f64,
    /// Annualised, so runs of different lengths are comparable. Over a short window
    /// this magnifies noise, which is why the raw return and the window are reported
    /// beside it.
    pub apy_pct: f64,
    pub fees_cents: f64,
    pub switches: usize,
    pub accrued_cents: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestResult {
    pub days: f64,
    pub steps: usize,
    pub eligible_niches: usize,
    pub outcomes: Vec<Outcome>,
    /// Set when the window is too short for the result to mean anything.
    pub warning: Option<String>,
}

impl BacktestResult {
    pub fn best(&self) -> Option<&Outcome> {
        self.outcomes
            .iter()
            .max_by(|a, b| a.final_cents.partial_cmp(&b.final_cents).unwrap_or(std::cmp::Ordering::Equal))
    }
    pub fn get(&self, name: &str) -> Option<&Outcome> {
        self.outcomes.iter().find(|o| o.name == name)
    }
}

/// Below this the annualised figures are noise dressed as signal.
pub const MIN_MEANINGFUL_DAYS: f64 = 3.0;

/// How long the buy-and-hold benchmark waits before judging which rates are best.
///
/// Long enough that every pool reporting daily has been heard from at least once. This
/// gives the benchmark no forward information: it ranks on rates already published by
/// then, and the strategies it is compared against are free to act over the same days.
pub const OPENING_WARMUP_DAYS: f64 = 7.0;

#[derive(Debug, Clone)]
pub struct Backtest {
    pub cfg: PaperConfig,
    pub policy: PolicyConfig,
    /// How often the portfolio may act.
    pub step_days: f64,
    /// How long a position is assumed to last when deciding whether a switch repays
    /// its own cost.
    pub expected_hold_days: f64,
    /// How much history the meter is shown at each step.
    ///
    /// Bounded for correctness before performance: a trend fitted over three years is
    /// not the trend anyone would act on, and it would drown a rate that turned last
    /// week. It also keeps the replay linear rather than quadratic in history.
    pub lookback_days: f64,
}

impl Default for Backtest {
    fn default() -> Self {
        Self {
            cfg: PaperConfig::default(),
            policy: PolicyConfig::default(),
            step_days: 1.0,
            expected_hold_days: 14.0,
            lookback_days: 21.0,
        }
    }
}

impl Backtest {
    pub fn run(&self, observations: &[Observation]) -> BacktestResult {
        let eligible: Vec<&Observation> = observations
            .iter()
            .filter(|o| is_paper_eligible(&o.niche_id))
            .collect();
        let series = RateSeries::from_observations(eligible.iter().copied());
        let owned: Vec<Observation> = eligible.into_iter().cloned().collect();

        let (Some(start), Some(end)) = (series.first_ts(), series.last_ts()) else {
            return BacktestResult {
                days: 0.0,
                steps: 0,
                eligible_niches: 0,
                outcomes: Vec::new(),
                warning: Some("no eligible observations yet".into()),
            };
        };
        let days = (end.saturating_sub(start)) as f64 / MS_PER_DAY;
        let step_ms = (self.step_days * MS_PER_DAY) as u64;

        let mut times: Vec<u64> = Vec::new();
        let mut t = start;
        while t < end {
            times.push(t);
            t = t.saturating_add(step_ms.max(1));
        }
        times.push(end);

        let rate_at = |n: &str, at: u64| series.rate_at(n, at);
        let mut outcomes = Vec::new();

        // Index once, by niche, in time order. Re-filtering the whole history for every
        // niche at every step is what turned a three-year replay into a hang.
        let mut by_niche: std::collections::BTreeMap<String, Vec<Observation>> =
            Default::default();
        for o in &owned {
            by_niche.entry(o.niche_id.clone()).or_default().push(o.clone());
        }
        for v in by_niche.values_mut() {
            v.sort_by_key(|o| o.ts_ms);
        }
        let lookback_ms = (self.lookback_days * MS_PER_DAY) as u64;
        let window = |niche: &str, now: u64| -> Vec<Observation> {
            let Some(v) = by_niche.get(niche) else {
                return Vec::new();
            };
            let from = now.saturating_sub(lookback_ms);
            let lo = v.partition_point(|o| o.ts_ms < from);
            let hi = v.partition_point(|o| o.ts_ms <= now);
            v[lo..hi].to_vec()
        };

        // The strategy under test: hold what the meter says to hold.
        outcomes.push(self.simulate("rotation (meter)", &times, &rate_at, |now, _held| {
            let meter = CrowdingMeter::default();
            let mut ranked: Vec<(String, f64)> = series
                .niches()
                .filter_map(|n| {
                    let report = meter.report(n, &window(n, now), now);
                    let d = policy::decide(&report, &EntryCost::default(), &self.policy);
                    // Insufficient is not a reason to avoid a niche — it is the normal
                    // state early on, and refusing to hold anything until the estimator
                    // speaks would just sit in cash for the first days.
                    matches!(d.signal, Signal::Enter | Signal::Hold | Signal::Insufficient)
                        .then(|| (n.clone(), series.rate_at(n, now).unwrap_or(0.0)))
                })
                .collect();
            ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            ranked.into_iter().map(|(n, _)| n).collect()
        }));

        // The adversary: chase the best rate, ignore the meter entirely.
        outcomes.push(self.simulate("chase top rate", &times, &rate_at, |now, _held| {
            let mut ranked: Vec<(String, f64)> = series
                .niches()
                .filter_map(|n| series.rate_at(n, now).map(|r| (n.clone(), r)))
                .collect();
            ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            ranked.into_iter().map(|(n, _)| n).collect()
        }));

        // The floor: pick the best rates at the outset and never move again.
        //
        // Ranked over a short warm-up rather than at the first millisecond. Each pool
        // reports at its own time of day, so requiring a reading at the exact global
        // minimum admitted whichever pool happened to be stamped earliest — one niche
        // out of thirty-five — and called it "best". That is not a benchmark, it is a
        // coin toss, and rotation was being measured against it.
        let opening: Vec<String> = {
            let warmup = start.saturating_add((OPENING_WARMUP_DAYS * MS_PER_DAY) as u64);
            let mut r: Vec<(String, f64)> = series
                .niches()
                .filter_map(|n| series.rate_at(n, warmup).map(|v| (n.clone(), v)))
                .collect();
            r.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            r.into_iter().map(|(n, _)| n).collect()
        };
        outcomes.push(self.simulate("hold best at start", &times, &rate_at, |_, _| opening.clone()));

        // The same meter, but only acting when the move pays for itself.
        let fee_cfg = self.cfg;
        outcomes.push(self.simulate("rotation (fee-aware)", &times, &rate_at, |now, held| {
            let meter = CrowdingMeter::default();
            let mut ranked: Vec<(String, f64)> = series
                .niches()
                .filter_map(|n| {
                    let report = meter.report(n, &window(n, now), now);
                    let d = policy::decide(&report, &EntryCost::default(), &self.policy);
                    matches!(d.signal, Signal::Enter | Signal::Hold | Signal::Insufficient)
                        .then(|| (n.clone(), series.rate_at(n, now).unwrap_or(0.0)))
                })
                .collect();
            ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

            // Anything already held stays unless a candidate clears the hurdle its own
            // switching cost sets.
            let per_slot = fee_cfg.starting_cents as f64 / fee_cfg.max_positions.max(1) as f64;
            let hurdle = fee_cfg
                .breakeven_gain_bps(per_slot, self.expected_hold_days)
                .max(fee_cfg.switch_gain_bps as f64);
            let worst_held = held
                .iter()
                .filter_map(|h| series.rate_at(h, now))
                .fold(f64::INFINITY, f64::min);

            let mut out: Vec<String> = held.to_vec();
            for (n, rate) in ranked {
                if out.len() >= fee_cfg.max_positions {
                    break;
                }
                if out.contains(&n) {
                    continue;
                }
                // Filling an empty slot is free of any incumbent to beat.
                if held.len() < fee_cfg.max_positions && !worst_held.is_finite() {
                    out.push(n);
                    continue;
                }
                if rate > worst_held + hurdle {
                    // Displace the weakest holding, not an arbitrary one.
                    if let Some(pos) = out
                        .iter()
                        .position(|h| series.rate_at(h, now).map(|r| r == worst_held).unwrap_or(false))
                    {
                        out[pos] = n;
                    } else {
                        out.push(n);
                    }
                } else if out.len() < fee_cfg.max_positions {
                    out.push(n);
                }
            }
            out
        }));

        BacktestResult {
            days,
            steps: times.len(),
            eligible_niches: series.niches().count(),
            outcomes,
            warning: (days < MIN_MEANINGFUL_DAYS).then(|| {
                format!(
                    "only {days:.2} days of history: annualised figures here are noise, \
                     and the strategies have barely had a chance to differ"
                )
            }),
        }
    }

    fn simulate(
        &self,
        name: &str,
        times: &[u64],
        rate_at: &dyn Fn(&str, u64) -> Option<f64>,
        mut targets_at: impl FnMut(u64, &[String]) -> Vec<String>,
    ) -> Outcome {
        let mut p = Portfolio::new(&self.cfg);
        for (i, &now) in times.iter().enumerate() {
            p.accrue(now, rate_at);
            // No trading on the final mark: it exists to value the book, and acting on
            // it would book fees the run never had time to earn back.
            if i + 1 < times.len() {
                let held: Vec<String> = p.positions.iter().map(|x| x.niche_id.clone()).collect();
                let targets = targets_at(now, &held);
                p.rebalance(&targets, now, &self.cfg);
            }
        }
        let start = self.cfg.starting_cents as f64;
        let final_cents = p.total_cents();
        let days = (times.last().copied().unwrap_or(0).saturating_sub(times[0])) as f64 / MS_PER_DAY;
        let return_pct = (final_cents / start - 1.0) * 100.0;
        Outcome {
            name: name.to_string(),
            final_cents,
            return_pct,
            apy_pct: if days > 0.0 { return_pct * 365.0 / days } else { 0.0 },
            fees_cents: p.fees_paid_cents,
            switches: p.switches,
            accrued_cents: p.accrued_cents,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn days_ms(d: f64) -> u64 {
        (d * MS_PER_DAY) as u64
    }

    /// A niche whose rate follows `f(day)`.
    fn niche(id: &str, days: usize, f: impl Fn(f64) -> f64) -> Vec<Observation> {
        (0..days * 4)
            .map(|i| {
                let t = i as f64 / 4.0;
                Observation::new(id, days_ms(t), "defillama").reward(f(t).max(1.0) as u64)
            })
            .collect()
    }

    #[test]
    fn the_hold_benchmark_ranks_on_rates_not_on_reporting_times() {
        // Two pools reporting at different times of day. The worse pool reports first.
        // Requiring a reading at the exact first timestamp would hand the benchmark to
        // it and call that "hold the best".
        let mut obs: Vec<Observation> = (0..60)
            .map(|i| {
                Observation::new("defi:early-but-poor", days_ms(i as f64) + 1, "defillama")
                    .reward(100)
            })
            .collect();
        obs.extend((0..60).map(|i| {
            Observation::new("defi:late-but-rich", days_ms(i as f64) + 50_000_000, "defillama")
                .reward(2000)
        }));
        let bt = Backtest {
            cfg: PaperConfig { max_positions: 1, ..Default::default() },
            ..Default::default()
        };
        let r = bt.run(&obs);
        let hold = r.get("hold best at start").unwrap();
        // 20% for most of two months should clearly beat 1%.
        assert!(
            hold.return_pct > 2.0,
            "the benchmark took the earliest reporter rather than the best rate: {:.3}%",
            hold.return_pct
        );
    }

    #[test]
    fn funding_niches_are_measured_but_never_traded() {
        assert!(is_paper_eligible("defi:Base:aave-v3:USDC"));
        assert!(!is_paper_eligible("perp:hyperliquid:BTC"));
        assert!(!is_paper_eligible("gh:owner/repo"));

        let mut obs = niche("perp:hyperliquid:BTC", 30, |_| 30_000.0);
        obs.extend(niche("defi:real", 30, |_| 500.0));
        let r = Backtest::default().run(&obs);
        assert_eq!(r.eligible_niches, 1, "the perp must not enter the book");
    }

    #[test]
    fn an_empty_history_says_so_instead_of_reporting_zero_percent() {
        let r = Backtest::default().run(&[]);
        assert!(r.outcomes.is_empty());
        assert!(r.warning.unwrap().contains("no eligible observations"));
    }

    #[test]
    fn a_short_window_is_flagged_as_meaningless() {
        let obs = niche("defi:a", 1, |_| 500.0);
        let r = Backtest::default().run(&obs);
        assert!(r.warning.unwrap().contains("noise"));
    }

    #[test]
    fn rotation_beats_holding_when_the_held_rate_collapses() {
        // One pool starts high and decays to nothing; another is steady. Holding the
        // day-one winner is exactly the mistake this project exists to avoid.
        let mut obs = niche("defi:decaying", 40, |t| 4000.0 * 0.5_f64.powf(t / 4.0));
        obs.extend(niche("defi:steady", 40, |_| 800.0));
        // One slot, so the strategies are actually forced to choose. With room for
        // every niche they all hold everything and nothing can be learned.
        let bt = Backtest {
            cfg: PaperConfig { max_positions: 1, ..Default::default() },
            ..Default::default()
        };
        let r = bt.run(&obs);

        let rot = r.get("rotation (meter)").unwrap();
        let hold = r.get("hold best at start").unwrap();
        assert!(
            rot.final_cents > hold.final_cents,
            "rotation {:.0} should beat hold {:.0}",
            rot.final_cents,
            hold.final_cents
        );
    }

    #[test]
    fn churning_between_identical_rates_loses_to_sitting_still() {
        // Every pool pays the same. There is nothing to gain by moving, so any strategy
        // that moves must end behind one that does not — the fee floor is real.
        let mut obs = niche("defi:a", 30, |_| 600.0);
        obs.extend(niche("defi:b", 30, |_| 600.0));
        let cfg = PaperConfig {
            max_positions: 1,
            switch_fee_cents: 50,
            switch_fee_bps: 10,
            ..Default::default()
        };
        let bt = Backtest {
            cfg,
            ..Default::default()
        };
        let r = bt.run(&obs);
        let hold = r.get("hold best at start").unwrap();
        assert_eq!(hold.switches, 1, "opens once, never moves again");
        for o in &r.outcomes {
            assert!(
                o.final_cents <= hold.final_cents + 1.0,
                "{} beat buy-and-hold on identical rates, which cannot be real",
                o.name
            );
        }
    }

    #[test]
    fn every_strategy_pays_for_the_moves_it_makes() {
        let mut obs = niche("defi:a", 20, |t| 1000.0 + 400.0 * (t / 3.0).sin());
        obs.extend(niche("defi:b", 20, |t| 1000.0 - 400.0 * (t / 3.0).sin()));
        let bt = Backtest {
            cfg: PaperConfig { max_positions: 1, ..Default::default() },
            ..Default::default()
        };
        let r = bt.run(&obs);
        let chase = r.get("chase top rate").unwrap();
        assert!(chase.switches > 2, "a flip-flopping rate should force switches");
        assert!(chase.fees_cents > 0.0, "and each switch must cost something");
    }
}

#[cfg(test)]
mod diagnose {
    use super::*;

    /// Not a test of behaviour — a probe for the held-out window anomaly.
    #[test]
    #[ignore]
    fn dump_hold_on_a_late_window() {
        let path = std::env::var("HL_OBS").unwrap_or_default();
        let Ok(text) = std::fs::read_to_string(&path) else {
            eprintln!("set HL_OBS to an observations.jsonl");
            return;
        };
        let all: Vec<Observation> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let mut ts: Vec<u64> = all.iter().map(|o| o.ts_ms).collect();
        ts.sort_unstable();
        let split = ts[0] + ((ts[ts.len() - 1] - ts[0]) as f64 * 0.67) as u64;
        let test: Vec<Observation> = all.into_iter().filter(|o| o.ts_ms > split).collect();

        let series = RateSeries::from_observations(test.iter());
        let start = series.first_ts().unwrap();
        let with_reading_at_start = series
            .niches()
            .filter(|n| series.rate_at(n, start).is_some())
            .count();
        eprintln!(
            "test niches: {}, with a reading at the very first timestamp: {}",
            series.niches().count(),
            with_reading_at_start
        );

        let r = Backtest::default().run(&test);
        for o in &r.outcomes {
            eprintln!(
                "{:<22} final {:.2} accrued {:.2} fees {:.2} switches {}",
                o.name, o.final_cents, o.accrued_cents, o.fees_cents, o.switches
            );
        }
    }
}
