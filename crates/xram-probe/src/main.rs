//! XRAM M0 probe - the go/no-go measurement.
//!
//! The XRAM plan rests on one empirical claim: that batching many transfers per
//! driver synchronise recovers most of the PCIe link, where issuing one
//! `cuMemcpy` per block request does not. `nbd-vram` takes the latter path and
//! reports 2.7 GB/s read - slower than the NVMe it is meant to replace, on a bus
//! rated 24-26 GB/s.
//!
//! This probe measures both shapes on the machine it runs on and prints the gap.
//! If batching does not clear the threshold here, XRAM should not be built.

mod bench;

use std::time::Duration;

use bench::{Config, Dir, HostBuf, HostMem, Rig, Sample, Sync};
use clap::Parser;
use xram_cuda::{cuda, Cuda, CudaError};

/// Transfer sizes a swap device realistically sees.
///
/// Linux swap issues 4 KiB pages, clustered by `vm.page-cluster` (default 3, so
/// 32 KiB). Anything at or below 256 KiB is the regime XRAM must win in;
/// the larger sizes are here to locate the link ceiling.
const SWAP_SIZES: &[usize] = &[4 << 10, 16 << 10, 32 << 10, 64 << 10, 256 << 10];
const BULK_SIZES: &[usize] = &[1 << 20, 4 << 20, 16 << 20];

/// Batched throughput at swap-realistic sizes must clear this to justify the
/// project: comfortably past `nbd-vram`'s 2.5 GiB/s and a fast NVMe's ~2.9 GiB/s.
const GO_THRESHOLD_GIBS: f64 = 8.0;
/// The plan's stretch target.
const STRONG_GIBS: f64 = 15.0;

#[derive(Parser, Debug)]
#[command(
    name = "xram-probe",
    about = "XRAM M0: measure what this PCIe link sustains"
)]
struct Args {
    /// Seconds to spend on each configuration.
    #[arg(long, default_value_t = 0.25)]
    dwell: f64,
    /// Largest span (bytes in flight per pass), in MiB.
    #[arg(long, default_value_t = 256)]
    span_mib: usize,
    /// CUDA streams to round-robin across when batching.
    #[arg(long, default_value_t = 4)]
    streams: usize,
    /// Write the full sample set as JSON here.
    #[arg(long)]
    json: Option<std::path::PathBuf>,
    /// Device ordinal.
    #[arg(long, default_value_t = 0)]
    device: i32,
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(go) => {
            if go {
                std::process::ExitCode::SUCCESS
            } else {
                std::process::ExitCode::from(2)
            }
        }
        Err(e) => {
            eprintln!("\nxram-probe: {e}");
            if matches!(e, CudaError::Unavailable(_)) {
                eprintln!(
                    "\nThis probe needs an NVIDIA GPU and the driver's libcuda.so.1.\n\
                     It is expected to fail in CI and containers - XRAM builds and unit-tests\n\
                     without a GPU, but every performance number must come from real hardware."
                );
            }
            std::process::ExitCode::from(1)
        }
    }
}

fn run(args: &Args) -> Result<bool, CudaError> {
    let cuda = cuda()?;
    let n = cuda.device_count()?;
    if n <= 0 {
        return Err(CudaError::NoDevice);
    }
    let dev = cuda.device(args.device)?;
    let name = cuda.device_name(dev)?;
    let ctx = cuda.create_context(dev, xram_cuda::CTX_SCHED_BLOCKING_SYNC)?;
    cuda.set_current(ctx)?;

    let (free, total) = cuda.mem_info()?;
    let link = pcie_link_of(cuda, dev);

    println!("XRAM M0 probe");
    println!("  device      : {name} (ordinal {})", args.device);
    println!(
        "  VRAM        : {:.1} GiB free of {:.1} GiB",
        gib(free),
        gib(total)
    );
    match &link {
        Some((speed, width)) => println!("  PCIe link   : {speed} x{width}"),
        None => println!("  PCIe link   : unknown (sysfs unreadable)"),
    }

    // Keep well inside free VRAM; the probe must never be the reason a GPU app
    // fails to start.
    let span = (args.span_mib << 20).min(free / 4).max(4 << 20);
    let dwell = Duration::from_secs_f64(args.dwell);

    // SAFETY: freed at the end of this function, once, under the same context.
    let dev_ptr = unsafe { cuda.mem_alloc(span)? };
    let streams: Vec<_> = (0..args.streams.max(1))
        .map(|_| cuda.create_stream(xram_cuda::STREAM_NON_BLOCKING))
        .collect::<Result<_, _>>()?;

    let pinned = HostBuf::new(cuda, span, HostMem::Pinned)?;
    let pageable = HostBuf::new(cuda, span, HostMem::Pageable)?;
    let rig = Rig {
        cuda,
        dev_ptr,
        dev_bytes: span,
        streams,
    };

    println!(
        "  span        : {:.0} MiB per pass, {:.0}s dwell\n",
        span as f64 / 1048576.0,
        args.dwell
    );

    let mut all: Vec<Sample> = Vec::new();

    // --- A/B: the shape that decides the project -------------------------
    println!("Per-request sync vs batched, pinned host memory, H2D");
    println!(
        "{:>9}  {:>7}  {:>12}  {:>12}  {:>8}  {:>10}",
        "xfer", "batch", "per-copy", "batched", "speedup", "batched p99"
    );
    for &size in SWAP_SIZES.iter().chain(BULK_SIZES) {
        let batch = (span / size).clamp(1, 512);
        let naive = rig.run(
            &pinned,
            Config {
                dir: Dir::H2D,
                sync: Sync::PerCopy,
                xfer_bytes: size,
                batch: batch.min(64),
                streams: 1,
            },
            dwell,
        )?;
        let fast = rig.run(
            &pinned,
            Config {
                dir: Dir::H2D,
                sync: Sync::PerBatch,
                xfer_bytes: size,
                batch,
                streams: args.streams,
            },
            dwell,
        )?;
        println!(
            "{:>9}  {:>7}  {:>9.2} GiB/s  {:>9.2} GiB/s  {:>7.1}x  {:>7.1} us",
            human(size),
            batch,
            naive.gib_per_s,
            fast.gib_per_s,
            fast.gib_per_s / naive.gib_per_s.max(1e-9),
            fast.p99_us,
        );
        all.push(naive);
        all.push(fast);
    }

    // --- C: direction symmetry -------------------------------------------
    println!("\nRead-back (D2H), batched, pinned - the swap-in path");
    println!(
        "{:>9}  {:>12}  {:>10}  {:>10}",
        "xfer", "batched", "p50", "p99"
    );
    for &size in SWAP_SIZES {
        let batch = (span / size).clamp(1, 512);
        let s = rig.run(
            &pinned,
            Config {
                dir: Dir::D2H,
                sync: Sync::PerBatch,
                xfer_bytes: size,
                batch,
                streams: args.streams,
            },
            dwell,
        )?;
        println!(
            "{:>9}  {:>9.2} GiB/s  {:>7.1} us  {:>7.1} us",
            human(size),
            s.gib_per_s,
            s.p50_us,
            s.p99_us
        );
        all.push(s);
    }

    // --- D: pinned vs pageable -------------------------------------------
    println!("\nPinned vs pageable host memory, batched H2D");
    println!(
        "{:>9}  {:>12}  {:>12}  {:>8}",
        "xfer", "pinned", "pageable", "ratio"
    );
    for &size in &[64 << 10, 1 << 20] {
        let batch = (span / size).clamp(1, 512);
        let cfg = Config {
            dir: Dir::H2D,
            sync: Sync::PerBatch,
            xfer_bytes: size,
            batch,
            streams: args.streams,
        };
        let p = rig.run(&pinned, cfg, dwell)?;
        let q = rig.run(&pageable, cfg, dwell)?;
        println!(
            "{:>9}  {:>9.2} GiB/s  {:>9.2} GiB/s  {:>7.2}x",
            human(size),
            p.gib_per_s,
            q.gib_per_s,
            p.gib_per_s / q.gib_per_s.max(1e-9)
        );
        all.push(p);
        all.push(q);
    }

    // --- verdict ----------------------------------------------------------
    let swap_best = best(&all, |s| {
        s.sync == Sync::PerBatch && s.host_mem == HostMem::Pinned && s.xfer_bytes <= 256 << 10
    });
    let ceiling = best(&all, |s| {
        s.sync == Sync::PerBatch && s.host_mem == HostMem::Pinned
    });
    let naive_best = best(&all, |s| s.sync == Sync::PerCopy);

    println!("\n{}", "-".repeat(64));
    println!("Best at swap-realistic sizes (<=256 KiB, batched) : {swap_best:6.2} GiB/s");
    println!("Link ceiling (any size, batched)                  : {ceiling:6.2} GiB/s");
    println!("Best with per-request sync (the nbd-vram shape)    : {naive_best:6.2} GiB/s");
    println!("Reference: nbd-vram 2.7 GB/s read, NVMe swap 3.1 GB/s\n");

    let go = swap_best >= GO_THRESHOLD_GIBS;
    if swap_best >= STRONG_GIBS {
        println!("VERDICT: STRONG GO - batching clears the {STRONG_GIBS:.0} GiB/s stretch target.");
    } else if go {
        println!("VERDICT: GO - clears the {GO_THRESHOLD_GIBS:.0} GiB/s bar, well past NVMe and nbd-vram.");
    } else {
        println!(
            "VERDICT: NO-GO - batched throughput at swap-realistic sizes is {swap_best:.2} GiB/s,\n\
             under the {GO_THRESHOLD_GIBS:.0} GiB/s bar. A VRAM swap tier would not beat this\n\
             machine's NVMe by enough to justify the deadlock risk. Stop here."
        );
    }

    if let Some(path) = &args.json {
        let doc = serde_json::json!({
            "device": name,
            "vram_free_bytes": free,
            "vram_total_bytes": total,
            "pcie_link": link,
            "span_bytes": span,
            "swap_realistic_best_gibs": swap_best,
            "ceiling_gibs": ceiling,
            "per_request_sync_best_gibs": naive_best,
            "verdict_go": go,
            "samples": all,
        });
        std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap())
            .unwrap_or_else(|e| eprintln!("could not write {}: {e}", path.display()));
        println!("\nwrote {}", path.display());
    }

    // SAFETY: nothing is in flight - every configuration synchronised before
    // returning - and none of these handles is used after this point.
    unsafe {
        for s in &rig.streams {
            cuda.destroy_stream(*s)?;
        }
        cuda.mem_free(dev_ptr)?;
        cuda.destroy_context(ctx)?;
    }
    Ok(go)
}

fn best(all: &[Sample], f: impl Fn(&Sample) -> bool) -> f64 {
    all.iter()
        .filter(|s| f(s))
        .map(|s| s.gib_per_s)
        .fold(0.0, f64::max)
}

fn pcie_link_of(cuda: &Cuda, dev: i32) -> Option<(String, String)> {
    let dom = cuda
        .device_attribute(xram_cuda::ATTR_PCI_DOMAIN_ID, dev)
        .ok()?;
    let bus = cuda
        .device_attribute(xram_cuda::ATTR_PCI_BUS_ID, dev)
        .ok()?;
    let slot = cuda
        .device_attribute(xram_cuda::ATTR_PCI_DEVICE_ID, dev)
        .ok()?;
    xram_cuda::pcie_link(&xram_cuda::bus_id(dom, bus, slot))
}

fn gib(b: usize) -> f64 {
    b as f64 / (1024.0 * 1024.0 * 1024.0)
}

fn human(b: usize) -> String {
    if b >= 1 << 20 {
        format!("{} MiB", b >> 20)
    } else {
        format!("{} KiB", b >> 10)
    }
}
