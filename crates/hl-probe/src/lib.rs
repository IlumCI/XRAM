//! Crowding measurement and rotation policy.
//!
//! Three layers, deliberately separable:
//!
//! * [`fit`] — robust exponential trend fitting over dirty scraped series.
//! * [`crowding`] — turns observations into a runway, in days.
//! * [`policy`] — turns a runway into enter / hold / exit.

pub mod crowding;
pub mod fit;
pub mod policy;

pub use crowding::{CrowdingMeter, CrowdingReport, Metric, MetricFit};
pub use fit::{fit, DecayFit, FitError, FitMethod, Point};
pub use policy::{decide, rank, Decision, PolicyConfig};
