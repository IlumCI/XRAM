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
    kaggle::KaggleSource, DefiLlamaSource, GithubSearchSource, GithubSource, HuggingFaceSource,
    HyperliquidSource, SimNiche, SimSource,
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
        let (hf, gh, kaggle, yields, perps, repos) = build_sources(&cfg);
        println!("  {} perp funding market(s) via hyperliquid", perps.filter.max_markets);
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

    let (hf, gh_search, kaggle, yields, perps, gh_repos) = build_sources(&cfg);

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
