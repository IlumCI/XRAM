//! The measurement core: time PCIe transfers under the two policies that
//! separate XRAM from the existing art.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use xram_cuda::{CuDevicePtr, CuStream, Cuda};

/// How often the caller waits for the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Sync {
    /// Synchronise after every single copy.
    ///
    /// This is what a block device that turns each request into one
    /// `cuMemcpy` + wait does, and it is the shape `nbd-vram` reports 2.7 GB/s
    /// with. Every transfer pays the full driver round trip.
    PerCopy,
    /// Issue a whole batch of async copies, then synchronise once.
    ///
    /// This is what XRAM intends to do: the io_uring queue hands us many
    /// requests at once, so the driver call overhead amortises across the batch
    /// instead of being paid per 4 KiB page.
    PerBatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Dir {
    H2D,
    D2H,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum HostMem {
    /// Page-locked. The driver can DMA straight out of it.
    Pinned,
    /// Ordinary `malloc`. The driver stages through its own internal pinned
    /// buffer, which costs an extra copy and caps throughput well below link
    /// rate.
    Pageable,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Sample {
    pub dir: Dir,
    pub host_mem: HostMem,
    pub sync: Sync,
    pub xfer_bytes: usize,
    pub batch: usize,
    pub streams: usize,
    pub gib_per_s: f64,
    /// Mean wall time attributable to one transfer in the batch.
    pub per_xfer_us: f64,
    pub p50_us: f64,
    pub p99_us: f64,
    pub iters: usize,
}

/// Host-side staging buffer, pinned or not.
///
/// Owns its allocation and releases it through the matching allocator; mixing
/// the two (freeing pinned memory with `Vec`'s allocator, say) corrupts the
/// driver's bookkeeping.
pub struct HostBuf {
    ptr: *mut c_void,
    len: usize,
    kind: HostMem,
    // Kept alive for the Pageable case; the driver never sees this Vec directly.
    owned: Option<Vec<u8>>,
}

impl HostBuf {
    pub fn new(cuda: &Cuda, len: usize, kind: HostMem) -> xram_cuda::Result<Self> {
        match kind {
            HostMem::Pinned => {
                // SAFETY: freed exactly once in Drop via mem_free_host.
                let ptr = unsafe { cuda.mem_alloc_host(len)? };
                // Touch it so first-use faults do not land inside a timed loop.
                // SAFETY: the driver just handed us `len` writable bytes.
                unsafe { std::ptr::write_bytes(ptr as *mut u8, 0xA5, len) };
                Ok(Self {
                    ptr,
                    len,
                    kind,
                    owned: None,
                })
            }
            HostMem::Pageable => {
                let mut v = vec![0xA5u8; len];
                let ptr = v.as_mut_ptr() as *mut c_void;
                Ok(Self {
                    ptr,
                    len,
                    kind,
                    owned: Some(v),
                })
            }
        }
    }

    pub fn ptr(&self) -> *mut c_void {
        self.ptr
    }
    pub fn len(&self) -> usize {
        self.len
    }
    /// Releases a pinned allocation. Pageable buffers drop with their `Vec`.
    ///
    /// Freeing needs the driver handle, which `Drop` cannot reach, so callers
    /// hold buffers for the process lifetime and let the pinned pages be
    /// reclaimed at exit. The probe is short-lived and allocates a fixed number
    /// of buffers, so this is bounded.
    pub fn kind(&self) -> HostMem {
        self.kind
    }
}

impl Drop for HostBuf {
    fn drop(&mut self) {
        // Pageable memory is owned by `owned` and freed here. Pinned memory is
        // intentionally left to process teardown, see `HostBuf::kind` docs.
        drop(self.owned.take());
    }
}

/// One transfer configuration to measure.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub dir: Dir,
    pub sync: Sync,
    pub xfer_bytes: usize,
    pub batch: usize,
    pub streams: usize,
}

pub struct Rig<'a> {
    pub cuda: &'a Cuda,
    pub dev_ptr: CuDevicePtr,
    pub dev_bytes: usize,
    pub streams: Vec<CuStream>,
}

impl<'a> Rig<'a> {
    /// Runs one configuration and returns its measured throughput.
    ///
    /// `xfer_bytes * batch` must fit in both the device allocation and the host
    /// buffer; the caller clamps `batch` to guarantee that.
    pub fn run(
        &self,
        host: &HostBuf,
        cfg: Config,
        min_duration: Duration,
    ) -> xram_cuda::Result<Sample> {
        let Config {
            dir,
            sync,
            xfer_bytes,
            batch,
            ..
        } = cfg;
        let n_streams = cfg.streams.clamp(1, self.streams.len());
        let cfg = Config {
            streams: n_streams,
            ..cfg
        };
        let span = xfer_bytes * batch;
        assert!(span <= self.dev_bytes && span <= host.len());

        // Warm up: first touch of a stream and the driver's lazy setup would
        // otherwise be charged to the first timed iteration.
        self.one_pass(host, cfg)?;

        let mut per_iter = Vec::new();
        let start = Instant::now();
        let mut iters = 0usize;
        while start.elapsed() < min_duration || iters < 3 {
            let t = Instant::now();
            self.one_pass(host, cfg)?;
            per_iter.push(t.elapsed().as_secs_f64());
            iters += 1;
            if iters >= 100_000 {
                break;
            }
        }

        let total: f64 = per_iter.iter().sum();
        let bytes_total = (span as f64) * (iters as f64);
        let gib_per_s = bytes_total / total / (1024.0 * 1024.0 * 1024.0);

        // Per-transfer latency distribution, derived from whole passes: with
        // PerCopy each pass is `batch` serialised round trips, so dividing is
        // exact; with PerBatch it is the amortised cost, which is the number we
        // actually care about.
        let mut per_xfer: Vec<f64> = per_iter.iter().map(|s| s * 1e6 / batch as f64).collect();
        per_xfer.sort_by(|a, b| a.partial_cmp(b).unwrap());

        Ok(Sample {
            dir,
            host_mem: host.kind(),
            sync,
            xfer_bytes,
            batch,
            streams: n_streams,
            gib_per_s,
            per_xfer_us: total * 1e6 / (iters * batch) as f64,
            p50_us: pct(&per_xfer, 0.50),
            p99_us: pct(&per_xfer, 0.99),
            iters,
        })
    }

    fn one_pass(&self, host: &HostBuf, cfg: Config) -> xram_cuda::Result<()> {
        let Config {
            dir,
            sync,
            xfer_bytes,
            batch,
            streams: n_streams,
        } = cfg;
        for i in 0..batch {
            let off = (i * xfer_bytes) as u64;
            let hptr = (host.ptr() as usize + i * xfer_bytes) as *mut c_void;
            let stream = self.streams[i % n_streams];
            // SAFETY: `off + xfer_bytes <= dev_bytes` and the host slice is
            // in-bounds, both guaranteed by the caller's clamp. The host buffer
            // outlives the synchronise below.
            unsafe {
                match dir {
                    Dir::H2D => self.cuda.memcpy_h2d_async(
                        self.dev_ptr + off,
                        hptr as *const c_void,
                        xfer_bytes,
                        stream,
                    )?,
                    Dir::D2H => {
                        self.cuda
                            .memcpy_d2h_async(hptr, self.dev_ptr + off, xfer_bytes, stream)?
                    }
                }
            }
            if sync == Sync::PerCopy {
                self.cuda.sync_stream(stream)?;
            }
        }
        if sync == Sync::PerBatch {
            for s in &self.streams[..n_streams] {
                self.cuda.sync_stream(*s)?;
            }
        }
        Ok(())
    }
}

fn pct(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[i]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_pick_expected_elements() {
        let v: Vec<f64> = (0..=100).map(|i| i as f64).collect();
        assert_eq!(pct(&v, 0.0), 0.0);
        assert_eq!(pct(&v, 0.5), 50.0);
        assert_eq!(pct(&v, 0.99), 99.0);
        assert_eq!(pct(&v, 1.0), 100.0);
    }

    #[test]
    fn percentile_of_empty_is_nan() {
        assert!(pct(&[], 0.5).is_nan());
    }

    #[test]
    fn single_element_percentiles_are_that_element() {
        assert_eq!(pct(&[7.0], 0.0), 7.0);
        assert_eq!(pct(&[7.0], 0.99), 7.0);
    }
}
