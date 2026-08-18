//! Fetch verified contract source from a public block explorer.
//!
//! Only *verified, published* source is fetched, which is inherently public — the same
//! code the explorer already renders to anyone. Nothing here interacts with a live
//! contract; it reads source the way a human opens the "Contract" tab.

use anyhow::{bail, Context, Result};
use hl_venues::http::Transport;
use serde::Deserialize;

/// Public Blockscout instances, keyed by a short chain name. All free, no API key.
pub fn explorer_base(chain: &str) -> Option<&'static str> {
    Some(match chain.to_ascii_lowercase().as_str() {
        "eth" | "ethereum" | "mainnet" => "https://eth.blockscout.com",
        "base" => "https://base.blockscout.com",
        "arbitrum" | "arb" => "https://arbitrum.blockscout.com",
        "optimism" | "op" => "https://optimism.blockscout.com",
        "polygon" | "matic" => "https://polygon.blockscout.com",
        "gnosis" => "https://gnosis.blockscout.com",
        _ => return None,
    })
}

pub fn supported_chains() -> &'static [&'static str] {
    &["eth", "base", "arbitrum", "optimism", "polygon", "gnosis"]
}

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub path: String,
    pub code: String,
}

#[derive(Debug, Clone)]
pub struct ContractSources {
    pub name: String,
    pub chain: String,
    pub address: String,
    pub verified: bool,
    pub compiler: Option<String>,
    pub files: Vec<SourceFile>,
}

impl ContractSources {
    pub fn total_lines(&self) -> usize {
        self.files.iter().map(|f| f.code.lines().count()).sum()
    }
}

#[derive(Deserialize)]
struct BsResponse {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    is_verified: bool,
    #[serde(default)]
    source_code: Option<String>,
    #[serde(default)]
    compiler_version: Option<String>,
    #[serde(default)]
    additional_sources: Vec<BsAdditional>,
    #[serde(default)]
    file_path: Option<String>,
}

#[derive(Deserialize)]
struct BsAdditional {
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    source_code: Option<String>,
}

pub fn contract_url(base: &str, address: &str) -> String {
    format!("{base}/api/v2/smart-contracts/{address}")
}

/// Fetch and normalise verified source for `address` on `chain`.
pub fn fetch_sources(
    transport: &dyn Transport,
    chain: &str,
    address: &str,
) -> Result<ContractSources> {
    let base = explorer_base(chain)
        .with_context(|| format!("unsupported chain '{chain}'; try one of {:?}", supported_chains()))?;
    let resp = transport.get(
        &contract_url(base, address),
        &[("Accept", "application/json"), ("User-Agent", "Mozilla/5.0")],
    )?;
    if resp.status == 404 {
        bail!("no contract at {address} on {chain} (or the explorer has never seen it)");
    }
    if resp.status != 200 {
        bail!("explorer returned status {} for {address}", resp.status);
    }
    parse_contract(&resp.body, chain, address)
}

pub fn parse_contract(body: &str, chain: &str, address: &str) -> Result<ContractSources> {
    let r: BsResponse = serde_json::from_str(body).context("parsing explorer response")?;
    if !r.is_verified {
        bail!("{address} on {chain} is not verified; there is no published source to review");
    }
    let mut files = Vec::new();
    if let Some(code) = r.source_code.filter(|c| !c.trim().is_empty()) {
        files.push(SourceFile {
            path: r.file_path.clone().unwrap_or_else(|| "main.sol".into()),
            code,
        });
    }
    for a in r.additional_sources {
        if let Some(code) = a.source_code.filter(|c| !c.trim().is_empty()) {
            files.push(SourceFile {
                path: a.file_path.unwrap_or_else(|| "unnamed.sol".into()),
                code,
            });
        }
    }
    if files.is_empty() {
        bail!("{address} reports as verified but returned no source files");
    }
    Ok(ContractSources {
        name: r.name.unwrap_or_else(|| "unknown".into()),
        chain: chain.to_string(),
        address: address.to_string(),
        verified: true,
        compiler: r.compiler_version,
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_chains_resolve_and_unknown_ones_do_not() {
        assert!(explorer_base("eth").is_some());
        assert!(explorer_base("Base").is_some());
        assert!(explorer_base("dogecoin").is_none());
    }

    #[test]
    fn a_verified_contract_yields_all_its_files() {
        let body = r#"{"name":"Token","is_verified":true,"compiler_version":"0.8.20",
          "source_code":"contract Token {}","file_path":"Token.sol",
          "additional_sources":[{"file_path":"Lib.sol","source_code":"library Lib {}"}]}"#;
        let c = parse_contract(body, "eth", "0xabc").unwrap();
        assert_eq!(c.name, "Token");
        assert_eq!(c.files.len(), 2);
        assert!(c.verified);
    }

    #[test]
    fn unverified_source_is_refused_with_a_clear_reason() {
        let body = r#"{"name":"X","is_verified":false}"#;
        let e = parse_contract(body, "eth", "0xabc").unwrap_err().to_string();
        assert!(e.contains("not verified"), "got: {e}");
    }

    #[test]
    fn verified_but_empty_is_an_error_not_an_empty_review() {
        let body = r#"{"name":"X","is_verified":true,"source_code":"  "}"#;
        assert!(parse_contract(body, "eth", "0xabc").is_err());
    }
}
