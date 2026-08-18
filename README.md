# Halflife

**Measure how fast a niche is closing, and leave before it does.**

When generation is free, every strategy expressible as software saturates almost
immediately. So this does not try to own a niche — it instruments how crowded each one
is becoming and rotates out before the margin collapses. Everyone optimises their play;
almost nobody measures its half-life, so they all exit late.

```sh
cargo test                      # 75 tests, no network
cargo run --bin hl -- demo      # full loop against niches with known half-lives
```

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
- [docs/KILL-LIST.md](docs/KILL-LIST.md) — six strategies considered, and the 2026 fact
  that killed each
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — how the code works

Zero capital, free tiers only. The quota governor reserves before every metered call and
persists counters across restarts, so the system is structurally incapable of running up
a bill.
