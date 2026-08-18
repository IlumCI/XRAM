# Halflife

**Measure how fast a niche is closing, and leave before it does.**

When generation is free, every strategy expressible as software saturates almost
immediately. So this does not try to own a niche — it instruments how crowded each one
is becoming and rotates out before the margin collapses. Everyone optimises their play;
almost nobody measures its half-life, so they all exit late.

```sh
cargo test                      # 144 tests, no network
cargo run --bin hl -- demo      # full loop against niches with known half-lives
cargo run --bin hl -- sweep     # poll every source, store, refresh REPORT.md
```

A scheduled workflow runs the sweep hourly on free public-repo runners and snapshots
state to the `data` branch, because the estimator refuses to speak until it has roughly
six samples over half a day — and nothing invoked by hand ever gets there. Live
portfolio state: [REPORT.md on the `data` branch](../../blob/data/REPORT.md).

```
NICHE                        SIGNAL    RUNWAY HALF-LIFE 95% CI    WEEKLY
fresh-airdrop   (true 1.5d)  EXIT        1.5d         1.5-1.5d       96%
bounty-board    (true 9d)    HOLD        9.0d         8.9-9.6d       41%
slow-program    (true 45d)   ENTER      44.1d       41.4-65.4d        9%
boring-but-alive (stable)    ENTER     812.4d                -        0%
```

**Honest ceiling:** rotation is an allocator, not a generator. It maximises yield
against whatever opportunity set exists; it cannot manufacture opportunity.

- [docs/THESIS.md](docs/THESIS.md) — the reasoning and the design commitments
- [docs/KILL-LIST.md](docs/KILL-LIST.md) — seven strategies considered, and the fact
  that killed each
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — how the code works

The largest source is **yield pools**: a rate that decays as capital floods in is
precisely what this measures, so APY maps onto the reward metric and TVL onto competitor
density with no new machinery. Unlike work markets, yield markets have no identity gate,
no terms to accept, no per-item human step and no minimum position.

Sources are all free. Kaggle needs a token (`KAGGLE_KEY`, as a repository secret for the
scheduled run); everything else works unauthenticated, though a GitHub token raises
search from 10 requests/minute to 1,000/hour. Hugging Face tag saturation is
the cheapest honest crowding measurement available: one request returns the 100 newest
artefacts for a tag, and the span they cover *is* the creation rate — `text-generation`
runs at ~4,400 new models/day, `robotics` at ~90.

Zero capital, free tiers only. The quota governor reserves before every metered call and
persists counters across restarts, so the system is structurally incapable of running up
a bill.
