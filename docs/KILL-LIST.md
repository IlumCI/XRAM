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
