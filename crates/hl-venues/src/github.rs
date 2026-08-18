//! Crowding observations from GitHub's public API.
//!
//! Free and unauthenticated, which caps us at 60 requests an hour — so this is
//! deliberately built around *one* request per niche: a single issue listing carries
//! enough to measure how fast work is being taken and how many people are chasing it.
//!
//! Honest about its proxies. The list endpoint does not expose time-to-first-response,
//! so claim latency here is time-from-open-to-close, and competitor count is the
//! comment count. Both track the real quantity well enough to fit a trend, and both are
//! labelled as proxies wherever they surface.

use crate::http::Transport;
use crate::timefmt::parse_rfc3339_utc;
use anyhow::{Context, Result};
use hl_core::{EntryCost, Niche, NicheClass, Observation, Source};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct GithubNiche {
    pub owner: String,
    pub repo: String,
    pub label: String,
}

impl GithubNiche {
    pub fn new(owner: &str, repo: &str, label: &str) -> Self {
        Self {
            owner: owner.into(),
            repo: repo.into(),
            label: label.into(),
        }
    }
    pub fn id(&self) -> String {
        format!("github:{}/{}:{}", self.owner, self.repo, self.label)
    }
    pub fn url(&self) -> String {
        format!(
            "https://api.github.com/repos/{}/{}/issues?labels={}&state=closed&sort=created&direction=desc&per_page=100",
            self.owner, self.repo, self.label
        )
    }
}

pub struct GithubSource {
    pub niches: Vec<GithubNiche>,
    transport: Box<dyn Transport>,
    id: String,
}

impl GithubSource {
    pub fn new(niches: Vec<GithubNiche>, transport: Box<dyn Transport>) -> Self {
        Self {
            niches,
            transport,
            id: "github".into(),
        }
    }

    /// Requests this source will make for one full poll. The governor needs this before
    /// the calls happen, not after.
    pub fn request_cost(&self) -> u32 {
        self.niches.len() as u32
    }
}

#[derive(Debug, Deserialize)]
struct GhIssue {
    #[serde(default)]
    title: String,
    created_at: String,
    #[serde(default)]
    closed_at: Option<String>,
    #[serde(default)]
    comments: u32,
    #[serde(default)]
    labels: Vec<GhLabel>,
    /// Present when the "issue" is actually a pull request. Excluded: a PR's lifetime
    /// measures review speed, not how fast work is claimed.
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GhLabel {
    #[serde(default)]
    name: String,
}

impl Source for GithubSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn niches(&self) -> Result<Vec<Niche>> {
        Ok(self
            .niches
            .iter()
            .map(|n| Niche {
                id: n.id(),
                label: format!("{}/{} [{}]", n.owner, n.repo, n.label),
                class: NicheClass::WorkMarket,
                opened_ms: None,
                first_seen_ms: hl_core::now_millis(),
                entry_cost: EntryCost {
                    money_cents: 0,
                    requests: 1,
                    seconds: 60,
                },
                source_url: Some(format!("https://github.com/{}/{}", n.owner, n.repo)),
                notes: "claim latency is a proxy: time from issue open to close".into(),
            })
            .collect())
    }

    fn observe(&self, since_ms: u64) -> Result<Vec<Observation>> {
        let mut out = Vec::new();
        for n in &self.niches {
            let token = auth_token();
            let headers = github_headers(token.as_deref());
            let borrowed: Vec<(&str, &str)> =
                headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let resp = self.transport.get(&n.url(), &borrowed)?;
            // 403 is overloaded: it means "slow down" and "you may not read this", and
            // conflating them sends you tuning rate limits when the real problem is a
            // token scoped to other repositories. The body says which.
            if resp.status != 200 {
                anyhow::bail!("{}", describe_failure(resp.status, &resp.body, &n.id()));
            }
            out.extend(parse_issues(&resp.body, &n.id(), self.id(), since_ms)?);
        }
        Ok(out)
    }
}

/// GitHub credentials from the environment, if any.
///
/// Unauthenticated search allows 10 requests a minute against a shared runner IP, which
/// in practice means 403s; the token a CI job is handed raises that to 1,000 an hour.
/// Absent or placeholder values yield `None` so we send no header at all rather than a
/// broken one.
pub fn auth_token() -> Option<String> {
    for key in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_string();
            if !v.is_empty() && v != "proxy-injected" {
                return Some(v);
            }
        }
    }
    None
}

/// Request headers for the GitHub API, with auth when available.
pub fn github_headers(token: Option<&str>) -> Vec<(String, String)> {
    let mut h = vec![
        ("Accept".to_string(), "application/vnd.github+json".to_string()),
        ("X-GitHub-Api-Version".to_string(), "2022-11-28".to_string()),
    ];
    if let Some(t) = token {
        h.push(("Authorization".to_string(), format!("Bearer {t}")));
    }
    h
}

/// Explain a non-200 in terms of what to do about it.
pub fn describe_failure(status: u16, body: &str, niche_id: &str) -> String {
    let lower = body.to_ascii_lowercase();
    let rate_limited = status == 429
        || lower.contains("rate limit")
        || lower.contains("abuse detection")
        || lower.contains("secondary rate");
    match status {
        _ if rate_limited => format!(
            "{niche_id}: rate limited (status {status}); back off rather than retrying"
        ),
        403 | 404 => format!(
            "{niche_id}: not readable with the current credentials (status {status}); \
             the token is scoped elsewhere, or the repository is private"
        ),
        _ => format!("{niche_id}: github returned status {status}"),
    }
}

/// Turn an issue listing into observations. Split out so it can be tested against
/// recorded payloads without a network.
pub fn parse_issues(
    body: &str,
    niche_id: &str,
    source: &str,
    since_ms: u64,
) -> Result<Vec<Observation>> {
    let issues: Vec<GhIssue> = serde_json::from_str(body).context("parsing github issue list")?;
    let mut out = Vec::new();
    for i in issues {
        if i.pull_request.is_some() {
            continue;
        }
        let Some(created) = parse_rfc3339_utc(&i.created_at) else {
            continue;
        };
        let Some(closed) = i.closed_at.as_deref().and_then(parse_rfc3339_utc) else {
            continue;
        };
        if closed < since_ms || closed < created {
            continue;
        }
        // The observation is timestamped when the work was *taken*, not when the issue
        // opened: the trend we are fitting is about the present, not about a backlog.
        let mut o = Observation::new(niche_id, closed, source)
            .claim_latency((closed - created).max(1));
        if i.comments > 0 {
            o = o.competitors(i.comments as f64);
        }
        let text = format!(
            "{} {}",
            i.title,
            i.labels.iter().map(|l| l.name.as_str()).collect::<Vec<_>>().join(" ")
        );
        if let Some(cents) = parse_bounty_cents(&text) {
            o = o.reward(cents);
        }
        out.push(o);
    }
    Ok(out)
}

/// Pull a dollar amount out of free text, e.g. `"💰 $250 bounty"` or `"$1,500"`.
///
/// Takes the largest match: bounty titles often carry several numbers and the reward is
/// reliably the biggest of them.
pub fn parse_bounty_cents(text: &str) -> Option<u64> {
    let b = text.as_bytes();
    let mut best: Option<u64> = None;
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'$' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        let mut whole = String::new();
        let mut cents = String::new();
        let mut in_cents = false;
        while j < b.len() {
            match b[j] {
                c if c.is_ascii_digit() => {
                    if in_cents {
                        if cents.len() < 2 {
                            cents.push(c as char);
                        }
                    } else {
                        whole.push(c as char);
                    }
                }
                b',' if !in_cents => {}
                b'.' if !in_cents => in_cents = true,
                b'k' | b'K' if !whole.is_empty() && !in_cents => {
                    whole.push_str("000");
                    j += 1;
                    break;
                }
                _ => break,
            }
            j += 1;
        }
        if !whole.is_empty() {
            if let Ok(w) = whole.parse::<u64>() {
                while cents.len() < 2 {
                    cents.push('0');
                }
                let total = w.saturating_mul(100) + cents.parse::<u64>().unwrap_or(0);
                best = Some(best.map_or(total, |b| b.max(total)));
            }
        }
        i = j.max(i + 1);
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::FixtureTransport;

    const PAYLOAD: &str = r#"[
      {"title":"Fix parser $500 bounty","created_at":"2026-08-01T00:00:00Z","closed_at":"2026-08-01T02:00:00Z","comments":3,"labels":[{"name":"bounty"}]},
      {"title":"Add retries","created_at":"2026-08-02T00:00:00Z","closed_at":"2026-08-02T01:00:00Z","comments":7,"labels":[]},
      {"title":"A pull request","created_at":"2026-08-03T00:00:00Z","closed_at":"2026-08-03T05:00:00Z","comments":1,"labels":[],"pull_request":{"url":"x"}},
      {"title":"Still open","created_at":"2026-08-04T00:00:00Z","closed_at":null,"comments":0,"labels":[]}
    ]"#;

    #[test]
    fn parses_real_shaped_payload() {
        let obs = parse_issues(PAYLOAD, "n", "github", 0).unwrap();
        assert_eq!(obs.len(), 2, "pull requests and open issues must be excluded");
        assert_eq!(obs[0].claim_latency_ms, Some(2 * 3_600_000));
        assert_eq!(obs[0].reward_cents, Some(50_000));
        assert_eq!(obs[0].competitors, Some(3.0));
        assert_eq!(obs[1].reward_cents, None);
    }

    #[test]
    fn observations_are_stamped_when_work_was_taken() {
        let obs = parse_issues(PAYLOAD, "n", "github", 0).unwrap();
        assert_eq!(obs[0].ts_ms, parse_rfc3339_utc("2026-08-01T02:00:00Z").unwrap());
    }

    #[test]
    fn since_filter_excludes_old_work() {
        let cut = parse_rfc3339_utc("2026-08-02T00:00:00Z").unwrap();
        let obs = parse_issues(PAYLOAD, "n", "github", cut).unwrap();
        assert_eq!(obs.len(), 1);
    }

    #[test]
    fn placeholder_tokens_are_not_sent_as_credentials() {
        std::env::set_var("GITHUB_TOKEN", "proxy-injected");
        assert_eq!(auth_token(), None, "a placeholder must not become a header");
        std::env::set_var("GITHUB_TOKEN", "  ");
        assert_eq!(auth_token(), None);
        std::env::set_var("GITHUB_TOKEN", "ghs_real");
        assert_eq!(auth_token().as_deref(), Some("ghs_real"));
        std::env::remove_var("GITHUB_TOKEN");
    }

    #[test]
    fn headers_carry_auth_only_when_there_is_a_token() {
        let none = github_headers(None);
        assert!(!none.iter().any(|(k, _)| k == "Authorization"));
        let some = github_headers(Some("t"));
        assert!(some.iter().any(|(k, v)| k == "Authorization" && v == "Bearer t"));
        assert!(some.iter().any(|(k, _)| k == "X-GitHub-Api-Version"));
    }

    #[test]
    fn bounty_amounts_are_extracted_from_messy_text() {
        assert_eq!(parse_bounty_cents("no money here"), None);
        assert_eq!(parse_bounty_cents("$250"), Some(25_000));
        assert_eq!(parse_bounty_cents("pays $1,500 today"), Some(150_000));
        assert_eq!(parse_bounty_cents("$12.34"), Some(1_234));
        assert_eq!(parse_bounty_cents("$2k reward"), Some(200_000));
        // Largest wins: titles carry issue numbers and version strings too.
        assert_eq!(parse_bounty_cents("$5 tip, $900 bounty"), Some(90_000));
        assert_eq!(parse_bounty_cents("$"), None);
    }

    #[test]
    fn rate_limiting_is_an_error_not_a_silent_gap() {
        let n = GithubNiche::new("o", "r", "bounty");
        let t = FixtureTransport::new()
            .with(n.url(), 403, r#"{"message":"API rate limit exceeded"}"#);
        let src = GithubSource::new(vec![n], Box::new(t));
        let err = src.observe(0).unwrap_err().to_string();
        assert!(err.contains("rate limited"), "got: {err}");
    }

    #[test]
    fn a_scoped_out_repository_is_not_reported_as_rate_limiting() {
        // Both arrive as 403. Confusing them sends you tuning poll intervals when the
        // actual problem is credentials.
        let msg = describe_failure(403, r#"{"message":"Resource not accessible"}"#, "n");
        assert!(msg.contains("credentials"), "got: {msg}");
        assert!(!msg.contains("rate limited"));

        let msg = describe_failure(403, r#"{"message":"API rate limit exceeded"}"#, "n");
        assert!(msg.contains("rate limited"), "got: {msg}");

        assert!(describe_failure(500, "boom", "n").contains("500"));
    }

    #[test]
    fn a_successful_poll_costs_one_request_per_niche() {
        let a = GithubNiche::new("o", "r", "bounty");
        let b = GithubNiche::new("o", "r2", "bounty");
        let t = FixtureTransport::new()
            .with(a.url(), 200, PAYLOAD)
            .with(b.url(), 200, "[]");
        let src = GithubSource::new(vec![a, b], Box::new(t));
        assert_eq!(src.request_cost(), 2);
        assert_eq!(src.observe(0).unwrap().len(), 2);
    }
}
