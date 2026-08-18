//! A simulated venue with known ground truth.
//!
//! The crowding meter makes a falsifiable claim — "this niche halves every N days" — so
//! it needs a source where N is known in advance. Everything the meter reports about a
//! [`SimSource`] can be checked against the number that generated the data.

use hl_core::{EntryCost, Niche, NicheClass, Observation, Rng, Source, MS_PER_DAY};

/// Ground truth for one simulated niche.
#[derive(Debug, Clone)]
pub struct SimNiche {
    pub id: String,
    /// Days for reward to halve. `None` means a stable niche.
    pub reward_half_life_days: Option<f64>,
    /// Days for claim latency to halve, i.e. how fast competitors are arriving.
    pub latency_half_life_days: Option<f64>,
    pub initial_reward_cents: u64,
    pub initial_latency_ms: u64,
    /// Multiplicative noise, as a fraction. 0.4 means values wander +/-20%.
    pub noise: f64,
    pub opened_ms: u64,
}

impl SimNiche {
    pub fn collapsing(id: &str, half_life_days: f64) -> Self {
        Self {
            id: id.into(),
            reward_half_life_days: Some(half_life_days),
            latency_half_life_days: Some(half_life_days),
            initial_reward_cents: 10_000,
            initial_latency_ms: 6 * 60 * 60 * 1000,
            noise: 0.3,
            opened_ms: 0,
        }
    }
    pub fn stable(id: &str) -> Self {
        Self {
            id: id.into(),
            reward_half_life_days: None,
            latency_half_life_days: None,
            initial_reward_cents: 5_000,
            initial_latency_ms: 12 * 60 * 60 * 1000,
            noise: 0.3,
            opened_ms: 0,
        }
    }
    pub fn noise(mut self, noise: f64) -> Self {
        self.noise = noise;
        self
    }
}

pub struct SimSource {
    pub id: String,
    pub niches: Vec<SimNiche>,
    /// Observations generated per niche per day.
    pub samples_per_day: usize,
    pub horizon_days: f64,
    pub seed: u64,
}

impl SimSource {
    pub fn new(niches: Vec<SimNiche>, horizon_days: f64) -> Self {
        Self {
            id: "sim".into(),
            niches,
            samples_per_day: 4,
            horizon_days,
            seed: 0xC0FFEE,
        }
    }

    fn decay(half_life: Option<f64>, t_days: f64) -> f64 {
        match half_life {
            Some(hl) if hl > 0.0 => 0.5_f64.powf(t_days / hl),
            _ => 1.0,
        }
    }
}

impl Source for SimSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn niches(&self) -> anyhow::Result<Vec<Niche>> {
        Ok(self
            .niches
            .iter()
            .map(|n| Niche {
                id: n.id.clone(),
                label: format!("simulated niche {}", n.id),
                class: NicheClass::WorkMarket,
                opened_ms: Some(n.opened_ms),
                first_seen_ms: n.opened_ms,
                entry_cost: EntryCost {
                    money_cents: 0,
                    requests: 1,
                    seconds: 30,
                },
                source_url: None,
                notes: match n.reward_half_life_days {
                    Some(hl) => format!("ground truth: reward half-life {hl} days"),
                    None => "ground truth: stable".into(),
                },
            })
            .collect())
    }

    fn observe(&self, since_ms: u64) -> anyhow::Result<Vec<Observation>> {
        let mut out = Vec::new();
        let total = (self.horizon_days * self.samples_per_day as f64) as usize;
        for n in &self.niches {
            // Seeding per niche keeps each stream reproducible and independent.
            let mut rng = Rng::from_key(&format!("{}:{}", self.seed, n.id));
            for i in 0..total {
                let t = i as f64 / self.samples_per_day as f64;
                let ts_ms = n.opened_ms + (t * MS_PER_DAY) as u64;
                if ts_ms < since_ms {
                    // Still advance the generator so a resumed poll sees the same
                    // series it would have seen from the start.
                    let _ = rng.unit();
                    let _ = rng.unit();
                    continue;
                }
                let jitter = |r: &mut Rng| 1.0 + n.noise * (r.unit() - 0.5);
                let reward = n.initial_reward_cents as f64
                    * Self::decay(n.reward_half_life_days, t)
                    * jitter(&mut rng);
                let latency = n.initial_latency_ms as f64
                    * Self::decay(n.latency_half_life_days, t)
                    * jitter(&mut rng);
                out.push(
                    Observation::new(&n.id, ts_ms, &self.id)
                        .reward(reward.max(1.0) as u64)
                        .claim_latency(latency.max(1.0) as u64),
                );
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_probe::{crowding::CrowdingMeter, policy, PolicyConfig};
    use hl_core::Signal;

    #[test]
    fn the_meter_recovers_the_half_life_it_was_given() {
        for &hl in &[2.0, 7.0, 21.0] {
            let src = SimSource::new(vec![SimNiche::collapsing("n", hl)], hl * 3.0);
            let obs = src.observe(0).unwrap();
            let now = obs.iter().map(|o| o.ts_ms).max().unwrap();
            let r = CrowdingMeter::default().report("n", &obs, now);
            let got = r.half_life_days().expect("collapse must be detected");
            assert!(
                (got - hl).abs() / hl < 0.15,
                "true half-life {hl}d, measured {got:.2}d"
            );
        }
    }

    #[test]
    fn a_stable_simulated_niche_is_not_reported_as_closing() {
        let src = SimSource::new(vec![SimNiche::stable("s")], 14.0);
        let obs = src.observe(0).unwrap();
        let now = obs.iter().map(|o| o.ts_ms).max().unwrap();
        let r = CrowdingMeter::default().report("s", &obs, now);
        let d = policy::decide(&r, &EntryCost::default(), &PolicyConfig::default());
        assert_ne!(d.signal, Signal::Exit, "reason: {}", d.reason);
    }

    #[test]
    fn a_fast_collapse_produces_an_exit_and_a_slow_one_does_not() {
        let cfg = PolicyConfig::default();
        let fast = SimSource::new(vec![SimNiche::collapsing("f", 1.5)], 6.0);
        let slow = SimSource::new(vec![SimNiche::collapsing("s", 40.0)], 30.0);

        let fo = fast.observe(0).unwrap();
        let fr = CrowdingMeter::default().report("f", &fo, fo.iter().map(|o| o.ts_ms).max().unwrap());
        assert_eq!(policy::decide(&fr, &EntryCost::default(), &cfg).signal, Signal::Exit);

        let so = slow.observe(0).unwrap();
        let sr = CrowdingMeter::default().report("s", &so, so.iter().map(|o| o.ts_ms).max().unwrap());
        let d = policy::decide(&sr, &EntryCost::default(), &cfg);
        assert_eq!(d.signal, Signal::Enter, "reason: {}", d.reason);
    }

    #[test]
    fn heavy_noise_is_reported_as_undetermined_rather_than_guessed() {
        let src = SimSource::new(
            vec![SimNiche::collapsing("n", 30.0).noise(1.9)],
            4.0,
        );
        let obs = src.observe(0).unwrap();
        let now = obs.iter().map(|o| o.ts_ms).max().unwrap();
        let r = CrowdingMeter::default().report("n", &obs, now);
        let d = policy::decide(&r, &EntryCost::default(), &PolicyConfig::default());
        assert_eq!(d.signal, Signal::Insufficient, "reason: {}", d.reason);
    }

    #[test]
    fn resuming_from_a_timestamp_yields_the_same_series() {
        let src = SimSource::new(vec![SimNiche::collapsing("n", 5.0)], 10.0);
        let all = src.observe(0).unwrap();
        let cut = all[all.len() / 2].ts_ms;
        let resumed = src.observe(cut).unwrap();
        let expected: Vec<_> = all.iter().filter(|o| o.ts_ms >= cut).collect();
        assert_eq!(resumed.len(), expected.len());
        assert_eq!(resumed[0].reward_cents, expected[0].reward_cents);
    }
}
