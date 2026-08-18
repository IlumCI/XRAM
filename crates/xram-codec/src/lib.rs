//! Compressed-page storage for XRAM's memory tiers.
//!
//! Compression is what makes the VRAM tier worth having: a 12 GiB card holds
//! only 12 GiB of pages uncompressed, which is a modest addition to a 16 GiB
//! machine. At the ~2.5x that general anonymous memory typically reaches, the
//! same card carries closer to 30 GiB - and because pages are compressed before
//! they cross PCIe, the same change also cuts transfer bytes on the path it
//! accelerates.
//!
//! Two cases are handled specially, both because they are common and because
//! they are nearly free:
//!
//! - **Same-fill pages** (all-zero above all) are recorded in the handle itself
//!   and occupy no storage at all.
//! - **Incompressible pages** are stored raw rather than inflated. Anything
//!   already compressed or encrypted - and notably quantised model weights,
//!   which sit near maximum entropy - lands here, so the tier degrades to
//!   plain capacity rather than wasting CPU.

mod handle;
mod pool;

pub use handle::{Handle, MAX_CLASSES, MAX_SLOT};
pub use pool::{CompressedPool, PoolError, Stats};
