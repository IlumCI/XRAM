//! Observation sources.
//!
//! Each source is a candidate income stream. They exist to feed the crowding meter, and
//! they are all free to poll — a source that costs money to watch cannot be watched at
//! all under this system's constraints.

pub mod github;
pub mod http;
pub mod sim;
pub mod timefmt;

pub use github::{GithubNiche, GithubSource};
pub use http::{FixtureTransport, HttpResponse, Transport, UreqTransport};
pub use sim::{SimNiche, SimSource};
