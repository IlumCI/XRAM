//! Sandboxed command execution.
//!
//! Kept from the previous design (see `docs/KILL-LIST.md`): several niche classes
//! require running code we did not write, and doing that safely and cheaply is a
//! prerequisite rather than a strategy.

pub mod sandbox;

pub use sandbox::{LocalSandbox, RunOutput, RunSpec, Sandbox, SandboxCaps};
