//! Core domain model for **Halflife**.
//!
//! The premise: when generation is free, every strategy expressible as software
//! saturates almost immediately. So this system does not try to own a niche. It
//! measures how fast each niche is closing and rotates out before the margin does.
//!
//! Two disciplines are enforced here rather than remembered:
//!
//! * [`governor::Governor`] — every metered call reserves quota first and persists its
//!   counters, so the system is structurally incapable of running up a bill.
//! * [`ledger::Ledger`] — an append-only hash chain of what we spent and what came
//!   back. A rotation strategy is only as good as its record of realised yield, and
//!   that record has to survive our own later optimism.

pub mod governor;
pub mod ledger;
pub mod prng;
pub mod store;
pub mod types;

pub use governor::{Governor, Permit, QuotaDenied, QuotaLimits};
pub use ledger::{Ledger, LedgerEvent, LedgerRecord, NicheYield};
pub use prng::Rng;
pub use store::{observation_key, ObservationStore};
pub use types::*;

use std::time::{SystemTime, UNIX_EPOCH};

pub const MS_PER_DAY: f64 = 86_400_000.0;

/// Milliseconds since the Unix epoch.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Lowercase hex of the SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}
