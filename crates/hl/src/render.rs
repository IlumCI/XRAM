//! Terminal rendering.
//!
//! Output is a decision aid, so it leads with the runway in days and always shows the
//! interval. A half-life printed without its uncertainty invites exactly the false
//! confidence this system exists to avoid.

use hl_core::Signal;
use hl_probe::{CrowdingReport, Decision, Metric};

/// One niche as it appears in the report.
pub struct Row {
    pub label: String,
    /// Raw observations stored for this niche. Distinct from the fitted sample count:
    /// a niche can hold observations and still fit nothing, and the gap between the two
    /// is exactly "how much longer until this says something".
    pub observations: usize,
    pub decision: Decision,
    pub report: CrowdingReport,
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
        "{:<28} {:<6} {:>5} {:>9} {:>16} {:>8}  {}\n",
        "NICHE", "SIGNAL", "OBS", "RUNWAY", "HALF-LIFE 95% CI", "WEEKLY", "BASIS"
    ));
    out.push_str(&"-".repeat(112));
    out.push('\n');
    for Row {
        label,
        observations,
        decision: d,
        report: r,
    } in rows
    {
        let ci = r
            .metrics
            .iter()
            .find(|m| m.metric == Metric::Reward)
            .or_else(|| r.metrics.first())
            .and_then(|m| m.fit.half_life_ci95())
            .map(|(lo, hi)| format!("{lo:.1}-{hi:.1}d"))
            .unwrap_or_else(|| "-".into());
        let basis: Vec<&str> = r.metrics.iter().map(|m| m.metric.label()).collect();
        out.push_str(&format!(
            "{:<28} {:<6} {:>5} {:>9} {:>16} {:>7.0}%  {}\n",
            truncate(label, 28),
            signal_tag(d.signal),
            observations,
            fmt_days(d.runway_days),
            ci,
            r.weekly_decay * 100.0,
            if basis.is_empty() { "none yet".into() } else { basis.join(",") }
        ));
        out.push_str(&format!("{:<28} {}\n", "", d.reason));
    }
    out
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
        }]);
        assert!(table.contains("    3"), "raw count must appear: {table}");
        assert!(table.contains("unknown"));
        assert!(table.contains("none yet"));
    }

    #[test]
    fn long_labels_are_truncated_not_wrapped() {
        let s = truncate("github:some-very-long-owner/repository:label", 28);
        assert_eq!(s.chars().count(), 28);
        assert!(s.ends_with('…'));
    }
}
