//! Attack-surface mapping for Solidity source.
//!
//! What this is: a first-pass map that tells a human reviewer *where to look*. It counts
//! the externally-reachable, state-changing surface, and flags well-known dangerous
//! patterns so review time goes to the right lines first.
//!
//! What this is emphatically not: a bug finder. Every flag is a place to investigate,
//! never a verdict. It does no dataflow, so it cannot tell whether a flagged pattern is
//! actually reachable or actually dangerous — and it is blind by construction to the
//! category that dominates real findings: business-logic errors, which look like
//! perfectly ordinary code. Treat a clean report as "nothing obvious in the known-footgun
//! set," never as "safe".
//!
//! The heuristics are line/token level, deliberately simple, and calibrated to
//! over-flag rather than miss: a false positive costs a human a glance, a false negative
//! costs a missed lead. Comments and string literals are stripped first so a pattern
//! named in a comment is not mistaken for the pattern itself.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// A pattern that has caused catastrophic loss when misused. Look first.
    High,
    /// A common footgun; frequently fine, occasionally fatal.
    Medium,
    /// Worth a glance in context.
    Low,
}

impl Severity {
    pub fn rank(self) -> u8 {
        match self {
            Severity::High => 3,
            Severity::Medium => 2,
            Severity::Low => 1,
        }
    }
    pub fn tag(self) -> &'static str {
        match self {
            Severity::High => "HIGH",
            Severity::Medium => "MED ",
            Severity::Low => "LOW ",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flag {
    pub category: String,
    pub severity: Severity,
    pub file: String,
    pub line: usize,
    pub snippet: String,
    /// Why a human should look — the failure mode, never a claim that it is present.
    pub why: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FunctionSig {
    pub name: String,
    pub visibility: String,
    /// True when the function can change state (not view/pure).
    pub mutating: bool,
    pub payable: bool,
    pub file: String,
    pub line: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceMap {
    pub flags: Vec<Flag>,
    pub functions: Vec<FunctionSig>,
    /// Externally reachable, state-changing entry points — the real attack surface.
    pub entry_points: usize,
    pub payable_entry_points: usize,
    pub total_lines: usize,
    /// A crude proxy for how much there is to review.
    pub surface_score: u32,
}

impl SurfaceMap {
    pub fn high(&self) -> usize {
        self.flags.iter().filter(|f| f.severity == Severity::High).count()
    }
}

/// One heuristic: a substring to look for, and what it means.
struct Rule {
    needle: &'static str,
    category: &'static str,
    severity: Severity,
    why: &'static str,
}

const RULES: &[Rule] = &[
    Rule { needle: "delegatecall", category: "delegatecall", severity: Severity::High,
        why: "runs external code in this contract's storage context; a wrong target or layout is catastrophic" },
    Rule { needle: "selfdestruct", category: "selfdestruct", severity: Severity::High,
        why: "destroys the contract and forwards its balance; check who can reach it" },
    Rule { needle: "suicide", category: "selfdestruct", severity: Severity::High,
        why: "deprecated alias for selfdestruct" },
    Rule { needle: "tx.origin", category: "tx.origin-auth", severity: Severity::High,
        why: "if used for authorization, a malicious intermediary contract defeats it" },
    Rule { needle: ".call{value", category: "low-level-value-call", severity: Severity::High,
        why: "external value call: classic reentrancy surface; check state is settled before it" },
    Rule { needle: ".call.value", category: "low-level-value-call", severity: Severity::High,
        why: "external value call (old syntax): reentrancy surface" },
    Rule { needle: "ecrecover", category: "signature", severity: Severity::Medium,
        why: "signature recovery: check for zero-address returns and malleability, and replay protection" },
    Rule { needle: "assembly", category: "inline-assembly", severity: Severity::Medium,
        why: "bypasses Solidity's checks; read it carefully by hand" },
    Rule { needle: "unchecked", category: "unchecked-math", severity: Severity::Medium,
        why: "arithmetic here is not overflow-checked; confirm bounds are guaranteed" },
    Rule { needle: "block.timestamp", category: "time-dependence", severity: Severity::Low,
        why: "miner-influenceable within seconds; unsafe as randomness or a tight deadline" },
    Rule { needle: "blockhash", category: "weak-randomness", severity: Severity::Medium,
        why: "predictable/manipulable; never a randomness source for value" },
    Rule { needle: "block.number", category: "time-dependence", severity: Severity::Low,
        why: "block times vary across chains; check assumptions about elapsed time" },
    Rule { needle: "_authorizeUpgrade", category: "upgradeable", severity: Severity::High,
        why: "upgrade authorization: whoever passes this can replace the whole implementation" },
    Rule { needle: "initializer", category: "upgradeable", severity: Severity::Medium,
        why: "initializer pattern: check it cannot be called twice or front-run on deploy" },
    Rule { needle: "onlyOwner", category: "access-control", severity: Severity::Low,
        why: "owner-gated: check owner transfer, renounce, and what a compromised owner reaches" },
];

/// Strip `//` and `/* */` comments and double-quoted strings so a keyword named in prose
/// is not mistaken for the code pattern. Character positions are preserved (blanked, not
/// removed) so reported line numbers stay accurate.
pub fn strip_noise(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = vec![b' '; b.len()];
    let mut i = 0;
    #[derive(PartialEq)]
    enum S { Code, Line, Block, Str }
    let mut st = S::Code;
    while i < b.len() {
        match st {
            S::Code => {
                if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
                    st = S::Line;
                } else if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
                    st = S::Block;
                    i += 2;
                    continue;
                } else if b[i] == b'"' {
                    st = S::Str;
                } else {
                    out[i] = b[i];
                }
                // newlines always preserved for line counting
                if b[i] == b'\n' {
                    out[i] = b'\n';
                }
            }
            S::Line => {
                if b[i] == b'\n' {
                    out[i] = b'\n';
                    st = S::Code;
                }
            }
            S::Block => {
                if b[i] == b'\n' {
                    out[i] = b'\n';
                }
                if b[i] == b'*' && i + 1 < b.len() && b[i + 1] == b'/' {
                    st = S::Code;
                    i += 2;
                    continue;
                }
            }
            S::Str => {
                if b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if b[i] == b'"' {
                    st = S::Code;
                } else if b[i] == b'\n' {
                    out[i] = b'\n';
                }
            }
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Extremely rough function-signature extraction. Good enough to inventory the surface;
/// not a parser, and it says so.
fn extract_functions(clean: &str, file: &str) -> Vec<FunctionSig> {
    let mut out = Vec::new();
    for (idx, line) in clean.lines().enumerate() {
        let t = line.trim_start();
        let Some(rest) = t.strip_prefix("function ") else { continue };
        let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        if name.is_empty() {
            continue;
        }
        let l = line.to_ascii_lowercase();
        let visibility = if l.contains("external") {
            "external"
        } else if l.contains("public") {
            "public"
        } else if l.contains("internal") {
            "internal"
        } else if l.contains("private") {
            "private"
        } else {
            "public" // Solidity default for functions is public
        };
        let mutating = !(l.contains(" view") || l.contains(" pure"));
        out.push(FunctionSig {
            name,
            visibility: visibility.to_string(),
            mutating,
            payable: l.contains("payable"),
            file: file.to_string(),
            line: idx + 1,
        });
    }
    out
}

/// Build the surface map for a set of source files.
pub fn analyze(files: &[(String, String)]) -> SurfaceMap {
    let mut flags = Vec::new();
    let mut functions = Vec::new();
    let mut total_lines = 0;

    for (path, code) in files {
        total_lines += code.lines().count();
        let clean = strip_noise(code);
        functions.extend(extract_functions(&clean, path));
        for (idx, line) in clean.lines().enumerate() {
            for rule in RULES {
                if line.contains(rule.needle) {
                    flags.push(Flag {
                        category: rule.category.to_string(),
                        severity: rule.severity,
                        file: path.clone(),
                        line: idx + 1,
                        snippet: line.trim().chars().take(100).collect(),
                        why: rule.why.to_string(),
                    });
                }
            }
        }
    }

    flags.sort_by(|a, b| {
        b.severity
            .rank()
            .cmp(&a.severity.rank())
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    let entry_points = functions
        .iter()
        .filter(|f| (f.visibility == "external" || f.visibility == "public") && f.mutating)
        .count();
    let payable_entry_points = functions.iter().filter(|f| f.payable).count();
    let external_calls = flags.iter().filter(|f| f.category == "low-level-value-call").count();

    let surface_score =
        (entry_points as u32) + (external_calls as u32 * 3) + (payable_entry_points as u32 * 2);

    SurfaceMap {
        flags,
        functions,
        entry_points,
        payable_entry_points,
        total_lines,
        surface_score,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(code: &str) -> SurfaceMap {
        analyze(&[("T.sol".into(), code.to_string())])
    }

    #[test]
    fn a_pattern_named_in_a_comment_is_not_flagged() {
        let m = map("// this contract avoids delegatecall entirely\ncontract C {}");
        assert!(m.flags.is_empty(), "the word in a comment must not trip the rule");
    }

    #[test]
    fn a_pattern_in_a_string_is_not_flagged() {
        let m = map(r#"contract C { function f() public { emit Log("use tx.origin here"); } }"#);
        assert!(!m.flags.iter().any(|f| f.category == "tx.origin-auth"));
    }

    #[test]
    fn real_dangerous_patterns_are_flagged_with_line_numbers() {
        let code = "contract C {\n  function upgrade() external {\n    x.delegatecall(data);\n  }\n}";
        let m = map(code);
        let d = m.flags.iter().find(|f| f.category == "delegatecall").unwrap();
        assert_eq!(d.line, 3, "line number must survive comment stripping");
        assert_eq!(d.severity, Severity::High);
    }

    #[test]
    fn high_severity_sorts_first() {
        let code = "contract C {\n  uint x = block.timestamp;\n  function k() external { y.delegatecall(z); }\n}";
        let m = map(code);
        assert_eq!(m.flags[0].severity, Severity::High, "high before low");
    }

    #[test]
    fn the_surface_is_the_external_state_changing_functions() {
        let code = "contract C {\n\
          function a() external {}\n\
          function b() public view returns (uint) {}\n\
          function c() internal {}\n\
          function d() external payable {}\n\
        }";
        let m = map(code);
        // a and d mutate and are external; b is view; c is internal.
        assert_eq!(m.entry_points, 2, "only external/public state-changing functions count");
        assert_eq!(m.payable_entry_points, 1);
        assert!(m.surface_score >= 2);
    }

    #[test]
    fn line_numbers_survive_a_block_comment() {
        let code = "contract C {\n/* a long\n block comment\n mentioning selfdestruct */\n  function f() external { z.delegatecall(d); }\n}";
        let m = map(code);
        assert!(!m.flags.iter().any(|f| f.category == "selfdestruct"), "in-comment mention ignored");
        let d = m.flags.iter().find(|f| f.category == "delegatecall").unwrap();
        assert_eq!(d.line, 5);
    }

    #[test]
    fn a_clean_contract_produces_no_flags_but_still_reports_surface() {
        let m = map("contract C {\n  function ping() external {}\n}");
        assert!(m.flags.is_empty());
        assert_eq!(m.entry_points, 1, "no flags is not the same as no surface");
    }
}
