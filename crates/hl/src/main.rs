//! Halflife CLI.
//!
//! `hl demo` runs the whole loop against simulated niches whose true half-lives are
//! known, so the estimator can be checked against ground truth without touching a
//! network or spending a request.

mod config;
mod render;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::Config;
use hl_core::{
    now_millis, Governor, Ledger, LedgerEvent, Observation, ObservationStore, QuotaLimits,
    Signal, Source, MS_PER_DAY,
};
use hl_probe::{crowding::CrowdingMeter, policy, PolicyConfig};
use hl_scout::{Scout, SightingIndex};
use hl_venues::{
    github::GithubNiche, github_search::GithubSearchNiche, http::UreqTransport, huggingface::HfNiche,
    kaggle::KaggleSource, ContestsSource, DefiLlamaSource, GithubSearchSource, GithubSource,
    HuggingFaceSource, HyperliquidSource, SimNiche, SimSource,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    /// Poll every configured source once, store what comes back, refresh the report.
    /// This is what the scheduled job runs.
    Sweep {
        /// Exercise the whole path without network calls or writes.
        #[arg(long)]
        dry_run: bool,
        #[arg(long, default_value = "halflife.toml")]
        config: PathBuf,
    },
    /// Render the ranked portfolio from stored observations.
    Report {
        #[arg(long, default_value = "REPORT.md")]
        out: PathBuf,
        #[arg(long, default_value = "halflife.toml")]
        config: PathBuf,
    },
    /// Appraise what live competitions would actually pay, and whether they can be
    /// entered without a person in the loop.
    Appraise,
    /// Pull real daily history for the largest yield pools into the store, so the
    /// paper portfolio has something to replay today rather than in a week.
    Backfill {
        #[arg(long, default_value_t = 40)]
        pools: usize,
    },
    /// Replay stored observations and ask whether following the meter would have paid.
    Paper {
        /// Notional starting balance, in dollars.
        #[arg(long, default_value_t = 1000.0)]
        capital: f64,
        /// How many niches to hold at once.
        #[arg(long, default_value_t = 3)]
        positions: usize,
    },
    /// Search parameters on a training window, then score the winner once on data
    /// that took no part in choosing it.
    Tune {
        #[arg(long, default_value_t = 1000.0)]
        capital: f64,
        /// Share of history used for training. The rest is held out.
        #[arg(long, default_value_t = 0.67)]
        train: f64,
    },
    /// Test the project's own premise: does crowding actually depress reward here?
    Cohort {
        /// Restrict to niches whose id starts with this prefix, e.g. `defi:` or `perp:`.
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long, default_value_t = 90)]
        days: usize,
    },
    /// Hunt live high-rate windows, ranked by what is actually realisable.
    Hunt {
        /// Capital to size the per-hour figures against, in dollars.
        #[arg(long, default_value_t = 20.0)]
        capital: f64,
        #[arg(long, default_value_t = 100.0)]
        min_apy: f64,
        /// Assumed cost of entering and leaving, in dollars.
        #[arg(long, default_value_t = 0.15)]
        entry_cost: f64,
    },
    /// Rank live authorized audit contests by where skilled review time pays best.
    Audit {
        /// Minimum days of review runway left to bother listing a contest.
        #[arg(long, default_value_t = 0.0)]
        min_days: f64,
        /// Hide contests that require KYC to collect.
        #[arg(long)]
        no_kyc: bool,
    },
    /// Map the attack surface of a verified contract, to guide a human review.
    /// Reads only already-public source; finds no bugs, replaces no review.
    Review {
        /// Chain: eth, base, arbitrum, optimism, polygon, gnosis.
        chain: String,
        /// Contract address (must have verified source).
        address: String,
        /// Show every flag, not just the high-severity head.
        #[arg(long)]
        full: bool,
    },
    /// Show realised yield per niche and verify the ledger chain.
    Ledger,
    /// Show free-tier quota consumed today.
    Quota,
}

fn default_limits() -> HashMap<String, QuotaLimits> {
    let mut m = HashMap::new();
    // GitHub core API: 60 requests/hour unauthenticated, 1,000/hour for the Actions
    // token. Held well under, because being rate limited mid-sweep corrupts a series
    // rather than merely delaying it.
    m.insert("github".into(), QuotaLimits::new(5, 1_200, u64::MAX));
    // Search is a separate, much tighter bucket: 30/minute authenticated.
    m.insert("github-search".into(), QuotaLimits::new(10, 600, u64::MAX));
    // Hugging Face publishes no hard limit for anonymous reads; this is politeness,
    // not a documented ceiling. The per-minute figure has to clear one whole sweep in a
    // burst, or a sweep with more tags than the limit can never complete — the governor
    // is all-or-nothing per source by design, since half a sweep fits a trend to half a
    // picture.
    m.insert("huggingface".into(), QuotaLimits::new(30, 2_000, u64::MAX));
    // Kaggle documents no public rate limit; two listing pages an hour is negligible.
    m.insert("kaggle".into(), QuotaLimits::new(10, 500, u64::MAX));
    // DefiLlama is free and unauthenticated, and one request covers every pool.
    m.insert("defillama".into(), QuotaLimits::new(5, 300, u64::MAX));
    // Hyperliquid is public and permissionless; one call covers every market.
    m.insert("hyperliquid".into(), QuotaLimits::new(5, 300, u64::MAX));
    // Backfill is a burst of one request per pool, run by hand rather than hourly, so
    // it gets its own generous-but-bounded bucket instead of eating the sweep's.
    m.insert("defillama-history".into(), QuotaLimits::new(120, 600, u64::MAX));
    // Two contest platforms, polled together; free and unauthenticated.
    m.insert("contests".into(), QuotaLimits::new(5, 300, u64::MAX));
    m
}

/// Build the live sources described by the config.
fn build_sources(
    cfg: &Config,
) -> (
    HuggingFaceSource,
    GithubSearchSource,
    KaggleSource,
    DefiLlamaSource,
    HyperliquidSource,
    ContestsSource,
    Option<GithubSource>,
) {
    let mut hf: Vec<HfNiche> = cfg.hf_model_tags.iter().map(|t| HfNiche::models(t)).collect();
    hf.extend(cfg.hf_dataset_tags.iter().map(|t| HfNiche::datasets(t)));

    let searches: Vec<GithubSearchNiche> = cfg
        .github_searches
        .iter()
        .map(|pair| GithubSearchNiche::new(&pair[0], &pair[1]))
        .collect();

    let repos: Vec<GithubNiche> = cfg
        .github_repos
        .iter()
        .filter_map(|spec| {
            let (repo, label) = spec.rsplit_once(':')?;
            let (owner, name) = repo.split_once('/')?;
            Some(GithubNiche::new(owner, name, label))
        })
        .collect();

    (
        HuggingFaceSource::new(hf, Box::new(UreqTransport::default())),
        GithubSearchSource::new(searches, Box::new(UreqTransport::default())),
        KaggleSource::new(Box::new(UreqTransport::default())),
        DefiLlamaSource::new(Box::new(UreqTransport::default())),
        HyperliquidSource::new(Box::new(UreqTransport::default())),
        ContestsSource::new(Box::new(UreqTransport::default())),
        (!repos.is_empty())
            .then(|| GithubSource::new(repos, Box::new(UreqTransport::default()))),
    )
}

/// Poll one source under quota, returning what it produced.
///
/// A source that fails is reported and skipped: one rate-limited API must not cost us
/// the sweep, and a gap in one series is survivable where a missed sweep is not.
fn poll(
    governor: &Governor,
    provider: &str,
    cost: u32,
    since_ms: u64,
    src: &dyn Source,
) -> (Vec<Observation>, Vec<String>) {
    let mut errors = Vec::new();
    let mut permits = Vec::new();
    for _ in 0..cost {
        match governor.acquire(provider, 0) {
            Ok(p) => permits.push(p),
            Err(e) => {
                errors.push(format!("{provider}: {e}"));
                break;
            }
        }
    }
    if permits.len() < cost as usize {
        // Partial quota means a partial picture; better to skip than to fit a trend to
        // half a sweep.
        for p in permits {
            governor.settle(p, 0);
        }
        return (Vec::new(), errors);
    }
    let out = match src.observe(since_ms) {
        Ok(o) => {
            // A source that returns nothing is not necessarily broken, but it is never
            // what we expect, and staying quiet about it is how a dead source goes
            // unnoticed for days.
            if o.is_empty() {
                errors.push(format!("{}: returned no observations", src.id()));
            }
            o
        }
        Err(e) => {
            errors.push(format!("{}: {e:#}", src.id()));
            Vec::new()
        }
    };
    for p in permits {
        governor.settle(p, 0);
    }
    (out, errors)
}

fn main() -> Result<()> {
    let Cli { state, cmd } = Cli::parse();
    std::fs::create_dir_all(&state)?;
    let ledger = Ledger::open(state.join("ledger.jsonl"))?;
    let governor = Governor::new(default_limits()).persisted(state.join("quota.json"));

    match cmd {
        Cmd::Demo { days } => demo(&ledger, days),
        Cmd::Watch { repos, label, live } => {
            watch(&ledger, &governor, &state, &repos, &label, live)
        }
        Cmd::Sweep { dry_run, config } => sweep(&state, &ledger, &governor, &config, dry_run),
        Cmd::Report { out, config } => report(&state, &out, &config),
        Cmd::Appraise => appraise(),
        Cmd::Backfill { pools } => backfill(&state, &governor, pools),
        Cmd::Paper { capital, positions } => paper(&state, capital, positions),
        Cmd::Tune { capital, train } => tune_cmd(&state, capital, train),
        Cmd::Cohort { prefix, days } => cohort_cmd(&state, prefix.as_deref(), days),
        Cmd::Hunt { capital, min_apy, entry_cost } => hunt_cmd(&governor, capital, min_apy, entry_cost),
        Cmd::Audit { min_days, no_kyc } => audit_cmd(&governor, min_days, no_kyc),
        Cmd::Review { chain, address, full } => review_cmd(&chain, &address, full),
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
        rows.push(render::Row {
            observations: obs.iter().filter(|o| o.niche_id == n.id).count(),
            label: format!("{} ({})", n.id, n.notes),
            decision,
            report,
            value: None,
            unit: render::ValueUnit::Money,
        });
    }

    print!("{}", render::decision_table(&rows));

    let acted = rows
        .iter()
        .filter(|r| matches!(r.decision.signal, Signal::Enter | Signal::Hold))
        .count();
    println!("\n{acted} of {} niches worth effort right now.", rows.len());
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
        rows.push(render::Row {
            observations: obs.iter().filter(|o| o.niche_id == n.id).count(),
            label: n.label.clone(),
            decision,
            report,
            value: None,
            unit: render::ValueUnit::Money,
        });
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

/// One pass over every configured source.
///
/// Designed to be run by a scheduler on a machine that keeps nothing: all state lives
/// in the store, the ledger and the quota file, and every one of those survives the
/// process exiting.
fn sweep(state: &Path, ledger: &Ledger, governor: &Governor, config: &Path, dry_run: bool) -> Result<()> {
    let cfg = Config::load(config)?;
    let store_path = state.join("observations.jsonl");

    if dry_run {
        println!("dry run: {} request(s) would be made\n", cfg.request_cost());
        let (hf, gh, kaggle, yields, perps, contests, repos) = build_sources(&cfg);
        println!("  {} perp funding market(s) via hyperliquid", perps.filter.max_markets);
        let _ = &contests;
        println!("  audit contests via cantina + sherlock");
        println!("  {} yield pool(s) via defillama", yields.filter.max_pools);
        for n in hf.niches()?.iter().chain(gh.niches()?.iter()) {
            println!("  {}", n.id);
        }
        if kaggle.request_cost() == 0 {
            println!("  (kaggle: no KAGGLE_KEY set, source skipped)");
        }
        if let Some(r) = &repos {
            for n in r.niches()? {
                println!("  {}", n.id);
            }
        }
        println!("\nstore: {}", store_path.display());
        return Ok(());
    }

    let mut store = ObservationStore::open(&store_path)?;
    let mut idx = SightingIndex::load(state.join("sightings.json"))?;
    let resume = store.resume_points()?;
    let now = now_millis();
    let before = store.len();
    let mut errors: Vec<String> = Vec::new();

    let (hf, gh_search, kaggle, yields, perps, contests, gh_repos) = build_sources(&cfg);

    let mut all: Vec<Observation> = Vec::new();
    let mut record = |src: &dyn Source, provider: &str, cost: u32| -> Result<()> {
        // Register every niche before polling, so a source that fails still contributes
        // its discovery — knowing a window exists is worth something even when we
        // cannot measure it yet.
        if let Ok(niches) = src.niches() {
            for n in niches {
                idx.refresh(&n);
                if let Some(s) = idx.record(&n, src.id(), now) {
                    ledger.append(LedgerEvent::NicheDiscovered {
                        niche_id: s.niche_id.clone(),
                        class: format!("{:?}", n.class).to_lowercase(),
                        detection_latency_ms: s.detection_latency_ms(),
                        source: s.source,
                    })?;
                }
            }
        }
        let since = resume.get(provider).copied().unwrap_or(0);
        let (obs, errs) = poll(governor, provider, cost, since, src);
        all.extend(obs);
        errors.extend(errs);
        Ok(())
    };

    record(&hf, "huggingface", hf.request_cost())?;
    record(&gh_search, "github-search", gh_search.request_cost())?;
    record(&yields, "defillama", yields.request_cost())?;
    record(&perps, "hyperliquid", perps.request_cost())?;
    record(&contests, "contests", contests.request_cost())?;
    if kaggle.request_cost() > 0 {
        record(&kaggle, "kaggle", kaggle.request_cost())?;
    }
    if let Some(r) = &gh_repos {
        record(r, "github", r.request_cost())?;
    }

    let added = store.append(&all)?;
    let pruned = store.prune(now.saturating_sub((cfg.retain_days * MS_PER_DAY) as u64))?;
    idx.save()?;

    println!(
        "sweep: {} collected, {added} new, {pruned} pruned, {} total",
        all.len(),
        store.len()
    );
    if before == 0 && added > 0 {
        println!("first observations stored; the estimator needs a day or so of these.");
    }
    for e in &errors {
        println!("  ! {e}");
    }

    report(state, Path::new("REPORT.md"), config)
}

/// Render every niche the store knows about, ranked.
fn report(state: &Path, out: &Path, config: &Path) -> Result<()> {
    let cfg = Config::load(config)?;
    let store = ObservationStore::open(state.join("observations.jsonl"))?;
    let obs = store.read_all()?;
    let idx = SightingIndex::load(state.join("sightings.json"))?;
    let now = now_millis();
    let meter = CrowdingMeter::default();
    let policy_cfg = PolicyConfig::default();

    let mut rows: Vec<render::Row> = Vec::new();
    for id in store.niche_ids()? {
        let r = meter.report(&id, &obs, now);
        let sighting = idx.get(&id);
        // A published deadline bounds the runway however healthy the trend looks.
        let days_left = sighting
            .and_then(|s| s.closes_ms)
            .map(|c| c.saturating_sub(now) as f64 / MS_PER_DAY);
        let d = policy::decide_with_deadline(
            &r,
            &hl_core::EntryCost::default(),
            &policy_cfg,
            days_left,
        );
        let label = match sighting {
            Some(s) if !s.label.is_empty() && s.label != id => format!("{id}  ({})", s.label),
            _ => id.clone(),
        };
        let value = obs
            .iter()
            .filter(|o| o.niche_id == id && o.reward_cents.is_some())
            .max_by_key(|o| o.ts_ms)
            .and_then(|o| o.reward_cents);
        rows.push(render::Row {
            observations: obs.iter().filter(|o| o.niche_id == id).count(),
            unit: render::ValueUnit::for_niche(&id),
            label,
            decision: d,
            report: r,
            value,
        });
    }
    // Undetermined niches sort last: they are pending, not promising.
    rows.sort_by(|a, b| {
        let key = |r: &render::Row| match r.decision.signal {
            Signal::Insufficient => (1, 0.0),
            _ => (0, -r.decision.runway_days.unwrap_or(f64::INFINITY)),
        };
        key(a).partial_cmp(&key(b)).unwrap_or(std::cmp::Ordering::Equal)
    });

    let table = render::decision_table(&rows);
    print!("{table}");

    let span_days = match (obs.iter().map(|o| o.ts_ms).min(), obs.iter().map(|o| o.ts_ms).max()) {
        (Some(lo), Some(hi)) => (hi - lo) as f64 / MS_PER_DAY,
        _ => 0.0,
    };
    let actionable = rows
        .iter()
        .filter(|r| !matches!(r.decision.signal, Signal::Insufficient))
        .count();

    let md = format!(
        "# Halflife portfolio\n\n\
         {} observations across {} niches, spanning {:.1} days. \
         {actionable} of {} niches have enough evidence to act on.\n\n\
         `Insufficient` means not enough data yet, which is not the same as a bad niche. \
         Runway is the shorter of the measured trend (against the fast end of its \
         confidence interval) and any published deadline. VALUE is the latest reward \
         reading in its own unit: bounty size for GitHub, prize per entering team for \
         Kaggle, annualised rate for yield pools.\n\n\
         ```\n{}```\n\n\
         Watching {} tag(s) and {} search(es). Regenerated by the scheduled sweep.\n",
        obs.len(),
        rows.len(),
        span_days,
        rows.len(),
        table,
        cfg.hf_model_tags.len() + cfg.hf_dataset_tags.len(),
        cfg.github_searches.len(),
    );
    std::fs::write(out, md).with_context(|| format!("writing {}", out.display()))?;
    println!("\nwrote {}", out.display());
    Ok(())
}

/// What the money actually looks like, once the naive arithmetic is set aside.
fn appraise() -> Result<()> {
    use hl_act::{appraise::Automatability, KaggleActuator};
    use hl_venues::kaggle::KaggleSource;

    if hl_venues::kaggle::auth_token().is_none() {
        println!("no KAGGLE_KEY set; nothing to appraise.");
        return Ok(());
    }

    let source = KaggleSource::new(Box::new(UreqTransport::default()));
    let actuator = KaggleActuator::new(Box::new(UreqTransport::default()));
    let now = now_millis();

    // Niches carry the competition metadata we need; re-listing keeps this command
    // independent of whether a sweep has run.
    let comps = source.competitions()?;
    println!(
        "{:<44} {:>10} {:>7} {:>11} {:>12}  ENTRY",
        "COMPETITION", "PRIZE", "TEAMS", "NAIVE/TEAM", "HONEST EV"
    );
    println!("{}", "-".repeat(104));

    let mut automatable = 0;
    for c in &comps {
        if c.submissions_disabled {
            continue;
        }
        let a = actuator.appraise(c, now, None)?;
        if a.automatable.is_automatable() && a.prize_cents.is_some() {
            automatable += 1;
        }
        println!(
            "{:<44} {:>10} {:>7} {:>11} {:>12}  {}",
            render::truncate_pub(&a.label, 44),
            render::fmt_money(a.prize_cents),
            a.competitors,
            render::fmt_money(a.naive_per_competitor_cents().map(|v| v as u64)),
            render::fmt_money(Some(a.expected_cents as u64)),
            a.automatable.label(),
        );
    }

    println!("\n{automatable} of {} competitions can be entered without a person in the loop.", comps.len());
    println!(
        "NAIVE/TEAM is prize divided by entrants. It is not an expected value and is shown\n\
         only for contrast: prizes go to the top few, so HONEST EV stays at zero until we\n\
         have actually placed in something."
    );
    if automatable == 0 {
        println!(
            "\nEvery cash competition here is {} or {}. That is the venue defending itself\n\
             against automated entry.",
            Automatability::NotebookOnly.label(),
            Automatability::HumanJudged.label()
        );
    }
    Ok(())
}

/// The one falsifiable question this project can answer for free.
fn paper(state: &Path, capital: f64, positions: usize) -> Result<()> {
    use hl_paper::{Backtest, PaperConfig};

    let store = ObservationStore::open(state.join("observations.jsonl"))?;
    let obs = store.read_all()?;
    let bt = Backtest {
        cfg: PaperConfig {
            starting_cents: (capital * 100.0).round() as u64,
            max_positions: positions.max(1),
            ..Default::default()
        },
        ..Default::default()
    };
    let r = bt.run(&obs);

    println!(
        "{:.2} days of history, {} eligible niches, {} steps, ${:.2} notional across {} slot(s)\n",
        r.days, r.eligible_niches, r.steps, capital, positions
    );
    if r.outcomes.is_empty() {
        println!("{}", r.warning.unwrap_or_else(|| "nothing to test".into()));
        return Ok(());
    }

    println!(
        "{:<22} {:>12} {:>10} {:>10} {:>9} {:>8}",
        "STRATEGY", "FINAL", "RETURN", "ANNUALISED", "FEES", "SWITCHES"
    );
    println!("{}", "-".repeat(76));
    for o in &r.outcomes {
        println!(
            "{:<22} {:>11.2} {:>9.4}% {:>9.2}% {:>8.2} {:>8}",
            o.name,
            o.final_cents / 100.0,
            o.return_pct,
            o.apy_pct,
            o.fees_cents / 100.0,
            o.switches
        );
    }

    if let Some(w) = &r.warning {
        println!("\n! {w}");
    }

    // The comparison that decides whether any of this was worth building.
    let rot = r.get("rotation (meter)");
    let chase = r.get("chase top rate");
    let hold = r.get("hold best at start");
    if let (Some(rot), Some(chase), Some(hold)) = (rot, chase, hold) {
        println!();
        let beats_hold = rot.final_cents - hold.final_cents;
        let beats_chase = rot.final_cents - chase.final_cents;
        println!(
            "rotation vs hold:  {:+.2}   rotation vs naive chase:  {:+.2}",
            beats_hold / 100.0,
            beats_chase / 100.0
        );
        if r.warning.is_none() && beats_chase <= 0.0 {
            println!(
                "the meter is not earning its keep yet: chasing the top rate with no \n\
                 estimator at all did as well or better."
            );
        }
    }
    Ok(())
}

/// Load real pool history so the backtest has a window worth measuring.
fn backfill(state: &Path, governor: &Governor, pools: usize) -> Result<()> {
    let src = DefiLlamaSource::new(Box::new(UreqTransport::default()));
    // One request for the listing, then one per pool.
    let cost = pools as u32 + 1;
    let mut permits = Vec::new();
    for _ in 0..cost {
        match governor.acquire("defillama-history", 0) {
            Ok(p) => permits.push(p),
            Err(e) => {
                println!("stopping before any calls: {e}");
                return Ok(());
            }
        }
    }
    println!("fetching daily history for up to {pools} pools ({cost} requests)...");
    let obs = src.backfill(pools)?;
    for p in permits {
        governor.settle(p, 0);
    }

    let mut store = ObservationStore::open(state.join("observations.jsonl"))?;
    let added = store.append(&obs)?;
    let span = match (obs.iter().map(|o| o.ts_ms).min(), obs.iter().map(|o| o.ts_ms).max()) {
        (Some(a), Some(b)) => (b - a) as f64 / MS_PER_DAY,
        _ => 0.0,
    };
    println!(
        "{} points fetched, {added} new, spanning {span:.0} days across {} niches",
        obs.len(),
        obs.iter().map(|o| &o.niche_id).collect::<std::collections::BTreeSet<_>>().len()
    );
    Ok(())
}

/// Parameter search, with the cost of searching reported alongside the result.
fn tune_cmd(state: &Path, capital: f64, train_fraction: f64) -> Result<()> {
    use hl_paper::{tune, PaperConfig};

    let store = ObservationStore::open(state.join("observations.jsonl"))?;
    let obs = store.read_all()?;
    let base = PaperConfig {
        starting_cents: (capital * 100.0).round() as u64,
        ..Default::default()
    };
    let Some(r) = tune(&obs, &base, train_fraction) else {
        println!("not enough history on both sides of the split to tune against.");
        return Ok(());
    };

    println!(
        "{} variants searched on {:.0} training days, scored once on {:.0} held-out days\n",
        r.variants_tried, r.train_days, r.test_days
    );
    println!("  best on training data : {}", r.best_label);
    println!("  its training return   : {:+.2}%   <- chosen by this, so it proves nothing", r.train_return_pct);
    println!("  its HELD-OUT return   : {:+.2}%   <- the only number that counts", r.test_return_pct);
    println!("  degradation           : {:+.2}%   <- what the search cost", r.degradation_pct);
    println!();
    println!("  buy-and-hold, held out: {:+.2}%", r.test_hold_return_pct);
    println!("  median variant, held out: {:+.2}%   <- what an arbitrary choice scored", r.median_test_return_pct);
    println!();
    if r.survived {
        println!(
            "the winner beat both controls out of sample. That is weak evidence of a real\n\
             effect — weak because it is one split of one asset class, and the honest next\n\
             step is a different period or a different market, not a wider grid."
        );
    } else {
        println!(
            "the winner did not clear both controls out of sample. The search ordered noise:\n\
             its training score came from fitting the window it was scored on."
        );
    }
    Ok(())
}

/// The premise, as a number, for whichever market is asked about.
fn cohort_cmd(state: &Path, prefix: Option<&str>, days: usize) -> Result<()> {
    use hl_paper::CohortStudy;

    let store = ObservationStore::open(state.join("observations.jsonl"))?;
    let all = store.read_all()?;
    let obs: Vec<_> = all
        .into_iter()
        .filter(|o| prefix.map_or(true, |p| o.niche_id.starts_with(p)))
        .collect();

    let study = CohortStudy {
        max_days: days,
        ..Default::default()
    };
    let r = study.run(&obs);

    println!(
        "{} niches with enough history{}\n",
        r.niches,
        prefix.map(|p| format!(" under '{p}'")).unwrap_or_default()
    );
    if r.niches == 0 {
        println!("nothing to study. Backfill history first, or drop the prefix filter.");
        return Ok(());
    }

    println!("{:>6}  {:>14}  {:>14}", "DAY", "REWARD vs d0", "CROWD vs d0");
    for (d, rew) in &r.reward_by_age {
        if ![0usize, 7, 14, 30, 45, 60, 90].contains(d) {
            continue;
        }
        let crowd = r
            .crowd_by_age
            .iter()
            .find(|(cd, _)| cd == d)
            .map(|(_, c)| format!("x{c:.2}"))
            .unwrap_or_else(|| "-".into());
        println!("{d:>6}  {rew:>13.3}  {crowd:>14}");
    }

    println!(
        "\nwithin-niche correlation of log(crowd) against log(reward):\n           median {:.3}, negative in {:.0}% of niches",
        r.median_correlation,
        r.share_negative * 100.0
    );
    println!("\n{}", r.verdict);
    Ok(())
}

/// Live high-rate windows, with the difference between quoted and realisable made
/// impossible to miss.
fn hunt_cmd(governor: &Governor, capital: f64, min_apy: f64, entry_cost: f64) -> Result<()> {
    use hl_venues::{
        defillama::{parse_pools, POOLS_URL},
        hunt::{hunt, HuntFilter, RiskKind},
        http::Transport,
    };

    let permit = match governor.acquire("defillama", 0) {
        Ok(p) => p,
        Err(e) => {
            println!("{e}");
            return Ok(());
        }
    };
    let transport = UreqTransport::default();
    let resp = transport.get(POOLS_URL, &[("Accept", "application/json")])?;
    governor.settle(permit, 0);
    if resp.status != 200 {
        anyhow::bail!("defillama returned status {}", resp.status);
    }
    let pools = parse_pools(&resp.body)?;
    let cands = hunt(
        &pools,
        &HuntFilter {
            min_apy,
            ..Default::default()
        },
    );

    println!(
        "{} candidates at or above {:.0}% APY, sized against ${:.2}\n",
        cands.len(),
        min_apy,
        capital
    );
    println!(
        "{:<32} {:>9} {:>9} {:>8} {:>9} {:>9}  WHAT THE RATE IS",
        "POOL", "HEADLINE", "BASE", "EMIT%", "c/h base", "c/h head"
    );
    println!("{}", "-".repeat(116));
    for c in &cands {
        println!(
            "{:<32} {:>8.0}% {:>8.0}% {:>7.0}% {:>9.3} {:>9.3}  {}{}",
            render::truncate_pub(&format!("{}/{} {}", c.chain, c.project, c.symbol), 32),
            c.apy,
            c.apy_base,
            c.emission_share * 100.0,
            c.base_cents_per_hour(capital),
            c.headline_cents_per_hour(capital),
            c.risk.label(),
            if c.stablecoin { ", stable principal" } else { "" },
        );
    }

    let realisable: Vec<_> = cands.iter().filter(|c| c.risk == RiskKind::BaseYield).collect();
    println!(
        "\n{} of {} pay in the asset you deposit. The rest quote a rate in a token whose\n         price their own emissions are diluting, so the headline column is an upper bound\n         that assumes the token holds its value.",
        realisable.len(),
        cands.len()
    );
    if let Some(best) = realisable.first() {
        match best.hours_to_repay(capital, entry_cost) {
            Some(h) => println!(
                "\nbest realisable: {} {} at {:.0}% base -> {:.3} c/hour on ${:.0}, \n\
                 repaying a ${:.2} entry in {:.0}h",
                best.chain, best.symbol, best.apy_base,
                best.base_cents_per_hour(capital), capital, entry_cost, h
            ),
            None => println!("\nno candidate repays its entry cost from base yield alone."),
        }
    }
    println!(
        "\nTwo distortions this list cannot correct for: every pool here is one that has\n         not collapsed yet, and pools that died are absent from the source entirely."
    );
    Ok(())
}

/// Where a skilled reviewer should spend time. This ranks authorized contests; it does
/// not review, submit, or fetch code. The human does the work inside the contest rules.
fn audit_cmd(governor: &Governor, min_days: f64, no_kyc: bool) -> Result<()> {
    use hl_venues::ContestsSource;

    let mut permits = Vec::new();
    for _ in 0..2 {
        match governor.acquire("contests", 0) {
            Ok(p) => permits.push(p),
            Err(e) => { println!("{e}"); return Ok(()); }
        }
    }
    let src = ContestsSource::new(Box::new(UreqTransport::default()));
    let mut contests = src.fetch()?;
    for p in permits { governor.settle(p, 0); }

    let now = now_millis();
    contests.retain(|c| c.prize_usd > 0.0);
    contests.retain(|c| c.days_left(now).map_or(true, |d| d >= min_days));
    if no_kyc {
        contests.retain(|c| !c.kyc_required);
    }

    // Rank by marginal pot: prize divided by findings already in, so an under-reviewed
    // pot outranks a bigger one that a hundred people have already combed. Contests with
    // no published findings count fall back to the raw pot, flagged as unknown depth.
    contests.sort_by(|a, b| {
        let key = |c: &hl_venues::Contest| c.prize_per_finding().unwrap_or(c.prize_usd);
        key(b).partial_cmp(&key(a)).unwrap_or(std::cmp::Ordering::Equal)
    });

    println!("{} live authorized contest(s)\n", contests.len());
    println!(
        "{:<30} {:>9} {:>8} {:>11} {:>7}  FLAGS",
        "CONTEST", "POT", "FOUND", "$/FINDING", "DAYS"
    );
    println!("{}", "-".repeat(92));
    for c in &contests {
        println!(
            "{:<30} {:>9} {:>8} {:>11} {:>7}  {}",
            render::truncate_pub(&format!("{} [{}]", c.name, c.platform), 30),
            format!("${:.0}", c.prize_usd),
            c.findings_so_far.map(|f| f.to_string()).unwrap_or_else(|| "?".into()),
            c.prize_per_finding().map(|v| format!("${v:.0}")).unwrap_or_else(|| "-".into()),
            c.days_left(now).map(|d| format!("{d:.1}")).unwrap_or_else(|| "?".into()),
            if c.kyc_required { "KYC" } else { "open" },
        );
    }
    println!(
        "\nRanked by pot-per-finding: a big pool already split across hundreds of findings\n         is worse than a smaller under-reviewed one. This ranks WHERE to look; whether you\n         can find a valid, unique bug in that codebase is the actual work, and it is real\n         work. '$/FINDING' is contest crowding, not your expected payout."
    );
    Ok(())
}

/// Map a contract's attack surface for a human reviewer. Reads public verified source
/// only; it does not find bugs, and a clean map is not a clean bill of health.
fn review_cmd(chain: &str, address: &str, full: bool) -> Result<()> {
    use hl_audit::{analyze, surface::Severity, fetch_sources};

    let transport = UreqTransport::default();
    let src = fetch_sources(&transport, chain, address)?;
    let files: Vec<(String, String)> =
        src.files.iter().map(|f| (f.path.clone(), f.code.clone())).collect();
    let map = analyze(&files);

    println!(
        "{} on {}  ({} file(s), {} lines{})",
        src.name,
        src.chain,
        src.files.len(),
        src.total_lines(),
        src.compiler.as_deref().map(|c| format!(", {c}")).unwrap_or_default()
    );
    println!(
        "attack surface: {} external state-changing function(s), {} payable, surface score {}",
        map.entry_points, map.payable_entry_points, map.surface_score
    );
    println!(
        "flags: {} high, {} total\n",
        map.high(),
        map.flags.len()
    );

    let shown: Vec<_> = if full {
        map.flags.iter().collect()
    } else {
        map.flags.iter().filter(|f| f.severity == Severity::High).collect()
    };
    if shown.is_empty() {
        println!("(no {} flags)", if full { "" } else { "high-severity" });
    } else {
        for f in shown {
            println!(
                "  [{}] {:<22} {}:{}",
                f.severity.tag(), f.category, f.file.rsplit('/').next().unwrap_or(&f.file), f.line
            );
            println!("        {}", f.snippet);
            println!("        why: {}", f.why);
        }
    }
    if !full && map.flags.len() > map.high() {
        println!("\n{} lower-severity flag(s) hidden; --full to see them.", map.flags.len() - map.high());
    }

    println!(
        "\nThis maps where to look. It does no dataflow and is blind to logic bugs, which\n         are most real findings. A clean map means nothing obvious in the known-footgun\n         set, never that the code is safe. The review is yours to do."
    );
    Ok(())
}
