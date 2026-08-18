//! Paper trading, and the only falsifiable test this project has.
//!
//! Everything else here measures. This asks the question that matters: if you had
//! followed the signal, would you have been better off than ignoring it? It runs on
//! stored observations, costs nothing, risks nothing, and is perfectly capable of
//! returning "no".

pub mod backtest;
pub mod portfolio;

pub use backtest::{is_paper_eligible, Backtest, BacktestResult, Outcome};
pub use portfolio::{PaperConfig, Portfolio, Position, RateSeries};
