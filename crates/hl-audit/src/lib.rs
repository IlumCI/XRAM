//! A review aid for a human security auditor.
//!
//! It fetches verified (already-public) contract source and maps the attack surface,
//! flagging well-known dangerous patterns so a person's review time goes to the right
//! lines first. It is not a bug finder, does no dataflow, and is blind to logic errors
//! by construction. A clean report means "nothing obvious in the known-footgun set", not
//! "safe". The judgment is the human's; this only sorts the reading order.

pub mod source;
pub mod surface;

pub use source::{fetch_sources, supported_chains, ContractSources, SourceFile};
pub use surface::{analyze, Flag, Severity, SurfaceMap};
