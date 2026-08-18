//! Terminal rendering.
//!
//! Output is a decision aid, so it leads with the runway in days and always shows the
//! interval. A half-life printed without its uncertainty invites exactly the false
//! confidence this system exists to avoid.

use hl_core::Signal;
use hl_probe::{CrowdingReport, Decision, Metric};

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
pub fn decision_table(rows: &[(String, Decision, CrowdingReport)]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<28} {:<6} {:>9} {:>16} {:>9}  {}\n",
        "NICHE", "SIGNAL", "RUNWAY", "HALF-LIFE 95% CI", "WEEKLY", "BASIS"
    ));
    out.push_str(&"-".repeat(110));
    out.push('\n');
    for (label, d, r) in rows {
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
            "{:<28} {:<6} {:>9} {:>16} {:>8.0}%  {}\n",
            truncate(label, 28),
            signal_tag(d.signal),
            fmt_days(d.runway_days),
            ci,
            r.weekly_decay * 100.0,
            if basis.is_empty() { "no data".into() } else { basis.join(",") }
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
    fn long_labels_are_truncated_not_wrapped() {
        let s = truncate("github:some-very-long-owner/repository:label", 28);
        assert_eq!(s.chars().count(), 28);
        assert!(s.ends_with('…'));
    }
}
