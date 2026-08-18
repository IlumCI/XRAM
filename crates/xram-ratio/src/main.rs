//! Measures what XRAM's compressed tier would actually achieve here.
//!
//! The plan's capacity arithmetic assumes roughly 2.5x on general anonymous
//! memory. That figure decides whether a 12 GiB card carries 12 GiB or 30 GiB
//! of pages, so it should be measured on real data rather than quoted. This
//! tool compresses actual resident pages - from a live process, or from a file -
//! and reports the ratio, the throughput, and how much is lost to slab slack.

mod sample;

use std::io::Read;
use std::time::Instant;

use clap::Parser;
use sample::{anon_regions, MemReader};
use xram_codec::CompressedPool;

const PAGE: usize = 4096;

#[derive(Parser, Debug)]
#[command(
    name = "xram-ratio",
    about = "Measure XRAM's compression tier on real data"
)]
struct Args {
    /// Sample anonymous memory from this live process.
    #[arg(long, conflicts_with = "file")]
    pid: Option<u32>,
    /// Compress a file's contents instead of process memory.
    #[arg(long, conflicts_with = "pid")]
    file: Option<std::path::PathBuf>,
    /// Stop after this many pages.
    #[arg(long, default_value_t = 262_144)]
    max_pages: usize,
    /// Break the store path down into its stages, to show what actually costs.
    #[arg(long)]
    breakdown: bool,
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xram-ratio: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> std::io::Result<()> {
    let pages = match (&args.pid, &args.file) {
        (Some(pid), _) => from_process(*pid, args.max_pages)?,
        (_, Some(path)) => from_file(path, args.max_pages)?,
        _ => {
            // Default to this process, which at least exercises the path. It is
            // a poor sample - a tiny CLI's heap is not representative - so say so.
            eprintln!("note: no --pid or --file given, sampling this process (unrepresentative)");
            from_process(std::process::id(), args.max_pages)?
        }
    };

    if pages.is_empty() {
        eprintln!("no resident pages sampled; nothing to measure");
        return Ok(());
    }

    let mut pool = CompressedPool::new(PAGE);
    let mut handles = Vec::with_capacity(pages.len() / PAGE);

    let t = Instant::now();
    for page in pages.chunks_exact(PAGE) {
        handles.push(pool.store(page).expect("page size matches pool"));
    }
    let compress_s = t.elapsed().as_secs_f64();

    // Verify every page survives, and time the read-back path while we are here.
    let mut out = vec![0u8; PAGE];
    let t = Instant::now();
    for (i, h) in handles.iter().enumerate() {
        pool.load(*h, &mut out).expect("load");
        let orig = &pages[i * PAGE..(i + 1) * PAGE];
        assert_eq!(out, orig, "page {i} did not survive the round trip");
    }
    let decompress_s = t.elapsed().as_secs_f64();

    if args.breakdown {
        breakdown(&pages);
    }

    let s = pool.stats();
    let logical = s.logical_bytes as f64;
    let mib = |b: f64| b / (1024.0 * 1024.0);

    println!("pages sampled        : {}", s.pages);
    println!(
        "  same-fill          : {:>8}  ({:.1}%)  stored in the handle, zero cost",
        s.same_fill_pages,
        100.0 * s.same_fill_pages as f64 / s.pages as f64
    );
    println!(
        "  incompressible     : {:>8}  ({:.1}%)  stored raw rather than inflated",
        s.incompressible_pages,
        100.0 * s.incompressible_pages as f64 / s.pages as f64
    );
    println!("logical             : {:>9.1} MiB", mib(logical));
    println!(
        "physical (with slack): {:>8.1} MiB",
        mib(s.physical_bytes as f64)
    );
    println!(
        "payload only         : {:>8.1} MiB",
        mib(s.payload_bytes as f64)
    );
    println!();
    // Two ratios, because they answer different questions. The slab-inclusive
    // one is what the tier actually delivers right now; on a small sample it is
    // dominated by half-empty chunks and reads far too low. The payload ratio is
    // what a populated pool converges to, and is the right number for capacity
    // planning.
    let steady = if s.payload_bytes == 0 {
        f64::INFINITY
    } else {
        logical / s.payload_bytes as f64
    };
    println!(
        "compression ratio    : {:>8.2}x   payload only, the steady-state figure",
        steady
    );
    println!(
        "  as measured now    : {:>8.2}x   including slab slack on this sample",
        s.ratio()
    );
    println!(
        "slab waste           : {:>8.1}%   (falls as the pool fills)",
        100.0 * s.waste()
    );
    println!(
        "compress throughput  : {:>8.2} GiB/s",
        logical / compress_s / (1024.0 * 1024.0 * 1024.0)
    );
    println!(
        "decompress throughput: {:>8.2} GiB/s",
        logical / decompress_s / (1024.0 * 1024.0 * 1024.0)
    );

    println!();
    println!("What this means for a VRAM tier, at the steady-state ratio:");
    for gib in [8u64, 12, 16, 24] {
        println!(
            "  {gib:>2} GiB card holds ~{:>5.1} GiB of pages",
            gib as f64 * steady
        );
    }
    if s.pages < 8192 {
        println!(
            "\nSample is only {} pages. Slab slack dominates at this size; take the\n\
             steady-state ratio, not the measured one, and re-run against a larger process.",
            s.pages
        );
    }
    if steady < 1.5 {
        println!(
            "\nRatio is low. Check whether this sample is dominated by already-compressed\n\
             or encrypted data; the plan's capacity numbers assume general anonymous memory."
        );
    }
    Ok(())
}

/// Splits the store path into its stages.
///
/// Compression throughput bounds how fast pages can be demoted into the
/// compressed tier, so it matters where the time goes: LZ4 on 4 KiB blocks is
/// inherently limited (no cross-page history, per-call overhead), whereas slab
/// bookkeeping would be ours to fix.
fn breakdown(pages: &[u8]) {
    let n = pages.len() / PAGE;
    let bytes = pages.len() as f64;
    let gibs = |secs: f64| bytes / secs / (1024.0 * 1024.0 * 1024.0);

    let t = Instant::now();
    let mut same = 0usize;
    for page in pages.chunks_exact(PAGE) {
        let f = page[0];
        if page.iter().all(|&b| b == f) {
            same += 1;
        }
    }
    let scan_s = t.elapsed().as_secs_f64();

    let mut scratch = vec![0u8; lz4_flex::block::get_maximum_output_size(PAGE)];
    let t = Instant::now();
    let mut total = 0usize;
    for page in pages.chunks_exact(PAGE) {
        total += lz4_flex::block::compress_into(page, &mut scratch).unwrap_or(PAGE);
    }
    let lz4_s = t.elapsed().as_secs_f64();

    let mut pool = CompressedPool::new(PAGE);
    let t = Instant::now();
    let mut handles = Vec::with_capacity(n);
    for page in pages.chunks_exact(PAGE) {
        handles.push(pool.store(page).expect("store"));
    }
    let pool_s = t.elapsed().as_secs_f64();

    // Repeat into a pool whose chunks are already allocated and whose free
    // lists are already threaded, to separate one-time growth from steady state.
    for h in handles {
        pool.free(h).expect("free");
    }
    let t = Instant::now();
    for page in pages.chunks_exact(PAGE) {
        pool.store(page).expect("store");
    }
    let warm_s = t.elapsed().as_secs_f64();

    println!("stage breakdown over {n} pages");
    println!(
        "  same-fill scan     : {:>8.2} GiB/s  ({same} hit, short-circuits the rest)",
        gibs(scan_s)
    );
    println!(
        "  raw LZ4 only       : {:>8.2} GiB/s  ({:.2}x on payload)",
        gibs(lz4_s),
        bytes / total as f64
    );
    println!(
        "  pool store, cold   : {:>8.2} GiB/s  growing the pool: every new chunk is\n\
         {:22}first-touch faulted, a one-time cost per byte of tier",
        gibs(pool_s),
        ""
    );
    println!(
        "  pool store, warm   : {:>8.2} GiB/s  steady state, {:.0}% over raw LZ4",
        gibs(warm_s),
        100.0 * (warm_s - lz4_s).max(0.0) / lz4_s
    );
    println!();
}

fn from_process(pid: u32, max_pages: usize) -> std::io::Result<Vec<u8>> {
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps"))?;
    let regions = anon_regions(&maps);
    let reader = MemReader::open(pid)?;

    let mut out = Vec::with_capacity(max_pages.min(1 << 16) * PAGE);
    let mut page = vec![0u8; PAGE];
    let mut skipped = 0usize;

    for r in &regions {
        let mut addr = r.start;
        while addr + PAGE as u64 <= r.end && out.len() / PAGE < max_pages {
            match reader.read_page(addr, &mut page) {
                Ok(true) => out.extend_from_slice(&page),
                Ok(false) => skipped += 1,
                // A mapping can vanish under us in a live process; that is
                // expected, not a reason to abandon the sample.
                Err(_) => skipped += 1,
            }
            addr += PAGE as u64;
        }
        if out.len() / PAGE >= max_pages {
            break;
        }
    }
    let mapped: u64 = regions.iter().map(|r| r.len()).sum();
    eprintln!(
        "pid {pid}: {} anonymous regions spanning {:.1} MiB mapped; \
         sampled {} pages, {skipped} not resident",
        regions.len(),
        mapped as f64 / (1024.0 * 1024.0),
        out.len() / PAGE,
    );
    Ok(out)
}

fn from_file(path: &std::path::Path, max_pages: usize) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    f.by_ref()
        .take((max_pages * PAGE) as u64)
        .read_to_end(&mut buf)?;
    buf.truncate(buf.len() - buf.len() % PAGE);
    eprintln!("read {} pages from {}", buf.len() / PAGE, path.display());
    Ok(buf)
}
