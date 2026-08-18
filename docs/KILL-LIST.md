# Kill list

Every idea considered for this project, and the specific 2026 fact that killed it.
Written down so none of them gets re-litigated later on vibes.

The theses are in the order they were reached. Each one survived the objection that
killed its predecessor, which is why the list is worth keeping: the pattern across the
deaths turned out to matter more than any individual death.

---

## 1. AI agent products and wrappers

**Killed by:** 5,600+ AI startups shut down between January 2025 and March 2026, almost
all in horizontal categories. Capital consolidated into a handful of orchestration
platforms, starving everything below them.

Not a market. A graveyard.

---

## 2. Crypto MEV searching in Rust

**Killed by:** two things at once.

Solana MEV hit record volume in early 2026 ($480M+ cumulative searcher profit through
Q2), but the contest is now microsecond proximity to the slot leader — *"strategy
quality is now secondary to infrastructure quality for most active categories."* That is
colocation, capex and permanent redeployment: a full-time job with a hardware budget.

And the extractable surface is shrinking **by design**. Jito's Block Assembly
Marketplace runs an encrypted mempool inside TEEs, and more than 50% of Solana
validators already run it.

Requires capital. Requires daily operation. Competes against firms whose entire payroll
does only this.

---

## 3. Prediction-market arbitrage (Polymarket / Kalshi)

**Killed by:** commoditisation. Spreads of 1.5–4.5% in 2–7 second windows, with
step-by-step tutorials on dev.to. Requires capital on both venues, KYC, and continuous
inventory management.

Piecework, not rent — and impossible at zero capital.

---

## 4. x402 / agentic payment rails

**Killed by:** the volume is not real. ~165M settled transactions and ~69k active agents
against a ~$7B ecosystem valuation — but **~$28K/day of actual commerce, roughly half of
it classified as gamified**. CoinDesk, March 2026: *"demand is just not there yet."*

Building here means competing with Stripe/OpenAI (ACP), Google (AP2), Visa (TAP),
Mastercard and Coinbase over a $28K/day pie.

**MCP monetisation** dies the same way: under 5% of MCP servers earn anything at all,
and global agent-to-tool payment volume is below $50K/day.

---

## 5. The delivery index / oracle business

The strongest *rent* idea considered, and the one that got closest.

The reasoning: whoever defines the number everyone settles against gets paid without
working — S&P, Platts, Argus, ICE Benchmark. That playbook was just run on AI in
18 months by Silicon Data (DRW-backed): scrape GPU rental prices, normalise, ship
`SDH100RT` to Bloomberg, and from **5 October 2026 CME/NYMEX settles H100 and B200
Compute futures against their index**.

**Killed by:** the obvious version is already owned. Silicon Data also ships `SDLLMTK`,
an LLM Token Expenditure Index, daily. The remaining gap — indexing what providers
actually *deliver* rather than what they post — is real and defensible, but it needs a
probe budget, institutional credibility, and eventually customers.

**It is a business.** Ruled out by the constraints, not by the logic. If the constraints
ever change, this is the one to revisit.

---

## 6. Verified work, submitted first

The plan that was actually approved and started, then killed mid-build.

The reasoning: generation is free and commoditised (~81% on SWE-bench Verified, and free
tiers everywhere), while verification is the decisive step nobody spends on — of agent
patches that look plausible, **7.8% pass patch-level tests but fail the full developer
suite, 29.6% diverge functionally from ground truth**, and verification costs **0.02
seconds per patch**. So: spend nothing on generation, spend everything on verification
throughput, and submit only provably-correct work to open bounty markets.

**Killed by:** the venues are closing, and quality does not buy access.

- **curl ended its six-year HackerOne bug bounty programme in January 2026**, explicitly
  because AI-generated submissions flooded the security queue.
- **The Jazzband collective shut down entirely** under AI-generated spam PRs and issues.
- **Ghostty (January 2026)** restricted AI contributions to pre-approved issues and
  existing maintainers.
- GitHub's Copilot coding agent already does issue → branch → tests → PR natively.
  Cursor background agents run at roughly $4–5 per pull request. Cursor is at $2B ARR.

The fatal assumption was that provable correctness would earn access. It does not:
maintainers pay the triage cost **before** they can distinguish good from slop, so they
are closing the door **categorically** — to correct submissions too. Provably-right work
in front of a shut door is worth zero.

---

## The pattern

All six died to variations of one dynamic. Each was a position *inside* a game whose
entry cost has collapsed to approximately zero, because agents now write the software.

> **When generation is free, every strategy expressible as software saturates almost
> immediately.**

Which means any answer shaped like *"build X, collect rent"* is wrong by construction.
If one person can build it in a week with free inference, two million people can build
it in that same week.

What survives saturation is only ever a **non-replicable input**: capital; legal or
physical position; privileged access; costly non-transferable standing; proprietary data
generated as a byproduct of something else; or **rotation speed** — owning no niche at
all, and instrumenting crowding directly so you leave before the margin does.

At zero capital and with no institutional standing, rotation speed is the only one of
those available. That is what this repository builds. Its honest ceiling is stated in
[THESIS.md](THESIS.md): rotation is an allocator, not a generator.

---

## 7. Automated competition entry

Phase 4 of the build: enter machine-judged competitions automatically, on the reasoning
that entry is free, automation is expected rather than resented, and the payoff
distribution is fat-tailed.

**Killed by:** the money is deliberately walled off from automation, and a survey of the
live board says so unambiguously. Of 21 competitions:

| Shape | Count | Automatable |
|---|---|---|
| Cash prize, notebook-only entry | 5 | no — the API cannot enter these at all |
| Cash prize, human-judged prose | 1 | no |
| Cash prize, file submission | 2 | yes |
| No cash prize | 13 | pointless |

Every large prize sat behind notebook-only submission — ARC-AGI at $850k and $700k,
RSNA at $77k, Biohub at $60k, AI Agent Security at $50k. The two genuinely automatable
cash competitions were the Pokémon strategy challenge and Kaggriculture, the latter the
most crowded on the board at 5,185 teams for $50k.

Two further blockers, both verified against the live API rather than assumed:

* **There is no join endpoint.** Accepting competition rules is web-only; the API
  answers submission calls with *"You do not have a Team in this Competition"* until a
  person has accepted in their own name. The official CLI cannot do it either.
* **Expected value was never what it looked like.** Prize divided by entrants — the
  figure this project briefly quoted at $3,169/team — is not an expectation. Prizes go
  to the top few, so a median entrant expects approximately nothing.

**The pattern, one layer down.** Notebook-only submission is a venue defending itself
against automated entry. That is this project's own thesis reappearing at the actuation
layer: *what can be automated gets saturated, so whatever still pays has built a wall
against automation.* And the wall is made of identity — an account, in a person's name,
that has accepted terms. The same non-replicable input the whole search started from.

What survives is the classification itself, in `hl-act`: the rare exception — real
money, file submission, thin field — now gets flagged the day it appears rather than
rediscovered by hand.

---

## 8. Rotation itself, on yield rates

The project's own premise, tested against 1,353 days of real pool history pulled from
DefiLlama's free per-pool endpoint. $1,000 notional, three slots, daily steps, no
lookahead — the meter sees only observations timestamped at or before each step.

| Strategy | Return | Annualised | Fees | Switches |
| --- | --- | --- | --- | --- |
| **hold best at start** | **+16.34%** | 4.41% | $0.70 | 1 |
| chase top rate (no meter) | +11.23% | 3.03% | $156.58 | 405 |
| rotation (meter) | +8.95% | 2.41% | $185.64 | 491 |
| rotation (fee-aware) | +5.78% | 1.56% | $1.10 | 3 |

**Killed by:** buy-and-hold beat every active strategy, and the meter did worse than
naively chasing the top rate.

The first reading was that fees did it — rotation paid $185.64 to earn $89.49, switching
every 2.8 days. That produced a real fix, since the policy had no notion of switching
cost at all: it emitted enter/hold/exit purely on runway without ever asking whether a
move repays itself. A fee-aware variant with a breakeven hurdle cut fees to $1.10.

It came last. **Selection, not churn, is the defect.** Near-zero fees and it still lost
to holding.

The specific diagnosis: the meter assumes a rate, once crowding starts, keeps decaying —
that is what a half-life *is*. Stablecoin yields do not do that. They mean-revert. A
pool paying well today tends to keep paying, and the pools that appear with spectacular
new rates are precisely the ones that collapse. Fitting an exponential decay to an
oscillation produces confident readings about a shape the data does not have.

### Then it was tuned properly, and still lost

"Why not tune it until it wins" is the right question, and the answer is not "don't
tune" — refining against evidence is the job. The trap is narrower: tuning and *scoring*
on the same data. Run enough variants against one sample and the best wins by
construction, its edge being the maximum of many draws from noise.

So the history was split. 72 variants competed on 906 training days; the single winner
was then run once on 446 days that took no part in choosing it.

| | Return |
| --- | --- |
| Winner, on the training data that chose it | **+14.30%** |
| Winner, on held-out data | **−0.06%** |
| Buy-and-hold, same held-out window | **+5.77%** |
| Median variant, same held-out window | +0.94% |

Degradation of **+14.36 points**. The entire training edge was selection. Out of sample
the tuned winner lost to buy-and-hold by nearly six points and lost to an arbitrary
variant — the search ordered noise and nothing else.

**The search also found a bug in its own benchmark**, which is the other thing this kind
of work is for. `hold best at start` required a rate reading at the exact first
millisecond of the window. Pools report at their own time of day, so exactly one niche
of thirty-five qualified, and the benchmark was holding whichever pool was stamped
earliest rather than whichever paid best — a coin toss that rotation was being measured
against. It now ranks over a seven-day warm-up. On the full history the conclusion was
unchanged (the earliest reporter happened to be a good pool), but the held-out figure
moved from a nonsensical −0.06% to +5.77%.

Where the model might still hold is markets where crowding kills an opportunity
*permanently* rather than temporarily: a new incentive programme, an airdrop window, a
bounty niche that saturates and stays saturated. That is a testable claim and it has not
been tested. It should not be believed until it is.

---

## 9. A different market — and where the premise actually holds

§8 killed rotation on yield pools. The obvious objection is that the market was wrong,
not the model, so the premise was tested directly in two markets rather than inferred
from a strategy that lost.

**The premise, stated as a measurement:** capital arrives, and the rate falls.

### Yield pools: the premise fails

139 stablecoin pools, aligned by their own age rather than calendar date and normalised
to each pool's own first reading.

| Days | Median rate vs day 0 | Median TVL vs day 0 |
| --- | --- | --- |
| 0 | 1.000 | ×1.00 |
| 30 | 1.000 | ×1.36 |
| 60 | 0.994 | **×2.07** |

TVL doubles in sixty days and the median rate does not move. Within-pool, log(TVL)
against log(APY) correlates at **−0.24**, negative in 67% of pools. The mechanism is
present and far too weak to survive fees — roughly a quarter the strength the model
assumes. On the repository's own stored history the same tool reports **−0.152**.

These are pools *currently listed* above $300k TVL, so pools that collapsed and were
delisted are absent. Survivorship makes even this optimistic.

### Perp funding: the premise holds, a hundred times faster

40 liquid Hyperliquid perps, 21 days of hourly funding. Taking every moment |funding|
enters the top decile (≥20% APR) and looking forward:

| Horizon | Median after | vs 11% baseline |
| --- | --- | --- |
| +1h | 30.1% APR | 2.75× |
| +6h | 17.8% APR | 1.62× |
| +24h | 11.0% APR | **1.00×, fully reverted** |

Continuous episodes last a median of **2 hours**. The excess over baseline decays with a
half-life of roughly **3.3 hours**.

This is the shape the meter was built for — a window that opens, pays, and closes. It is
simply two orders of magnitude faster than the design assumed. Consequences, plainly:

* An **hourly** sweep sees a three-hour half-life perhaps twice before it is gone.
* Six hours at 30% APR is 0.02% of notional. The window is real and thin.
* Collecting it means holding a leveraged position through those hours, so price
  movement dominates the funding earned unless the position is delta-neutral — which
  needs a second venue and more capital.

**Conclusion.** The model is not wrong everywhere; it was pointed at the wrong clock.
Where crowding genuinely closes a window, that window closes in hours, and capturing it
is a high-frequency, capital-intensive game rather than a passive one.

Both tests ship as tools — `hl cohort` and the persistence estimator — so any new market
can be asked the same question before a strategy is built on it. That is the sequence
that was missing: test the premise, then build.

---

## The turn: from extracting markets to serving one

Eight extraction theses died to one law: *a market with free entry has no unpriced
returns left in it.* By this point that was not a surprise, it was arithmetic — chasing
it further would have been the same mistake in a new costume.

So the frame changed. The only durable edges are a non-replicable input you own (none
available here) or **skilled work applied where others will not bother**. Everything
this project can do — surface, rank, measure crowding — is worth more pointed at the
second than at hunting a free lunch that cannot exist.

**Authorized security-audit contests** (`audit:` niches, Cantina + Sherlock) are the one
seam found all session that is *non-rival*. A contest is a public, explicit invitation
to review a named scope for a prize pool. Two reviewers who find two different bugs are
both paid — crowding does not deplete the reward, only the stock of unfound bugs, which
the platforms publish as a running findings count. So the crowding meter maps on for
real rather than by analogy: prize is reward, findings-so-far is the crowd, the deadline
is a hard close.

Three things keep it honest, and they are the whole point:

* **It is legitimate by construction.** Every niche is a published invitation. The tool
  ranks *where* review time pays; it never fetches contract source, scans for
  vulnerabilities, or submits anything. Private and invite-only contests are excluded
  because they are not open invitations.
* **It does not pretend to be passive.** The edge is skilled human work — finding a
  valid, unique bug in real code. Entry cost is recorded as days of labour, and the
  output says so in as many words. This is the direction chosen deliberately after
  accepting that no zero-work version survives.
* **`$/finding` is contest crowding, not expected payout.** It says a $2M pot already
  split across 300 findings is a worse place to look than a fresh $200k one. It says
  nothing about whether *you* can find a bug — that is the actual work, and the tool is
  honest that it cannot do it for you.

This is not a gold mine that pays while you sleep. It is a map of where the ground is
least dug, handed to someone willing to dig. That is the only shape of durable edge the
whole session found, and it is the true one.
