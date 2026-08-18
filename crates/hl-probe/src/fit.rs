//! Exponential trend fitting.
//!
//! Every crowding metric we care about moves multiplicatively: claim latency halves,
//! reward halves, competitor counts double. So the model is `y(t) = A·e^(-λt)` with `t`
//! in days, fitted in log space.
//!
//! Scraped data is dirty — a single mis-parsed reward or one opportunity that sat
//! unclaimed over a weekend will drag a least-squares line badly. The default estimator
//! is therefore Theil–Sen (median of pairwise slopes), which tolerates up to ~29%
//! corrupted points, with ordinary least squares available for comparison.

use hl_core::{Confidence, MS_PER_DAY};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FitMethod {
    /// Median of pairwise slopes. Resistant to outliers. Default.
    TheilSen,
    OrdinaryLeastSquares,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FitError {
    TooFewPoints { got: usize, need: usize },
    /// All samples share a timestamp, so there is no trend to speak of.
    ZeroSpan,
    /// Log-space fitting requires strictly positive values.
    NonPositiveValues { count: usize },
}

impl std::fmt::Display for FitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FitError::TooFewPoints { got, need } => {
                write!(f, "too few usable points: {got} < {need}")
            }
            FitError::ZeroSpan => write!(f, "all samples share one timestamp"),
            FitError::NonPositiveValues { count } => {
                write!(f, "{count} non-positive values cannot be fitted in log space")
            }
        }
    }
}

impl std::error::Error for FitError {}

/// Minimum points before a fit is attempted at all. Two points always fit a line
/// perfectly and tell you nothing.
pub const MIN_FIT_POINTS: usize = 4;

/// Variance floor, so a suspiciously perfect fit cannot claim infinite weight.
const MIN_VARIANCE: f64 = 1e-8;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecayFit {
    /// Decay rate per day. Positive means the metric is falling.
    pub lambda_per_day: f64,
    /// `ln A`: the fitted value at `t = 0`, where `t = 0` is the first sample.
    pub ln_intercept: f64,
    /// Coefficient of determination of the fitted line in log space, clamped to 0..1.
    ///
    /// Reported for humans, but deliberately *not* used to weight evidence: a flat
    /// series has no variance to explain and so scores near zero however cleanly it is
    /// measured. [`DecayFit::lambda_stderr`] is the honest measure of trust.
    pub r2: f64,
    /// Standard error of `lambda_per_day`. Small means the slope is well determined —
    /// including when the slope is zero.
    pub lambda_stderr: f64,
    pub n: usize,
    pub span_days: f64,
    pub first_ts_ms: u64,
    pub last_ts_ms: u64,
    pub method: FitMethod,
}

impl DecayFit {
    /// Days for the metric to halve. `None` when the metric is flat or rising.
    pub fn half_life_days(&self) -> Option<f64> {
        (self.lambda_per_day > 1e-9).then(|| std::f64::consts::LN_2 / self.lambda_per_day)
    }

    /// Days for the metric to double. `None` when the metric is flat or falling.
    pub fn doubling_time_days(&self) -> Option<f64> {
        (self.lambda_per_day < -1e-9).then(|| std::f64::consts::LN_2 / -self.lambda_per_day)
    }

    /// Fitted value `days` after the first sample.
    pub fn predict(&self, days: f64) -> f64 {
        (self.ln_intercept - self.lambda_per_day * days).exp()
    }

    /// Fitted value now, given the current wall clock.
    pub fn predict_at(&self, ts_ms: u64) -> f64 {
        self.predict(ts_ms.saturating_sub(self.first_ts_ms) as f64 / MS_PER_DAY)
    }

    /// Days from `ts_ms` until the metric falls to `fraction` of its value at `ts_ms`.
    ///
    /// `None` when the metric is not falling — an unbounded runway is a real answer and
    /// must not be confused with a short one.
    pub fn days_until_fraction(&self, fraction: f64) -> Option<f64> {
        if self.lambda_per_day <= 1e-9 || !(0.0..1.0).contains(&fraction) || fraction <= 0.0 {
            return None;
        }
        Some(-fraction.ln() / self.lambda_per_day)
    }

    /// 95% interval for the decay rate.
    pub fn lambda_ci95(&self) -> (f64, f64) {
        let d = 1.96 * self.lambda_stderr;
        (self.lambda_per_day - d, self.lambda_per_day + d)
    }

    /// 95% interval for the half-life, in days. Ordered low-to-high; `None` when the
    /// interval includes "not decaying at all", because an unbounded upper end is not a
    /// number anyone should act on.
    pub fn half_life_ci95(&self) -> Option<(f64, f64)> {
        let (lo, hi) = self.lambda_ci95();
        (lo > 1e-9).then(|| {
            (
                std::f64::consts::LN_2 / hi,
                std::f64::consts::LN_2 / lo,
            )
        })
    }

    /// How many standard errors the slope sits from zero.
    pub fn significance(&self) -> f64 {
        if self.lambda_stderr <= f64::EPSILON {
            return if self.lambda_per_day.abs() > 0.0 { f64::INFINITY } else { 0.0 };
        }
        self.lambda_per_day.abs() / self.lambda_stderr
    }

    /// Inverse-variance weight for combining this fit with others. This is the
    /// statistically optimal weighting for a weighted mean, and unlike r² it correctly
    /// rewards a precisely-measured flat trend.
    pub fn precision(&self) -> f64 {
        1.0 / (self.lambda_stderr * self.lambda_stderr + MIN_VARIANCE)
    }

    pub fn confidence(&self) -> Confidence {
        Confidence {
            r2: self.r2,
            samples: self.n,
            span_days: self.span_days,
        }
    }
}

/// One `(timestamp, value)` sample.
#[derive(Debug, Clone, Copy)]
pub struct Point {
    pub ts_ms: u64,
    pub value: f64,
}

impl Point {
    pub fn new(ts_ms: u64, value: f64) -> Self {
        Self { ts_ms, value }
    }
}

/// Fit `y = A·e^(-λt)` to `points`.
pub fn fit(points: &[Point], method: FitMethod) -> Result<DecayFit, FitError> {
    // `is_finite` first: it is what rejects NaN, which would slip past `<= 0.0`.
    let bad = points
        .iter()
        .filter(|p| !p.value.is_finite() || p.value <= 0.0)
        .count();
    if bad > 0 {
        return Err(FitError::NonPositiveValues { count: bad });
    }
    if points.len() < MIN_FIT_POINTS {
        return Err(FitError::TooFewPoints {
            got: points.len(),
            need: MIN_FIT_POINTS,
        });
    }

    let mut pts: Vec<Point> = points.to_vec();
    pts.sort_by_key(|p| p.ts_ms);
    let first_ts_ms = pts[0].ts_ms;
    let last_ts_ms = pts[pts.len() - 1].ts_ms;
    let span_days = last_ts_ms.saturating_sub(first_ts_ms) as f64 / MS_PER_DAY;
    if span_days <= 0.0 {
        return Err(FitError::ZeroSpan);
    }

    // Log space: ln y = ln A - λ t, so the fitted slope is -λ.
    let xs: Vec<f64> = pts
        .iter()
        .map(|p| p.ts_ms.saturating_sub(first_ts_ms) as f64 / MS_PER_DAY)
        .collect();
    let ys: Vec<f64> = pts.iter().map(|p| p.value.ln()).collect();

    let (slope, intercept) = match method {
        FitMethod::TheilSen => theil_sen(&xs, &ys),
        FitMethod::OrdinaryLeastSquares => ols(&xs, &ys),
    };

    Ok(DecayFit {
        lambda_per_day: -slope,
        ln_intercept: intercept,
        r2: r_squared(&xs, &ys, slope, intercept),
        lambda_stderr: slope_stderr(&xs, &ys, slope, intercept),
        n: pts.len(),
        span_days,
        first_ts_ms,
        last_ts_ms,
        method,
    })
}

fn ols(xs: &[f64], ys: &[f64]) -> (f64, f64) {
    let n = xs.len() as f64;
    let mx = xs.iter().sum::<f64>() / n;
    let my = ys.iter().sum::<f64>() / n;
    let mut num = 0.0;
    let mut den = 0.0;
    for (x, y) in xs.iter().zip(ys) {
        num += (x - mx) * (y - my);
        den += (x - mx) * (x - mx);
    }
    let slope = if den.abs() < f64::EPSILON { 0.0 } else { num / den };
    (slope, my - slope * mx)
}

fn theil_sen(xs: &[f64], ys: &[f64]) -> (f64, f64) {
    let mut slopes = Vec::with_capacity(xs.len() * (xs.len() - 1) / 2);
    for i in 0..xs.len() {
        for j in (i + 1)..xs.len() {
            let dx = xs[j] - xs[i];
            if dx.abs() > 1e-12 {
                slopes.push((ys[j] - ys[i]) / dx);
            }
        }
    }
    if slopes.is_empty() {
        return ols(xs, ys);
    }
    let slope = median(&mut slopes);
    // Median residual gives the matching robust intercept.
    let mut residuals: Vec<f64> = xs.iter().zip(ys).map(|(x, y)| y - slope * x).collect();
    (slope, median(&mut residuals))
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// Standard error of the fitted slope.
///
/// `se = s / sqrt(Sxx)` where `s` is the residual standard deviation. Sign is
/// irrelevant, so this serves for `lambda` as well as for the raw slope.
fn slope_stderr(xs: &[f64], ys: &[f64], slope: f64, intercept: f64) -> f64 {
    let n = xs.len();
    if n <= 2 {
        return f64::INFINITY;
    }
    let ss_res: f64 = xs
        .iter()
        .zip(ys)
        .map(|(x, y)| {
            let e = y - (slope * x + intercept);
            e * e
        })
        .sum();
    let mx = xs.iter().sum::<f64>() / n as f64;
    let sxx: f64 = xs.iter().map(|x| (x - mx) * (x - mx)).sum();
    if sxx <= f64::EPSILON {
        return f64::INFINITY;
    }
    (ss_res / (n as f64 - 2.0) / sxx).sqrt()
}

/// R² of an arbitrary line against the data, clamped to 0..1.
///
/// Clamping matters: a robust line can fit worse than the mean on a contrived series,
/// which yields a negative R². Reporting that as "confidence" would be nonsense, so it
/// floors at zero — no confidence, rather than negative confidence.
fn r_squared(xs: &[f64], ys: &[f64], slope: f64, intercept: f64) -> f64 {
    let n = ys.len() as f64;
    let my = ys.iter().sum::<f64>() / n;
    let ss_tot: f64 = ys.iter().map(|y| (y - my) * (y - my)).sum();
    let ss_res: f64 = xs
        .iter()
        .zip(ys)
        .map(|(x, y)| {
            let e = y - (slope * x + intercept);
            e * e
        })
        .sum();
    if ss_tot < 1e-12 {
        // A perfectly flat series is perfectly explained by a flat line.
        return if ss_res < 1e-12 { 1.0 } else { 0.0 };
    }
    (1.0 - ss_res / ss_tot).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(half_life_days: f64, a: f64, n: usize, step_days: f64) -> Vec<Point> {
        let lambda = std::f64::consts::LN_2 / half_life_days;
        (0..n)
            .map(|i| {
                let t = i as f64 * step_days;
                Point::new(
                    (t * MS_PER_DAY) as u64,
                    a * (-lambda * t).exp(),
                )
            })
            .collect()
    }

    #[test]
    fn recovers_a_known_half_life() {
        for &hl in &[0.5, 3.0, 14.0] {
            let f = fit(&series(hl, 1000.0, 12, hl / 4.0), FitMethod::TheilSen).unwrap();
            let got = f.half_life_days().unwrap();
            assert!(
                (got - hl).abs() / hl < 0.02,
                "half-life {hl} recovered as {got}"
            );
            assert!(f.r2 > 0.99);
        }
    }

    #[test]
    fn ols_and_theil_sen_agree_on_clean_data() {
        let pts = series(5.0, 100.0, 10, 1.0);
        let a = fit(&pts, FitMethod::TheilSen).unwrap();
        let b = fit(&pts, FitMethod::OrdinaryLeastSquares).unwrap();
        assert!((a.lambda_per_day - b.lambda_per_day).abs() < 1e-6);
    }

    #[test]
    fn theil_sen_survives_an_outlier_that_wrecks_ols() {
        let mut pts = series(5.0, 100.0, 12, 1.0);
        // One mis-scraped reward, three orders of magnitude out.
        pts[6].value = 100_000.0;
        let truth = std::f64::consts::LN_2 / 5.0;
        let robust = fit(&pts, FitMethod::TheilSen).unwrap();
        let fragile = fit(&pts, FitMethod::OrdinaryLeastSquares).unwrap();
        let robust_err = (robust.lambda_per_day - truth).abs();
        let fragile_err = (fragile.lambda_per_day - truth).abs();
        assert!(
            robust_err < fragile_err / 4.0,
            "robust err {robust_err} vs ols err {fragile_err}"
        );
        assert!(robust_err / truth < 0.1);
    }

    #[test]
    fn rising_metric_reports_doubling_not_half_life() {
        let pts: Vec<Point> = (0..10)
            .map(|i| Point::new((i as f64 * MS_PER_DAY) as u64, 2.0 * 1.1_f64.powi(i)))
            .collect();
        let f = fit(&pts, FitMethod::TheilSen).unwrap();
        assert!(f.half_life_days().is_none());
        assert!(f.doubling_time_days().unwrap() > 0.0);
        assert!(f.days_until_fraction(0.5).is_none(), "a rising metric has no runway to a floor");
    }

    #[test]
    fn refuses_impossible_inputs() {
        assert!(matches!(
            fit(&[Point::new(0, 1.0), Point::new(1, 1.0)], FitMethod::TheilSen),
            Err(FitError::TooFewPoints { .. })
        ));
        let flat: Vec<Point> = (0..6).map(|_| Point::new(5, 1.0)).collect();
        assert!(matches!(fit(&flat, FitMethod::TheilSen), Err(FitError::ZeroSpan)));
        let neg: Vec<Point> = (0..6)
            .map(|i| Point::new(i * 1000, if i == 3 { 0.0 } else { 1.0 }))
            .collect();
        assert!(matches!(
            fit(&neg, FitMethod::TheilSen),
            Err(FitError::NonPositiveValues { count: 1 })
        ));
    }

    #[test]
    fn runway_matches_the_half_life() {
        let f = fit(&series(4.0, 500.0, 10, 1.0), FitMethod::TheilSen).unwrap();
        let to_half = f.days_until_fraction(0.5).unwrap();
        assert!((to_half - 4.0).abs() < 0.1, "runway to 50% was {to_half}");
        let to_quarter = f.days_until_fraction(0.25).unwrap();
        assert!((to_quarter - 8.0).abs() < 0.2);
    }

    #[test]
    fn noise_lowers_confidence() {
        let clean = fit(&series(5.0, 100.0, 12, 1.0), FitMethod::TheilSen).unwrap();
        let mut noisy = series(5.0, 100.0, 12, 1.0);
        let mut rng = hl_core::Rng::new(9);
        for p in noisy.iter_mut() {
            p.value *= 0.2 + 1.6 * rng.unit();
        }
        let noisy = fit(&noisy, FitMethod::TheilSen).unwrap();
        assert!(clean.r2 > noisy.r2, "noise must reduce reported confidence");
    }
}

/// Collapse points into per-bucket medians.
///
/// Raw observations arrive unevenly — a busy Tuesday can contribute fifty samples and
/// the following week none. Fitting that directly lets one busy day set the trend, so
/// metrics with many samples per period are bucketed first.
pub fn bucket_median(points: &[Point], bucket_ms: u64) -> Vec<Point> {
    if bucket_ms == 0 || points.is_empty() {
        return points.to_vec();
    }
    let mut by_bucket: std::collections::BTreeMap<u64, Vec<f64>> = Default::default();
    for p in points {
        by_bucket.entry(p.ts_ms / bucket_ms).or_default().push(p.value);
    }
    by_bucket
        .into_iter()
        .map(|(bucket, mut vals)| Point {
            // Anchor at the bucket midpoint rather than its edge, so the fitted line is
            // not biased by where in the period the samples happened to land.
            ts_ms: bucket * bucket_ms + bucket_ms / 2,
            value: median(&mut vals),
        })
        .collect()
}

#[cfg(test)]
mod stderr_tests {
    use super::*;

    fn line(n: usize, hl: f64, noise: f64, seed: u64) -> Vec<Point> {
        let lambda = std::f64::consts::LN_2 / hl;
        let mut rng = hl_core::Rng::new(seed);
        (0..n)
            .map(|i| {
                let t = i as f64;
                let jitter = 1.0 + noise * (rng.unit() - 0.5);
                Point::new((t * MS_PER_DAY) as u64, 1000.0 * (-lambda * t).exp() * jitter)
            })
            .collect()
    }

    #[test]
    fn a_clean_fit_is_more_precise_than_a_noisy_one() {
        let clean = fit(&line(15, 5.0, 0.02, 1), FitMethod::TheilSen).unwrap();
        let noisy = fit(&line(15, 5.0, 1.2, 1), FitMethod::TheilSen).unwrap();
        assert!(clean.lambda_stderr < noisy.lambda_stderr);
        assert!(clean.precision() > noisy.precision());
    }

    #[test]
    fn a_precisely_flat_series_is_high_precision_and_insignificant() {
        // The case that r²-weighting got wrong: stable metric, tiny jitter.
        let pts: Vec<Point> = (0..15)
            .map(|i| Point::new((i as f64 * MS_PER_DAY) as u64, 100.0 + (i % 2) as f64 * 0.05))
            .collect();
        let f = fit(&pts, FitMethod::TheilSen).unwrap();
        assert!(f.r2 < 0.2, "r2 is near zero for a flat series: {}", f.r2);
        assert!(
            f.lambda_stderr < 0.01,
            "yet the slope is tightly determined: se {}",
            f.lambda_stderr
        );
        assert!(f.significance() < 2.0, "and indistinguishable from zero");
    }

    #[test]
    fn confidence_interval_brackets_the_truth() {
        let hl = 6.0;
        let f = fit(&line(20, hl, 0.3, 7), FitMethod::TheilSen).unwrap();
        let (lo, hi) = f.half_life_ci95().expect("a decaying series has a bounded interval");
        assert!(lo <= hl && hl <= hi, "CI [{lo:.2}, {hi:.2}] missed {hl}");
    }

    #[test]
    fn a_flat_series_has_no_half_life_interval() {
        let pts: Vec<Point> = (0..12)
            .map(|i| Point::new((i as f64 * MS_PER_DAY) as u64, 50.0))
            .collect();
        let f = fit(&pts, FitMethod::TheilSen).unwrap();
        assert_eq!(f.half_life_ci95(), None);
    }
}
