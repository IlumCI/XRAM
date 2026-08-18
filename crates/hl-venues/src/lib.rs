//! Observation sources.
//!
//! Each source is a candidate income stream. They exist to feed the crowding meter, and
//! they are all free to poll — a source that costs money to watch cannot be watched at
//! all under this system's constraints.

pub mod contests;
pub mod defillama;
pub mod github;
pub mod github_search;
pub mod huggingface;
pub mod kaggle;
pub mod http;
pub mod hunt;
pub mod hyperliquid;
pub mod sim;
pub mod timefmt;

pub use contests::{Contest, ContestsSource};
pub use defillama::{DefiLlamaSource, Pool, PoolFilter};
pub use github::{GithubNiche, GithubSource};
pub use github_search::{GithubSearchNiche, GithubSearchSource};
pub use huggingface::{HfKind, HfNiche, HuggingFaceSource};
pub use kaggle::{Competition, KaggleSource};
pub use hunt::{hunt, Candidate, HuntFilter, RiskKind};
pub use http::{FixtureTransport, HttpResponse, Transport, UreqTransport};
pub use hyperliquid::{HyperliquidSource, Perp, PerpFilter};
pub use sim::{SimNiche, SimSource};
