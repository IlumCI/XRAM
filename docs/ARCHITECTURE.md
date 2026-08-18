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
  with no network. The `ureq` implementation loads trust anchors from `SSL_CERT_FILE`,
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

## Testing

75 tests, none requiring network. The estimator is checked against ground truth from
`hl-venues::sim`; the governor is asserted to hold under synthetic flood and across
simulated restarts; the ledger is asserted to detect tampering. One `#[ignore]`d test
hits the live GitHub API for manual verification.
