//! What the sweep watches.
//!
//! Defaults are compiled in so a fresh checkout does something useful immediately, and
//! `halflife.toml` overrides them so changing the watchlist never needs a rebuild.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Hugging Face tags to measure saturation for.
    pub hf_model_tags: Vec<String>,
    pub hf_dataset_tags: Vec<String>,
    /// GitHub search queries, as `name = query`.
    pub github_searches: Vec<[String; 2]>,
    /// Specific repositories to watch, as `owner/name:label`.
    pub github_repos: Vec<String>,
    /// Observations older than this are pruned on each sweep.
    pub retain_days: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // A spread across the saturation range on purpose: `text-generation` is
            // known-dead and acts as a control, so if the meter ever reports it as open
            // the meter is wrong.
            hf_model_tags: vec![
                "text-generation".into(),
                "gguf".into(),
                "robotics".into(),
                "time-series-forecasting".into(),
                "reinforcement-learning".into(),
                "any-to-any".into(),
            ],
            hf_dataset_tags: vec!["reinforcement-learning".into(), "robotics".into()],
            github_searches: vec![
                ["bounty".into(), "label:bounty state:closed".into()],
                [
                    "paid-issue".into(),
                    "label:\"💎 Bounty\" state:closed".into(),
                ],
            ],
            github_repos: vec![],
            retain_days: 120.0,
        }
    }
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Config> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Total requests one full sweep will make. Checked against quota before any call.
    pub fn request_cost(&self) -> u32 {
        (self.hf_model_tags.len()
            + self.hf_dataset_tags.len()
            + self.github_searches.len()
            + self.github_repos.len()) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_usable_without_a_config_file() {
        let c = Config::load("/nonexistent/halflife.toml").unwrap();
        assert!(!c.hf_model_tags.is_empty());
        assert!(c.request_cost() > 0);
    }

    #[test]
    fn request_cost_counts_every_source() {
        let c = Config {
            hf_model_tags: vec!["a".into(), "b".into()],
            hf_dataset_tags: vec!["c".into()],
            github_searches: vec![["n".into(), "q".into()]],
            github_repos: vec!["o/r:bounty".into()],
            retain_days: 1.0,
        };
        assert_eq!(c.request_cost(), 5);
    }

    #[test]
    fn a_partial_config_file_keeps_the_other_defaults() {
        let dir = std::env::temp_dir().join(format!("hl-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("halflife.toml");
        std::fs::write(&p, "hf_model_tags = [\"only-this\"]\n").unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.hf_model_tags, vec!["only-this".to_string()]);
        assert!(!c.github_searches.is_empty(), "unspecified keys keep defaults");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
