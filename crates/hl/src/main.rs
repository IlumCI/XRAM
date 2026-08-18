//! Halflife CLI.
//!
//! `hl demo` runs the whole loop against simulated niches whose true half-lives are
//! known, so the estimator can be checked against ground truth without touching a
//! network or spending a request.

mod render;

use anyhow::Result;
use clap::{Parser, Subcommand};
use hl_core::{now_millis, Governor, Ledger, LedgerEvent, QuotaLimits, Signal, Source};
use hl_probe::{crowding::CrowdingMeter, policy, PolicyConfig};
use hl_scout::{Scout, SightingIndex};
use hl_venues::{
    github::GithubNiche, http::UreqTransport, GithubSource, SimNiche, SimSource,
};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "hl",
    about = "Measure how fast a niche is closing, and leave before it does."
)]
struct Cli {
    /// Where state lives (ledger, quota counters, sighting index).
    #[arg(long, default_value = ".halflife")]
    state: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the full loop against simulated niches with known half-lives.
    Demo {
        /// Days of history to simulate.
        #[arg(long, default_value_t = 21.0)]
        days: f64,
    },
    /// Measure real niches from GitHub's public API. Read-only, but it uses network.
    Watch {
        /// Repository as `owner/name`. Repeatable.
        #[arg(long = "repo", required = true)]
        repos: Vec<String>,
        #[arg(long, default_value = "bounty")]
        label: String,
        /// Actually make network requests. Without this, nothing leaves the machine.
        #[arg(long)]
        live: bool,
    },
    /// Show realised yield per niche and verify the ledger chain.
    Ledger,
    /// Show free-tier quota consumed today.
    Quota,
}

fn default_limits() -> HashMap<String, QuotaLimits> {
    let mut m = HashMap::new();
    // GitHub unauthenticated: 60 requests/hour. Held well under, because being rate
    // limited mid-sweep corrupts a series rather than merely delaying it.
    m.insert("github".into(), QuotaLimits::new(5, 1_200, u64::MAX));
    m
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    std::fs::create_dir_all(&cli.state)?;
    let ledger = Ledger::open(cli.state.join("ledger.jsonl"))?;
    let governor = Governor::new(default_limits()).persisted(cli.state.join("quota.json"));

    match cli.cmd {
        Cmd::Demo { days } => demo(&ledger, days),
        Cmd::Watch { repos, label, live } => {
            watch(&ledger, &governor, &cli.state, &repos, &label, live)
        }
        Cmd::Ledger => show_ledger(&ledger),
        Cmd::Quota => {
            for provider in default_limits().keys() {
                let l = governor.limits_for(provider);
                println!(
                    "{provider}: {}/{} requests today",
                    governor.requests_today(provider),
                    l.requests_per_day
                );
            }
            Ok(())
        }
    }
}

fn demo(ledger: &Ledger, days: f64) -> Result<()> {
    let src = SimSource::new(
        vec![
            SimNiche::collapsing("fresh-airdrop", 1.5),
            SimNiche::collapsing("bounty-board", 9.0),
            SimNiche::collapsing("slow-program", 45.0),
            SimNiche::stable("boring-but-alive"),
        ],
        days,
    );

    let mut scout = Scout::new(SightingIndex::new());
    let sweep = scout.sweep(&[&src], now_millis());
    println!(
        "scout: {} niches checked, {} new\n",
        sweep.checked,
        sweep.new_niches.len()
    );
    for s in &sweep.new_niches {
        ledger.append(LedgerEvent::NicheDiscovered {
            niche_id: s.niche_id.clone(),
            class: "work_market".into(),
            detection_latency_ms: s.detection_latency_ms(),
            source: s.source.clone(),
        })?;
    }

    let obs = src.observe(0)?;
    let now = obs.iter().map(|o| o.ts_ms).max().unwrap_or(0);
    let meter = CrowdingMeter::default();
    let cfg = PolicyConfig::default();

    let mut rows = Vec::new();
    for n in src.niches()? {
        let report = meter.report(&n.id, &obs, now);
        let decision = policy::decide(&report, &n.entry_cost, &cfg);
        ledger.append(LedgerEvent::Measured {
            niche_id: n.id.clone(),
            samples: report.confidence.samples,
            crowding_index: report.weekly_decay,
            half_life_days: report.half_life_days(),
        })?;
        rows.push((format!("{} ({})", n.id, n.notes), decision, report));
    }

    print!("{}", render::decision_table(&rows));

    let acted: Vec<&String> = rows
        .iter()
        .filter(|(_, d, _)| matches!(d.signal, Signal::Enter | Signal::Hold))
        .map(|(l, _, _)| l)
        .collect();
    println!(
        "\n{} of {} niches worth effort right now.",
        acted.len(),
        rows.len()
    );
    println!("ledger: {}", ledger.path().display());
    Ok(())
}

fn watch(
    ledger: &Ledger,
    governor: &Governor,
    state: &std::path::Path,
    repos: &[String],
    label: &str,
    live: bool,
) -> Result<()> {
    let mut niches = Vec::new();
    for r in repos {
        let (owner, name) = r
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("repo must be owner/name, got '{r}'"))?;
        niches.push(GithubNiche::new(owner, name, label));
    }

    if !live {
        println!("dry run: would poll {} niche(s), one request each\n", niches.len());
        for n in &niches {
            println!("  {}\n    {}", n.id(), n.url());
        }
        println!("\nre-run with --live to actually fetch.");
        return Ok(());
    }

    let src = GithubSource::new(niches, Box::new(UreqTransport::default()));
    // Reserve before calling, never after.
    let mut permits = Vec::new();
    for _ in 0..src.request_cost() {
        match governor.acquire("github", 0) {
            Ok(p) => permits.push(p),
            Err(e) => {
                println!("stopping: {e}");
                return Ok(());
            }
        }
    }

    let mut idx = SightingIndex::load(state.join("sightings.json"))?;
    let now = now_millis();
    for n in src.niches()? {
        if let Some(s) = idx.record(&n, src.id(), now) {
            ledger.append(LedgerEvent::NicheDiscovered {
                niche_id: s.niche_id.clone(),
                class: "work_market".into(),
                detection_latency_ms: s.detection_latency_ms(),
                source: s.source,
            })?;
        }
    }
    idx.save()?;

    let obs = src.observe(0)?;
    for p in permits {
        governor.settle(p, 0);
    }
    println!("collected {} observations\n", obs.len());

    let meter = CrowdingMeter::default();
    let cfg = PolicyConfig::default();
    let mut rows = Vec::new();
    for n in src.niches()? {
        let report = meter.report(&n.id, &obs, now);
        let decision = policy::decide(&report, &n.entry_cost, &cfg);
        ledger.append(LedgerEvent::Measured {
            niche_id: n.id.clone(),
            samples: report.confidence.samples,
            crowding_index: report.weekly_decay,
            half_life_days: report.half_life_days(),
        })?;
        rows.push((n.label.clone(), decision, report));
    }
    print!("{}", render::decision_table(&rows));
    Ok(())
}

fn show_ledger(ledger: &Ledger) -> Result<()> {
    match ledger.verify_chain() {
        Ok(n) => println!("ledger: {n} records, chain intact\n"),
        Err(e) => println!("ledger: CHAIN BROKEN — {e}\n"),
    }
    let y = ledger.yield_by_niche()?;
    if y.is_empty() {
        println!("no niches recorded yet.");
        return Ok(());
    }
    println!(
        "{:<32} {:>10} {:>10} {:>10} {:>12}",
        "NICHE", "EARNED", "SPENT", "NET", "PER REQ"
    );
    println!("{}", "-".repeat(78));
    let mut net_total: i64 = 0;
    for (id, n) in &y {
        net_total += n.net_cents();
        println!(
            "{:<32} {:>9}c {:>9}c {:>9}c {:>12}",
            id,
            n.earned_cents,
            n.spent_cents,
            n.net_cents(),
            n.cents_per_request()
                .map(|v| format!("{v:.2}c"))
                .unwrap_or_else(|| "-".into())
        );
    }
    println!("{}", "-".repeat(78));
    println!("{:<32} {:>31}c", "TOTAL NET", net_total);
    Ok(())
}
