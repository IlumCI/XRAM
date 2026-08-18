//! Minimal HTTP transport.
//!
//! Behind a trait for one reason: every source must be testable without a network, so
//! the test suite exercises real parsing against recorded payloads rather than mocking
//! the parser away.

use anyhow::{bail, Result};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// Cap on a response body.
///
/// `ureq::Response::into_string` refuses anything past 10 MB, which silently killed the
/// yield source: the pool listing is a single ~11 MB document. Reading through the
/// reader instead lifts that, and the cap is kept explicit so a runaway response still
/// cannot exhaust memory.
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Read a response body without `into_string`'s 10 MB ceiling.
fn read_body(resp: ureq::Response) -> Result<String> {
    use std::io::Read;
    let mut buf = Vec::with_capacity(64 * 1024);
    resp.into_reader()
        .take(MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut buf)?;
    if buf.len() > MAX_BODY_BYTES {
        bail!("response exceeds {MAX_BODY_BYTES} byte cap");
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

pub trait Transport: Send + Sync {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse>;

    /// POST a body. Some read-only APIs are POST-shaped — Hyperliquid's `info`
    /// endpoint takes a query object rather than a path — so this is still a read.
    fn post(&self, url: &str, headers: &[(&str, &str)], body: &str) -> Result<HttpResponse>;
}

pub struct UreqTransport {
    agent: ureq::Agent,
    timeout_secs: u64,
    user_agent: String,
}

/// Where to look for additional trust anchors, in order.
///
/// Egress frequently runs through a TLS-inspecting proxy whose CA is installed in the
/// environment rather than baked into any crate. A client that only trusts its own
/// bundled roots fails on every such network, so the environment's bundle is loaded on
/// top of the system store rather than instead of it.
const CA_ENV_VARS: [&str; 3] = ["SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE"];
const CA_FALLBACK_PATHS: [&str; 3] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/cert.pem",
];

fn load_roots() -> (rustls::RootCertStore, usize) {
    let mut roots = rustls::RootCertStore::empty();
    let mut paths: Vec<String> = CA_ENV_VARS
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .collect();
    paths.extend(CA_FALLBACK_PATHS.iter().map(|s| s.to_string()));

    let mut loaded = 0usize;
    for path in paths {
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let mut cursor = std::io::BufReader::new(&bytes[..]);
        for cert in rustls_pemfile::certs(&mut cursor).flatten() {
            // Duplicates across bundles are expected and harmless.
            if roots.add(cert).is_ok() {
                loaded += 1;
            }
        }
    }
    (roots, loaded)
}

fn build_agent(timeout_secs: u64) -> ureq::Agent {
    let mut b = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(timeout_secs));

    let (roots, loaded) = load_roots();
    if loaded > 0 {
        // An explicit provider, rather than the process-wide default: this crate should
        // not care whether something else installed one first.
        let cfg = rustls::ClientConfig::builder_with_provider(
            rustls::crypto::ring::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .expect("ring provider supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        b = b.tls_config(std::sync::Arc::new(cfg));
    }

    // ureq does not read proxy settings from the environment on its own.
    for key in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
        if let Ok(url) = std::env::var(key) {
            if url.is_empty() {
                continue;
            }
            if let Ok(p) = ureq::Proxy::new(&url) {
                b = b.proxy(p);
                break;
            }
        }
    }
    b.build()
}

impl Default for UreqTransport {
    fn default() -> Self {
        let timeout_secs = 15;
        Self {
            agent: build_agent(timeout_secs),
            timeout_secs,
            // Identifying the client honestly is the price of using someone's free API.
            user_agent: concat!("halflife/", env!("CARGO_PKG_VERSION"), " (crowding meter)")
                .to_string(),
        }
    }
}

impl UreqTransport {
    fn send(&self, mut req: ureq::Request, body: Option<&str>) -> Result<HttpResponse> {
        req = req.timeout(std::time::Duration::from_secs(self.timeout_secs));
        let outcome = match body {
            Some(b) => req.send_string(b),
            None => req.call(),
        };
        match outcome {
            Ok(resp) => Ok(HttpResponse {
                status: resp.status(),
                body: read_body(resp)?,
            }),
            // A 403 or 429 is data, not a failure: it is the venue telling us how
            // crowded its own rate limit is.
            Err(ureq::Error::Status(code, resp)) => Ok(HttpResponse {
                status: code,
                body: read_body(resp).unwrap_or_default(),
            }),
            Err(e) => bail!("http error for request: {e}"),
        }
    }
}

impl Transport for UreqTransport {
    fn post(&self, url: &str, headers: &[(&str, &str)], body: &str) -> Result<HttpResponse> {
        let mut req = self.agent.post(url).set("User-Agent", &self.user_agent);
        for (k, v) in headers {
            req = req.set(k, v);
        }
        self.send(req, Some(body))
    }

    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse> {
        let mut req = self
            .agent
            .get(url)
            .set("User-Agent", &self.user_agent);
        for (k, v) in headers {
            req = req.set(k, v);
        }
        self.send(req, None)
    }
}

/// Serves recorded payloads. Any unregistered URL is an error rather than an empty
/// response, so a test cannot silently pass by fetching nothing.
#[derive(Default)]
pub struct FixtureTransport {
    routes: Mutex<HashMap<String, HttpResponse>>,
    pub requests: Mutex<Vec<String>>,
}

impl FixtureTransport {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with(self, url: impl Into<String>, status: u16, body: impl Into<String>) -> Self {
        self.routes.lock().unwrap().insert(
            url.into(),
            HttpResponse {
                status,
                body: body.into(),
            },
        );
        self
    }
    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl FixtureTransport {
    fn serve(&self, url: &str) -> Result<HttpResponse> {
        self.requests.lock().unwrap().push(url.to_string());
        match self.routes.lock().unwrap().get(url) {
            Some(r) => Ok(r.clone()),
            None => bail!("no fixture registered for {url}"),
        }
    }
}

impl Transport for FixtureTransport {
    fn get(&self, url: &str, _headers: &[(&str, &str)]) -> Result<HttpResponse> {
        self.serve(url)
    }
    fn post(&self, url: &str, _headers: &[(&str, &str)], _body: &str) -> Result<HttpResponse> {
        self.serve(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_anchors_are_found_in_this_environment() {
        let (_, loaded) = load_roots();
        assert!(loaded > 0, "no CA certificates could be loaded; TLS would fail");
    }

    #[test]
    fn an_agent_builds_without_panicking() {
        let _ = UreqTransport::default();
    }

    #[test]
    fn fixtures_refuse_unregistered_urls() {
        let t = FixtureTransport::new().with("https://a/", 200, "ok");
        assert!(t.get("https://a/", &[]).is_ok());
        assert!(
            t.get("https://b/", &[]).is_err(),
            "an unstubbed request must fail loudly, not return nothing"
        );
        assert_eq!(t.request_count(), 2);
    }
}

#[cfg(test)]
mod live_probe {
    use super::*;
    /// Not run by default: hits the network. `cargo test -p hl-venues -- --ignored`
    #[test]
    #[ignore]
    fn live_github_rate_limit_endpoint() {
        let t = UreqTransport::default();
        let r = t
            .get("https://api.github.com/rate_limit", &[("Accept", "application/vnd.github+json")])
            .expect("request should complete");
        eprintln!("status={} body={}", r.status, &r.body[..r.body.len().min(300)]);
        assert_eq!(r.status, 200);
    }
}
