//! A notional portfolio that accrues rates and pays to move.
//!
//! Nothing here touches money. The point is to find out, for free, whether the signal
//! this project produces is worth acting on — and the only honest way to answer that is
//! to make rotation pay its own costs.

use hl_core::MS_PER_DAY;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PaperConfig {
    pub starting_cents: u64,
    /// How many niches to hold at once. Concentration raises return and raises the cost
    /// of being wrong about any one of them.
    pub max_positions: usize,
    /// Flat cost of opening or closing a position, in cents. On an L2 this is gas, and
    /// it is what stops rotation from looking free.
    pub switch_fee_cents: u64,
    /// Proportional cost of a switch, in basis points of the moved notional.
    pub switch_fee_bps: u64,
    /// A candidate must beat the position it would replace by this margin, in basis
    /// points of annual rate, before a switch is worth making.
    ///
    /// Without this the policy will happily pay a fee to move between two rates that
    /// differ by noise, which is exactly how the first backtest spent $185 in fees to
    /// earn $89.
    pub switch_gain_bps: u64,
}

impl Default for PaperConfig {
    fn default() -> Self {
        Self {
            // A round notional. Returns are reported as percentages, so the absolute
            // figure only sets how badly flat fees bite — which is the point of
            // reporting it alongside.
            starting_cents: 100_000,
            max_positions: 3,
            switch_fee_cents: 20,
            switch_fee_bps: 5,
            switch_gain_bps: 0,
        }
    }
}

impl PaperConfig {
    /// The rate advantage a switch must clear to repay itself over `hold_days`.
    ///
    /// Expressed in basis points of annual rate: moving `notional` costs a fee, and the
    /// extra rate has to earn that back within the time the position is expected to
    /// last.
    pub fn breakeven_gain_bps(&self, notional: f64, hold_days: f64) -> f64 {
        if notional <= 0.0 || hold_days <= 0.0 {
            return f64::INFINITY;
        }
        // Entering and leaving are both paid for.
        let fee = 2.0 * (self.switch_fee_cents as f64 + notional * self.switch_fee_bps as f64 / 10_000.0);
        (fee / notional) * (365.0 / hold_days) * 10_000.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub niche_id: String,
    pub notional_cents: f64,
    pub opened_ms: u64,
    /// Last moment this position's accrual was brought up to date.
    pub accrued_to_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Portfolio {
    pub cash_cents: f64,
    pub positions: Vec<Position>,
    pub fees_paid_cents: f64,
    pub switches: usize,
    pub accrued_cents: f64,
}

impl Portfolio {
    pub fn new(cfg: &PaperConfig) -> Self {
        Self {
            cash_cents: cfg.starting_cents as f64,
            positions: Vec::new(),
            fees_paid_cents: 0.0,
            switches: 0,
            accrued_cents: 0.0,
        }
    }

    pub fn total_cents(&self) -> f64 {
        self.cash_cents + self.positions.iter().map(|p| p.notional_cents).sum::<f64>()
    }

    pub fn holds(&self, niche_id: &str) -> bool {
        self.positions.iter().any(|p| p.niche_id == niche_id)
    }

    /// Credit each position with the rate it earned since it was last brought up to
    /// date.
    ///
    /// `rate_bps_at` returns the rate in force for a niche at a moment — the last
    /// reading at or before it, never a later one.
    pub fn accrue(
        &mut self,
        now_ms: u64,
        rate_bps_at: &dyn Fn(&str, u64) -> Option<f64>,
    ) {
        for p in &mut self.positions {
            if now_ms <= p.accrued_to_ms {
                continue;
            }
            let days = (now_ms - p.accrued_to_ms) as f64 / MS_PER_DAY;
            // The rate in force at the *start* of the interval. Using the end would
            // credit the position with a rate it had not yet been offered.
            if let Some(bps) = rate_bps_at(&p.niche_id, p.accrued_to_ms) {
                let earned = p.notional_cents * (bps / 10_000.0) * (days / 365.0);
                p.notional_cents += earned;
                self.accrued_cents += earned;
            }
            p.accrued_to_ms = now_ms;
        }
    }

    /// Move to holding exactly `targets`, paying to enter and to leave.
    pub fn rebalance(&mut self, targets: &[String], now_ms: u64, cfg: &PaperConfig) {
        let wanted: Vec<&String> = targets.iter().take(cfg.max_positions).collect();

        // Close anything no longer wanted.
        let mut kept = Vec::new();
        for p in std::mem::take(&mut self.positions) {
            if wanted.iter().any(|w| **w == p.niche_id) {
                kept.push(p);
            } else {
                let fee = self.fee_for(p.notional_cents, cfg);
                self.cash_cents += p.notional_cents - fee;
                self.fees_paid_cents += fee;
                self.switches += 1;
            }
        }
        self.positions = kept;

        let to_open: Vec<String> = wanted
            .iter()
            .filter(|w| !self.holds(w))
            .map(|w| (*w).clone())
            .collect();
        if to_open.is_empty() {
            return;
        }

        // Spread the whole book evenly across the new target set. Existing positions
        // are left where they are: churning them to equalise weights would pay fees for
        // no change in what is held.
        let free = self.cash_cents;
        if free <= 0.0 {
            return;
        }
        let each = free / to_open.len() as f64;
        for niche_id in to_open {
            let fee = self.fee_for(each, cfg);
            if each <= fee {
                continue;
            }
            self.cash_cents -= each;
            self.fees_paid_cents += fee;
            self.switches += 1;
            self.positions.push(Position {
                niche_id,
                notional_cents: each - fee,
                opened_ms: now_ms,
                accrued_to_ms: now_ms,
            });
        }
    }

    fn fee_for(&self, notional: f64, cfg: &PaperConfig) -> f64 {
        cfg.switch_fee_cents as f64 + notional * (cfg.switch_fee_bps as f64 / 10_000.0)
    }
}

/// Rate readings per niche, ordered in time, answering "what was on offer at T".
#[derive(Debug, Default, Clone)]
pub struct RateSeries {
    series: BTreeMap<String, Vec<(u64, f64)>>,
}

impl RateSeries {
    pub fn from_observations<'a>(obs: impl IntoIterator<Item = &'a hl_core::Observation>) -> Self {
        let mut series: BTreeMap<String, Vec<(u64, f64)>> = BTreeMap::new();
        for o in obs {
            if let Some(bps) = o.reward_cents {
                series
                    .entry(o.niche_id.clone())
                    .or_default()
                    .push((o.ts_ms, bps as f64));
            }
        }
        for v in series.values_mut() {
            v.sort_by_key(|(ts, _)| *ts);
        }
        Self { series }
    }

    /// The last reading at or before `at_ms`. Never a later one — that would be
    /// lookahead, and lookahead is how a backtest lies.
    pub fn rate_at(&self, niche_id: &str, at_ms: u64) -> Option<f64> {
        let v = self.series.get(niche_id)?;
        let idx = v.partition_point(|(ts, _)| *ts <= at_ms);
        (idx > 0).then(|| v[idx - 1].1)
    }

    pub fn niches(&self) -> impl Iterator<Item = &String> {
        self.series.keys()
    }

    pub fn first_ts(&self) -> Option<u64> {
        self.series.values().filter_map(|v| v.first().map(|(t, _)| *t)).min()
    }

    pub fn last_ts(&self) -> Option<u64> {
        self.series.values().filter_map(|v| v.last().map(|(t, _)| *t)).max()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_core::Observation;

    fn days(d: f64) -> u64 {
        (d * MS_PER_DAY) as u64
    }

    fn cfg_free() -> PaperConfig {
        PaperConfig {
            switch_fee_cents: 0,
            switch_fee_bps: 0,
            ..Default::default()
        }
    }

    #[test]
    fn a_year_at_ten_percent_earns_ten_percent() {
        let mut p = Portfolio::new(&cfg_free());
        p.rebalance(&["a".to_string()], 0, &cfg_free());
        let rate = |_: &str, _: u64| Some(1000.0); // 10.00%
        p.accrue(days(365.0), &rate);
        let total = p.total_cents();
        assert!((total - 110_000.0).abs() < 1.0, "got {total}");
    }

    #[test]
    fn accrual_uses_the_rate_in_force_not_a_later_one() {
        // Rate is 20% until day 10, then 2%. A position held over the first ten days
        // must earn the 20%, not be repriced by what came after.
        let obs = vec![
            Observation::new("a", days(1.0), "s").reward(2000),
            Observation::new("a", days(10.0), "s").reward(200),
        ];
        let series = RateSeries::from_observations(&obs);
        assert_eq!(series.rate_at("a", days(5.0)), Some(2000.0));
        assert_eq!(series.rate_at("a", days(10.0)), Some(200.0));
        assert_eq!(series.rate_at("a", days(0.5)), None, "before the first reading");
    }

    #[test]
    fn switching_costs_are_actually_paid() {
        let cfg = PaperConfig {
            switch_fee_cents: 100,
            switch_fee_bps: 50,
            max_positions: 1,
            ..Default::default()
        };
        let mut p = Portfolio::new(&cfg);
        p.rebalance(&["a".to_string()], 0, &cfg);
        let opened = p.total_cents();
        assert!(opened < 100_000.0, "opening must cost something");
        p.rebalance(&["b".to_string()], days(1.0), &cfg);
        assert_eq!(p.switches, 3, "one open, one close, one open");
        assert!(p.fees_paid_cents > 200.0);
        assert!(p.total_cents() < opened, "churn without yield must lose money");
    }

    #[test]
    fn holding_through_a_rebalance_is_not_charged() {
        let cfg = PaperConfig {
            switch_fee_cents: 100,
            max_positions: 1,
            ..Default::default()
        };
        let mut p = Portfolio::new(&cfg);
        p.rebalance(&["a".to_string()], 0, &cfg);
        let after_open = p.fees_paid_cents;
        p.rebalance(&["a".to_string()], days(1.0), &cfg);
        assert_eq!(p.fees_paid_cents, after_open, "staying put is free");
        assert_eq!(p.switches, 1);
    }

    #[test]
    fn breakeven_scales_with_fees_and_shrinks_with_patience() {
        let cfg = PaperConfig {
            switch_fee_cents: 20,
            switch_fee_bps: 5,
            ..Default::default()
        };
        // $333 held for a week: two fees of ~$0.37 against a week of earnings.
        let week = cfg.breakeven_gain_bps(33_300.0, 7.0);
        let month = cfg.breakeven_gain_bps(33_300.0, 30.0);
        assert!(week > month, "patience lowers the bar a switch has to clear");
        // Holding four times as long should cost roughly a quarter as much in rate.
        assert!((week / month - 30.0 / 7.0).abs() < 0.01);
        assert!(cfg.breakeven_gain_bps(0.0, 7.0).is_infinite());
    }

    #[test]
    fn positions_are_capped() {
        let cfg = PaperConfig {
            max_positions: 2,
            ..cfg_free()
        };
        let mut p = Portfolio::new(&cfg);
        let t: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        p.rebalance(&t, 0, &cfg);
        assert_eq!(p.positions.len(), 2);
    }

    #[test]
    fn a_niche_with_no_reading_accrues_nothing_rather_than_guessing() {
        let mut p = Portfolio::new(&cfg_free());
        p.rebalance(&["unknown".to_string()], 0, &cfg_free());
        p.accrue(days(30.0), &|_, _| None);
        assert!((p.total_cents() - 100_000.0).abs() < 1e-6);
        assert_eq!(p.accrued_cents, 0.0);
    }
}
