//! Terminal rendering.
//!
//! Output is a decision aid, so it leads with the runway in days and always shows the
//! interval. A half-life printed without its uncertainty invites exactly the false
//! confidence this system exists to avoid.

use hl_core::Signal;
use hl_probe::{CrowdingReport, Decision};

/// One niche as it appears in the report.
pub struct Row {
    pub label: String,
    /// Raw observations stored for this niche. Distinct from the fitted sample count:
    /// a niche can hold observations and still fit nothing, and the gap between the two
    /// is exactly "how much longer until this says something".
    pub observations: usize,
    pub decision: Decision,
    pub report: CrowdingReport,
    /// Most recent reward reading. Its meaning is per-source, which is why the unit
    /// travels with it — rendering a yield's basis points as dollars turned 3.52% APY
    /// into "$3".
    pub value: Option<u64>,
    pub unit: ValueUnit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueUnit {
    /// Cents: a bounty size, or prize per entering team.
    Money,
    /// Hundredths of a percent: an annualised rate.
    BasisPoints,
}

impl ValueUnit {
    /// Pick the unit from the niche id, since the reading's meaning follows its source.
    pub fn for_niche(niche_id: &str) -> ValueUnit {
        if niche_id.starts_with("defi:") {
            ValueUnit::BasisPoints
        } else {
            ValueUnit::Money
        }
    }
}

pub fn signal_tag(s: Signal) -> &'static str {
    match s {
        Signal::Enter => "ENTER",
        Signal::Hold => "HOLD ",
        Signal::Exit => "EXIT ",
        Signal::Insufficient => "?    ",
    }
}

/// `None` here means the runway is unmeasured, which is not the same as unbounded and
/// certainly not the same as zero.
pub fn fmt_days(d: Option<f64>) -> String {
    match d {
        Some(d) if d >= 1000.0 => ">1000d".into(),
        Some(d) => format!("{d:.1}d"),
        None => "unknown".into(),
    }
}

/// One line per niche: the table you actually rotate on.
pub fn decision_table(rows: &[Row]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<34} {:<6} {:>5} {:>9} {:>10} {:>8}  {}\n",
        "NICHE", "SIGNAL", "OBS", "RUNWAY", "VALUE", "WEEKLY", "BASIS"
    ));
    out.push_str(&"-".repeat(108));
    out.push('\n');
    for Row {
        label,
        observations,
        decision: d,
        report: r,
        value,
        unit,
    } in rows
    {
        let basis: Vec<&str> = r.metrics.iter().map(|m| m.metric.label()).collect();
        out.push_str(&format!(
            "{:<34} {:<6} {:>5} {:>9} {:>10} {:>7.0}%  {}\n",
            truncate(label, 34),
            signal_tag(d.signal),
            observations,
            fmt_days(d.runway_days),
            fmt_value(*value, *unit),
            r.weekly_decay * 100.0,
            if basis.is_empty() { "none yet".into() } else { basis.join(",") }
        ));
        out.push_str(&format!("{:<34} {}\n", "", d.reason));
    }
    out
}

/// A reading rendered in its own unit.
pub fn fmt_value(value: Option<u64>, unit: ValueUnit) -> String {
    match unit {
        ValueUnit::Money => fmt_money(value),
        ValueUnit::BasisPoints => match value {
            None => "-".into(),
            Some(bps) => format!("{:.2}%", bps as f64 / 100.0),
        },
    }
}

/// Money, at whatever precision is legible. Sub-dollar values keep their cents because
/// a niche worth $0.09 per attempt and one worth $9 are different propositions.
pub fn fmt_money(cents: Option<u64>) -> String {
    match cents {
        None => "-".into(),
        Some(c) if c >= 100_000 => format!("${}k", c / 100_000),
        Some(c) if c >= 100 => format!("${}", c / 100),
        Some(c) => format!("${}.{:02}", c / 100, c % 100),
    }
}

/// Public wrapper so other commands can share the table's truncation rule.
pub fn truncate_pub(s: &str, n: usize) -> String {
    truncate(s, n)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmeasured_runways_are_not_printed_as_zero() {
        assert_eq!(fmt_days(None), "unknown");
        assert_eq!(fmt_days(Some(3.25)), "3.2d");
        assert_eq!(fmt_days(Some(5000.0)), ">1000d");
    }

    #[test]
    fn a_niche_with_observations_but_no_fit_shows_the_gap() {
        // The state every niche is in for its first day: data arriving, nothing
        // fittable yet. Showing 0 samples while the store holds observations reads as a
        // broken collector, so the raw count is displayed alongside.
        let r = hl_probe::CrowdingMeter::default().report("n", &[], 0);
        let d = hl_probe::policy::decide(
            &r,
            &hl_core::EntryCost::default(),
            &hl_probe::PolicyConfig::default(),
        );
        let table = decision_table(&[Row {
            label: "n".into(),
            observations: 3,
            decision: d,
            report: r,
            value: None,
            unit: ValueUnit::Money,
        }]);
        assert!(table.contains("    3"), "raw count must appear: {table}");
        assert!(table.contains("unknown"));
        assert!(table.contains("none yet"));
    }

    #[test]
    fn a_rate_is_rendered_as_a_rate_not_as_dollars() {
        // 352 basis points is 3.52% APY. Rendered as money it read "$3", which is both
        // wrong and plausible enough to be believed.
        assert_eq!(fmt_value(Some(352), ValueUnit::BasisPoints), "3.52%");
        assert_eq!(fmt_value(Some(352), ValueUnit::Money), "$3");
        assert_eq!(fmt_value(None, ValueUnit::BasisPoints), "-");
        assert_eq!(ValueUnit::for_niche("defi:Base:aave-v3:USDC"), ValueUnit::BasisPoints);
        assert_eq!(ValueUnit::for_niche("gh:owner/repo"), ValueUnit::Money);
    }

    #[test]
    fn money_stays_legible_across_four_orders_of_magnitude() {
        assert_eq!(fmt_money(None), "-");
        assert_eq!(fmt_money(Some(9)), "$0.09");
        assert_eq!(fmt_money(Some(316_900)), "$3k");
        assert_eq!(fmt_money(Some(4_100)), "$41");
    }

    #[test]
    fn long_labels_are_truncated_not_wrapped() {
        let s = truncate("github:some-very-long-owner/repository:label", 28);
        assert_eq!(s.chars().count(), 28);
        assert!(s.ends_with('…'));
    }
}
