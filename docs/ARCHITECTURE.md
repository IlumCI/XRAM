# Architecture

Five crates. The dependency direction is strictly downward.

```
hl (CLI)
 ├── hl-scout    window detection, detection-latency accounting
 ├── hl-venues   observation sources (sim, GitHub), HTTP transport
 ├── hl-probe    trend fitting, crowding meter, rotation policy
 └── hl-core     domain types, quota governor, hash-chained ledger, PRNG
hl-exec          sandboxed execution (used by niche classes that run foreign code)
```

## hl-core

Domain model plus the two disciplines that must not be optional.

- **`types`** — `Niche`, `Observation`, `Signal`, `EntryCost`, `Confidence`, and the
  `Source` trait every venue implements. `EntryCost` splits money from requests from
  seconds, because at zero capital they are not interchangeable.
- **`governor`** — every metered call goes through `acquire()`, which reserves *before*
  the call and persists counters via write-then-rename. A crash mid-flight costs quota,
  never money; a restart cannot reset the daily allowance. An unconfigured provider
  cannot be called at all.
- **`ledger`** — append-only JSONL, each record hashed over its predecessor. Editing any
  past record breaks the chain and `verify_chain` says so. `yield_by_niche` is the table
  that decides what to keep and what to abandon; losses show as negative numbers.
- **`store`** — append-only observation store with stable dedupe keys. Polls overlap by
  design (inclusive resume boundaries, so nothing is missed), which means the same
  observation arrives repeatedly; stacking duplicates at one timestamp would weight that
  moment more heavily than the rest of the series and bias every fit after it. A changed
  value at the same instant *is* kept — that is new signal, not a duplicate. One corrupt
  line from a killed job is skipped rather than discarding the series, and retention
  pruning bounds growth for something that appends hourly forever.
- **`prng`** — SplitMix64, seeded from a key. Deliberately not `rand`: every random
  choice must be replayable from the ledger.

## hl-probe

The measurement core, in three separable layers.

### `fit` — robust exponential trend fitting

Crowding metrics move multiplicatively, so the model is `y(t) = A·e^(-λt)` fitted in log
space. The default estimator is **Theil–Sen** (median of pairwise slopes), which
tolerates roughly 29% corrupted points — scraped data contains mis-parsed rewards and
weekend outliers that would drag a least-squares line badly. There is a test asserting
Theil–Sen stays within 10% of truth on a series where one point is three orders of
magnitude out, while OLS does not.

Each fit reports `lambda_stderr`, from which come `lambda_ci95()` and
`half_life_ci95()`. `precision()` returns the inverse-variance weight.

### `crowding` — one runway from four metrics

Each of claim latency, reward, acceptance and competitor count is fitted independently
and converted to a *pressure*: the rate at which the niche loses value to us, per day.
Competitors is the one metric where rising is bad, and that asymmetry is encoded in
exactly one place.

Metrics are combined by **inverse-variance weighting**, which is the statistically
optimal weighted mean and also yields the aggregate standard error directly as
`1/√Σw`. This replaced r²-weighting, which silently discarded precisely-measured flat
metrics — see the regression tests in `policy`.

`is_determined()` asks whether the 95% interval lands cleanly in one camp: measurably
closing (interval excludes zero) or measurably stable (interval entirely below a rate
too slow to plan around). An interval covering both is a reason to keep measuring.

Many samples in one period are collapsed to per-bucket medians first, anchored at the
bucket midpoint, so one busy afternoon cannot set a weekly trend.

### `policy` — runway to decision

Affordability first and independently: a perfect window we cannot pay to enter is not
ours. Then data sufficiency, then determination, then the runway comparison. Decisions
are taken against `runway_days_conservative` — the fast end of the interval — because
the cost of being wrong is asymmetric.

## hl-venues

Each source is a candidate income stream, and all of them are free to poll.

- **`http`** — a `Transport` trait so every source is testable against recorded payloads
  with no network. Bodies are read through the reader rather than `into_string`, whose
  undocumented 10 MB ceiling silently killed the yield source: the pool listing is a
  single ~11 MB document. An explicit 64 MB cap replaces it. The `ureq` implementation loads trust anchors from `SSL_CERT_FILE`,
  `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE` and the system store, and honours `HTTPS_PROXY`
  — egress commonly runs through a TLS-inspecting proxy whose CA is installed in the
  environment rather than baked into any crate.
- **`sim`** — niches with *known* half-lives, so the meter's central claim is
  falsifiable. Tests assert recovery within 15% for half-lives from 2 to 21 days.
- **`github`** — real crowding data from the public API, one request per niche.
  Deliberately honest about its proxies: the list endpoint does not expose
  time-to-first-response, so claim latency here is time-from-open-to-close and
  competitor count is the comment count. Pull requests are excluded (their lifetime
  measures review speed, not claim speed). Observations are stamped when work was
  *taken*, not when the issue opened, because the trend is about the present.
  `describe_failure` separates "rate limited" from "token scoped elsewhere" — both
  arrive as HTTP 403, and confusing them sends you tuning poll intervals when the real
  problem is credentials.
- **`huggingface`** — tag saturation, and the strongest signal in the array. One
  unauthenticated request returns the 100 newest artefacts for a tag; the span they
  cover is the creation rate, so competitor density comes from a single call with no
  token. Stamped at the newest artefact rather than at poll time, which makes repeated
  polls of a quiet tag deduplicate naturally — nothing new created means nothing new to
  record. Deliberately reports no reward metric: downloads accumulate with age and
  cohort age shrinks as the rate rises, so any reward read from the same request would
  move with the rate and double-count it.
- **`github_search`** — cross-repository discovery, which is what turns the scout from
  "watch these repos" into "find windows anywhere". Reuses `parse_issues` on the search
  envelope's items. Search is a much tighter rate bucket than the core API (30/min
  authenticated, 10 unauthenticated), so it gets its own provider in the governor.
- **`kaggle`** — the cleanest niche in the array, because crowding is published rather
  than inferred: `teamCount` is literally how many others are chasing the prize, so
  expected value per entrant is a division. Two properties differ from every other
  source. Reward is fixed while entrants accumulate, so value per team decays
  mechanically; and each competition carries a **published deadline**, which no amount
  of measurement would reveal. That deadline is carried on `Niche::closes_ms` and bounds
  the runway directly — without it, a contest launched three days ago with two days left
  reads as "no measured erosion, enter". Non-cash prizes ("Knowledge", "Swag") are left
  unscored rather than recorded as zero, since they are a different kind of thing rather
  than a cheap one. Absent a token the source reports nothing and the rest of the sweep
  proceeds.
- **`defillama`** — yield pools, and the closest thing to a natural fit this project has
  found. A yield is a rate that decays as capital floods in, which is exactly what the
  crowding meter measures, so the mapping needed no new machinery: **APY** becomes the
  reward metric (falling = margin competed away) and **TVL** becomes competitor density
  (rising = capital crowding in). Both directions were already handled correctly.
  Measures `apyBase` rather than headline `apy`, because the headline folds in
  reward-token emissions that stop without warning. Filters out thin pools posting
  spectacular unreachable rates, pools carrying impermanent-loss risk (a second,
  uncorrelated way to lose the position that a rate trend says nothing about), and
  DefiLlama's own flagged outliers. One unauthenticated request covers every pool.
- **`hyperliquid`** — perpetual funding rates, and the purest crowding signal available
  anywhere: a funding rate is the price one side of a trade pays the other *for being
  crowded*, and it exists precisely to attract capital to the empty side until the
  imbalance closes. The rate is the crowd, quoted in basis points, decaying as the crowd
  arrives. |funding| maps to reward, open interest to competitor density. Chosen over
  Gate, KuCoin, Binance and Bybit because it is permissionless — the centralised venues
  need an account, which needs identity, which is the wall that killed most of this
  project's earlier ideas; Binance and Bybit also geo-block this egress outright. The
  response pairs assets to contexts positionally, so a length mismatch is refused rather
  than silently misattributing every rate.
- **`contests`** — authorized security-audit contests (Cantina, Sherlock), and the one
  non-rival seam in the whole source set. A contest is a public invitation to review a
  named scope for a prize; two reviewers who find different bugs are both paid, so
  crowding depletes only the stock of unfound bugs, published as a findings count. Prize
  maps to reward, findings-so-far to the crowd, the contest deadline to a hard close.
  The source surfaces and ranks; it never fetches contract source, scans, or submits,
  and private/invite-only contests are excluded because they are not open invitations.
  One null field in a live payload once failed the entire array parse — string fields
  that the API can return as null are `Option` with a fallback, so one bad element never
  drops the batch.
- **`timefmt`** — RFC 3339 parsing without a datetime dependency. Accepts only the UTC
  `Z` form; an offset form is refused rather than silently mis-parsed, since every
  latency we measure depends on it.

## hl-scout

`SightingIndex` persists every niche ever seen, so a restart does not rediscover the
world as brand new and render detection latency meaningless. A failing source is
recorded and stepped over rather than aborting the sweep. Median (not mean) detection
latency, so one niche found years late does not swamp the statistic.

## hl-exec

Sandboxed execution, retained from the discarded thesis 6 because several niche classes
require running code we did not write. A private working copy, a scrubbed environment
(sandboxed code must not inherit API keys), a hard wall-clock cap, network isolation via
`unshare -rn` where the kernel permits it unprivileged, and capped output capture. The
`Sandbox` trait exists so a microVM backend can replace the local one without touching
callers.

## Scheduled operation

Secrets: `GITHUB_TOKEN` is supplied automatically; `KAGGLE_KEY` is optional and must be
added as a repository secret. Credentials are read from the environment only and never
written to the repository or the state branch.

`.github/workflows/sweep.yml` runs `hl sweep` hourly on GitHub-hosted runners, free and
uncapped because the repository is public. State is snapshotted to a separate `data`
branch by force-push, keeping machine commits out of `main` and stopping git history
from growing without bound — the real history lives inside `observations.jsonl`, bounded
by retention pruning.

The Actions token also matters for correctness, not just politeness: unauthenticated
search allows 10 requests/minute against a shared runner IP, which in practice means
403s, while the CI token raises it to 1,000/hour.

Note that the governor is all-or-nothing per source: a sweep that cannot reserve quota
for every one of its requests skips the source entirely, because half a sweep fits a
trend to half a picture. Per-minute limits therefore have to clear one whole sweep in a
burst.

## Testing

101 tests, none requiring network. The estimator is checked against ground truth from
`hl-venues::sim`; the governor is asserted to hold under synthetic flood and across
simulated restarts; the ledger is asserted to detect tampering. One `#[ignore]`d test
hits the live GitHub API for manual verification.
