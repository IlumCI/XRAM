//! Cross-repository discovery via GitHub's search API.
//!
//! The repo-watchlist source can only measure niches we already knew to name. This one
//! finds them: one search across all public repositories for work carrying a bounty
//! label, which is how a window somewhere unfamiliar gets noticed at all.
//!
//! Search is rate-limited far more tightly than the core API — 30 requests/minute
//! authenticated, 10 unauthenticated — so each niche is deliberately one query, and the
//! governor is configured for the search bucket rather than the core one.

use crate::github::{auth_token, describe_failure, github_headers, parse_issues_by_repo};
use crate::http::Transport;
use anyhow::{Context, Result};
use hl_core::{EntryCost, Niche, NicheClass, Observation, Source};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct GithubSearchNiche {
    /// Short name for the niche, used as its id.
    pub name: String,
    /// A GitHub search query, e.g. `label:bounty state:closed`.
    pub query: String,
}

impl GithubSearchNiche {
    pub fn new(name: &str, query: &str) -> Self {
        Self {
            name: name.into(),
            query: query.into(),
        }
    }

    /// The default sweep: work that carried a bounty label and has been closed, which
    /// is the population whose claim latency we can actually measure.
    pub fn bounties() -> Self {
        Self::new("bounty-labelled", "label:bounty state:closed")
    }

    pub fn id(&self) -> String {
        format!("gh-search:{}", self.name)
    }

    pub fn url(&self) -> String {
        format!(
            "https://api.github.com/search/issues?q={}&sort=updated&order=desc&per_page=100",
            urlencode(&self.query)
        )
    }
}

/// Percent-encode a query. Small and explicit rather than pulling in a URL crate for
/// one call site.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub struct GithubSearchSource {
    pub niches: Vec<GithubSearchNiche>,
    transport: Box<dyn Transport>,
    id: String,
}

impl GithubSearchSource {
    pub fn new(niches: Vec<GithubSearchNiche>, transport: Box<dyn Transport>) -> Self {
        Self {
            niches,
            transport,
            id: "github-search".into(),
        }
    }
    pub fn request_cost(&self) -> u32 {
        self.niches.len() as u32
    }
}

#[derive(Debug, Deserialize)]
struct SearchEnvelope {
    #[serde(default)]
    total_count: u64,
    #[serde(default)]
    items: serde_json::Value,
}

impl Source for GithubSearchSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn niches(&self) -> Result<Vec<Niche>> {
        Ok(self
            .niches
            .iter()
            .map(|n| Niche {
                id: n.id(),
                label: format!("search: {}", n.query),
                class: NicheClass::WorkMarket,
                opened_ms: None,
                first_seen_ms: hl_core::now_millis(),
                entry_cost: EntryCost {
                    money_cents: 0,
                    requests: 1,
                    seconds: 60,
                },
                closes_ms: None,
                source_url: Some("https://github.com/search".into()),
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
            if resp.status != 200 {
                anyhow::bail!("{}", describe_failure(resp.status, &resp.body, &n.id()));
            }
            out.extend(parse_search(&resp.body, &n.id(), self.id(), since_ms)?.1);
        }
        Ok(out)
    }
}

/// Unwrap the search envelope and reuse the issue parser on its items.
///
/// Returns the reported total alongside the observations: the total is a coarse but
/// genuine measure of how much work of this kind exists at all.
pub fn parse_search(
    body: &str,
    niche_id: &str,
    source: &str,
    since_ms: u64,
) -> Result<(u64, Vec<Observation>)> {
    let env: SearchEnvelope =
        serde_json::from_str(body).context("parsing github search envelope")?;
    let items = serde_json::to_string(&env.items).unwrap_or_else(|_| "[]".into());
    // `niche_id` names the *search*, not a market; each result is attributed to the
    // repository it came from.
    let _ = niche_id;
    let obs = parse_issues_by_repo(&items, source, since_ms)?;
    Ok((env.total_count, obs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::FixtureTransport;

    const ENVELOPE: &str = r#"{"total_count":4210,"items":[
      {"repository_url":"https://api.github.com/repos/acme/widgets","title":"Fix parser $500","created_at":"2026-08-01T00:00:00Z","closed_at":"2026-08-01T02:00:00Z","comments":3,"labels":[{"name":"bounty"}]},
      {"repository_url":"https://api.github.com/repos/other/thing","title":"Ship it $250","created_at":"2026-08-01T00:00:00Z","closed_at":"2026-08-01T06:00:00Z","comments":1,"labels":[]},
      {"repository_url":"https://api.github.com/repos/spam/bot","title":"auto $10","created_at":"2026-08-02T00:00:00Z","closed_at":"2026-08-02T00:02:00Z","comments":0,"labels":[]},
      {"repository_url":"https://api.github.com/repos/acme/widgets","title":"A PR","created_at":"2026-08-02T00:00:00Z","closed_at":"2026-08-02T01:00:00Z","comments":1,"labels":[],"pull_request":{"url":"x"}},
      {"repository_url":"https://api.github.com/repos/acme/widgets","title":"Still open","created_at":"2026-08-03T00:00:00Z","closed_at":null,"comments":0,"labels":[]}
    ]}"#;

    #[test]
    fn results_are_split_into_one_niche_per_repository() {
        // "Every bounty-labelled issue on GitHub" is a mixture of unrelated markets, and
        // fitting one trend across it is meaningless.
        let (total, obs) = parse_search(ENVELOPE, "search-name", "gh", 0).unwrap();
        assert_eq!(total, 4210);
        let ids: Vec<&str> = obs.iter().map(|o| o.niche_id.as_str()).collect();
        assert!(ids.contains(&"gh:acme/widgets"));
        assert!(ids.contains(&"gh:other/thing"));
        assert!(
            !ids.iter().any(|i| i.contains("search-name")),
            "the search is not itself a market"
        );
    }

    #[test]
    fn automated_open_and_close_cycles_are_dropped() {
        let (_, obs) = parse_search(ENVELOPE, "n", "gh", 0).unwrap();
        assert!(
            !obs.iter().any(|o| o.niche_id == "gh:spam/bot"),
            "a two-minute open-close cycle is a bot, not a claimed bounty"
        );
        assert_eq!(obs.len(), 2, "PRs, open issues and bot cycles all excluded");
    }

    #[test]
    fn an_empty_result_is_not_an_error() {
        let (total, obs) = parse_search(r#"{"total_count":0,"items":[]}"#, "n", "gh", 0).unwrap();
        assert_eq!(total, 0);
        assert!(obs.is_empty());
    }

    #[test]
    fn queries_are_encoded_safely() {
        let n = GithubSearchNiche::bounties();
        assert!(n.url().contains("label:bounty+state:closed"));
        let odd = GithubSearchNiche::new("x", "label:\"help wanted\" org:foo/bar");
        let u = odd.url();
        assert!(!u.contains('"'), "raw quotes would break the request: {u}");
        assert!(u.contains("%22"));
    }

    #[test]
    fn a_scoped_out_token_is_reported_as_credentials_not_rate_limiting() {
        let n = GithubSearchNiche::bounties();
        let t = FixtureTransport::new()
            .with(n.url(), 403, r#"{"message":"Resource not accessible"}"#);
        let src = GithubSearchSource::new(vec![n], Box::new(t));
        let err = src.observe(0).unwrap_err().to_string();
        assert!(err.contains("credentials"), "got: {err}");
    }

    #[test]
    fn one_query_per_niche() {
        let n = GithubSearchNiche::bounties();
        let t = FixtureTransport::new().with(n.url(), 200, ENVELOPE);
        let src = GithubSearchSource::new(vec![n], Box::new(t));
        assert_eq!(src.request_cost(), 1);
        // Two usable issues from two repositories; one query, several niches.
        assert_eq!(src.observe(0).unwrap().len(), 2);
    }
}
