# Halflife

**Measure how fast a niche is closing, and leave before it does.**

## The premise

Six candidate strategies were researched and killed before this one
([KILL-LIST.md](KILL-LIST.md)). They died to a single dynamic:

> When generation is free, every strategy expressible as software saturates almost
> immediately.

Free inference is not a forecast. In 2026 Google AI Studio gives 1,500 requests/day at
1M context, Groq ~14.4K requests/day at 700+ tokens/sec, Cerebras 1M tokens/day, and
OpenRouter 30+ free models. The entry cost of building any software strategy has
collapsed to roughly zero — so any strategy that *can* be built by one person in a week
*will* be built by a great many people in that same week.

This has a consequence people mostly refuse to state: **there is no durable
software-only, zero-capital, zero-work income.** Any answer shaped like "build X,
collect rent" is wrong by construction.

What survives saturation is only ever a non-replicable input — capital, legal or
physical position, privileged access, costly standing, proprietary byproduct data, or
**rotation speed**. At zero capital and no institutional standing, rotation speed is the
only one on the table.

## The thesis

Do not try to own a niche. Own the **exit**.

Every niche has a half-life, and it is observable *before* the margin collapses:

| Signal | What it means when it moves |
|---|---|
| **Claim latency** falling | Machines have arrived. The single clearest signal. |
| **Reward** falling | Margin is being competed away. |
| **Acceptance rate** falling | Same effort, fewer hits. |
| **Competitor count** rising | Direct crowding. |

You act on the **derivative, not the level**. A niche paying well today with a nine-day
half-life and a fourteen-day payback is already a loss; the ledger just has not noticed
yet. Everyone optimises their play; almost nobody instruments how crowded it is
becoming, so they all exit late. Being first *out* is the edge, and unlike being first
*in* it is measurable.

Windows *open* on rule changes — a programme launches, a protocol deploys, an API opens
a free tier. The gap between "announced" and "crowded" is the whole opportunity, so
detection latency is tracked as a first-class metric and the scout is judged on it.

### Why this one does not saturate the way the others did

It is not a position in a market; it is a measurement *of* markets. Its value rises as
the flood grows — more competitors means more niches dying, which means more value in
knowing which ones and when. And it is instrumentation, which nobody wants to build,
because everyone wants to build the play.

### The honest ceiling

**Rotation is an allocator, not a generator.** It maximises yield against whatever
opportunity set exists; it cannot manufacture opportunity. If every niche saturates at
once, a perfect rotator earns approximately nothing.

This is not a money printer, and the ledger exists to say so out loud: it records real
spend against real receipts, per niche, so a stream that costs more than it returns
shows a negative number rather than quietly disappearing.

## What the system does

```
scout ──► niches ──► observations ──► crowding meter ──► policy ──► ledger
  │                                         │                │        │
detection                            runway in days      enter/hold  realised
 latency                             + 95% interval        /exit      yield
```

1. **Scout** notices niches opening and records how late it was.
2. **Sources** emit free observations. Each source is one candidate income stream.
3. **The crowding meter** fits an exponential trend per metric and combines them by
   inverse variance into one runway estimate, with an interval.
4. **Policy** turns the runway into enter / hold / exit, judged against the *fast* end of
   the interval — leaving early costs a little foregone yield, leaving late costs the
   position.
5. **The ledger** records what was spent and what came back, hash-chained, so the
   record survives our own later optimism.

## Design commitments

These are enforced in code, not remembered:

- **Ignorance is not a bearish signal.** `Insufficient` is a distinct verdict from
  `Exit`. An unmeasured niche is not an expired one, and the two call for opposite
  actions.
- **Never emit a verdict the data cannot support.** A trend is only acted on when its
  95% interval lands cleanly in one camp — measurably closing, or measurably stable. An
  interval spanning both means keep measuring.
- **r² gets no vote.** A stable niche has almost no variance to explain and scores a
  near-zero r² however cleanly it is measured. Gating on it discarded exactly the
  long-runway niches worth entering. Evidence is weighted by the standard error of the
  slope instead, which correctly rewards a precisely-measured flat line.
- **Free tiers are structural, not aspirational.** Every metered call reserves quota
  *before* it is made, and counters persist across restarts, so a restart cannot hand
  back a fresh daily allowance.
- **Costs are visible.** Money, requests and seconds are tracked separately, because at
  zero capital they are not interchangeable.

## Running it

```sh
cargo test                        # 75 tests, no network required
cargo run --bin hl -- demo        # full loop against niches with known half-lives
cargo run --bin hl -- watch --repo owner/name --label bounty        # dry run
cargo run --bin hl -- watch --repo owner/name --label bounty --live # one request
cargo run --bin hl -- ledger      # realised yield, and chain verification
```

`demo` is the falsifiable part: the simulated niches carry their true half-lives in
their labels, so the estimator can be checked against the number that generated the
data.

```
NICHE                        SIGNAL    RUNWAY HALF-LIFE 95% CI    WEEKLY
fresh-airdrop   (true 1.5d)  EXIT        1.5d         1.5-1.5d       96%
bounty-board    (true 9d)    HOLD        9.0d         8.9-9.6d       41%
slow-program    (true 45d)   ENTER      44.1d       41.4-65.4d        9%
boring-but-alive (stable)    ENTER     812.4d                -        0%
```

## Sources

Adoption and saturation figures are cited in [KILL-LIST.md](KILL-LIST.md). The
verification statistics behind the discarded thesis 6, and the free-tier figures above,
are drawn from 2026 published research and provider documentation; every non-obvious
number in this repository traces to one of the linked sources in that file.
