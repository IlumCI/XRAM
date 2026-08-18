//! Sandboxed command execution.
//!
//! The engine runs untrusted, model-written code against a project's own test suite,
//! many times over. Two properties matter: it must not escape, and it must be cheap
//! enough that running the *full* suite several times is the obvious choice rather than
//! an indulgence.
//!
//! [`LocalSandbox`] is the always-available backend: a private working copy, a scrubbed
//! environment, a hard wall-clock cap, and network isolation where the kernel allows it
//! unprivileged. The trait exists so a microVM backend can be dropped in later without
//! touching the engine.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

/// Cap on captured output. Test suites can print megabytes; we only need the tail-end
/// shape of it, and unbounded capture is a memory bug waiting for a bad fixture.
const MAX_CAPTURE: usize = 1 << 20;

#[derive(Debug, Clone)]
pub struct RunSpec {
    pub program: String,
    pub args: Vec<String>,
    pub workdir: PathBuf,
    pub timeout_secs: u64,
    /// Environment variables to pass through in addition to the minimal base set.
    pub env: Vec<(String, String)>,
}

impl RunSpec {
    pub fn new(program: impl Into<String>, workdir: impl AsRef<Path>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            workdir: workdir.as_ref().to_path_buf(),
            timeout_secs: 120,
            env: Vec::new(),
        }
    }
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }
    pub fn args<I: IntoIterator<Item = S>, S: Into<String>>(mut self, it: I) -> Self {
        self.args.extend(it.into_iter().map(Into::into));
        self
    }
    pub fn timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }
}

#[derive(Debug, Clone)]
pub struct RunOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
}

impl RunOutput {
    /// Both streams, in that order. Harnesses differ on which one they report to.
    pub fn combined(&self) -> String {
        let mut s = String::with_capacity(self.stdout.len() + self.stderr.len() + 1);
        s.push_str(&self.stdout);
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str(&self.stderr);
        s
    }
}

pub trait Sandbox: Send + Sync {
    fn name(&self) -> &str;
    fn run(&self, spec: &RunSpec) -> Result<RunOutput>;
}

/// Detected once, at construction: probing the kernel per-run would dominate the cost
/// of the runs themselves.
#[derive(Debug, Clone, Copy, Default)]
pub struct SandboxCaps {
    /// `unshare -rn` works unprivileged, so we can cut the network.
    pub net_isolation: bool,
    /// GNU `timeout` is present, which kills the whole process group rather than
    /// leaving orphaned test runners behind.
    pub timeout_tool: bool,
}

impl SandboxCaps {
    pub fn detect() -> Self {
        Self {
            net_isolation: probe(&["unshare", "-rn", "true"]),
            timeout_tool: probe(&["timeout", "1", "true"]),
        }
    }
}

fn probe(argv: &[&str]) -> bool {
    Command::new(argv[0])
        .args(&argv[1..])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub struct LocalSandbox {
    caps: SandboxCaps,
    name: String,
}

impl Default for LocalSandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalSandbox {
    pub fn new() -> Self {
        let caps = SandboxCaps::detect();
        let name = format!(
            "local(net_isolation={},timeout_tool={})",
            caps.net_isolation, caps.timeout_tool
        );
        Self { caps, name }
    }

    /// Construct with explicit capabilities, for tests that need a known configuration.
    pub fn with_caps(caps: SandboxCaps) -> Self {
        let name = format!(
            "local(net_isolation={},timeout_tool={})",
            caps.net_isolation, caps.timeout_tool
        );
        Self { caps, name }
    }

    pub fn caps(&self) -> SandboxCaps {
        self.caps
    }

    /// Build the real argv, wrapping the requested command in whatever isolation the
    /// kernel actually gave us.
    fn wrap(&self, spec: &RunSpec) -> (String, Vec<String>) {
        let mut argv: Vec<String> = Vec::new();
        if self.caps.net_isolation {
            argv.extend(["unshare".to_string(), "-rn".to_string()]);
        }
        if self.caps.timeout_tool {
            argv.extend([
                "timeout".to_string(),
                "--kill-after=2s".to_string(),
                format!("{}s", spec.timeout_secs),
            ]);
        }
        argv.push(spec.program.clone());
        argv.extend(spec.args.iter().cloned());
        let program = argv.remove(0);
        (program, argv)
    }
}

/// GNU `timeout` reports 124 when it had to kill the child.
const TIMEOUT_EXIT: i32 = 124;

impl Sandbox for LocalSandbox {
    fn name(&self) -> &str {
        &self.name
    }

    fn run(&self, spec: &RunSpec) -> Result<RunOutput> {
        let (program, args) = self.wrap(spec);
        let started = Instant::now();

        let mut cmd = Command::new(&program);
        cmd.args(&args)
            .current_dir(&spec.workdir)
            .env_clear()
            // A minimal, explicit environment: model-written code should not inherit
            // API keys from this process, and tests should not depend on our shell.
            .env("PATH", std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()))
            .env("HOME", spec.workdir.display().to_string())
            .env("LANG", "C.UTF-8")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            // Hash randomisation makes identical runs disagree, which would read as
            // flakiness in every Python fixture that iterates a set.
            .env("PYTHONHASHSEED", "0")
            .env("CI", "1");
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        let out = cmd
            .output()
            .with_context(|| format!("spawning {program} in {}", spec.workdir.display()))?;

        let duration_ms = started.elapsed().as_millis() as u64;
        let exit_code = out.status.code();
        let timed_out = exit_code == Some(TIMEOUT_EXIT)
            || (!self.caps.timeout_tool && duration_ms >= spec.timeout_secs * 1000);

        Ok(RunOutput {
            stdout: truncate(String::from_utf8_lossy(&out.stdout).into_owned()),
            stderr: truncate(String::from_utf8_lossy(&out.stderr).into_owned()),
            exit_code,
            timed_out,
            duration_ms,
        })
    }
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_CAPTURE {
        // Keep the tail: failures and summaries live at the end of test output.
        let cut = s.len() - MAX_CAPTURE;
        let cut = (cut..s.len())
            .find(|&i| s.is_char_boundary(i))
            .unwrap_or(s.len());
        s = format!("[...truncated...]\n{}", &s[cut..]);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_a_command_and_captures_output() {
        let sb = LocalSandbox::new();
        let dir = tempfile::tempdir().unwrap();
        let out = sb
            .run(&RunSpec::new("sh", dir.path()).arg("-c").arg("echo hi; echo bye >&2"))
            .unwrap();
        assert!(out.stdout.contains("hi"));
        assert!(out.stderr.contains("bye"));
        assert_eq!(out.exit_code, Some(0));
        assert!(!out.timed_out);
    }

    #[test]
    fn nonzero_exit_is_reported_not_hidden() {
        let sb = LocalSandbox::new();
        let dir = tempfile::tempdir().unwrap();
        let out = sb
            .run(&RunSpec::new("sh", dir.path()).arg("-c").arg("exit 3"))
            .unwrap();
        assert_eq!(out.exit_code, Some(3));
    }

    #[test]
    fn a_hanging_command_is_killed() {
        let sb = LocalSandbox::new();
        if !sb.caps().timeout_tool {
            return; // Without the tool there is nothing to assert.
        }
        let dir = tempfile::tempdir().unwrap();
        let out = sb
            .run(&RunSpec::new("sh", dir.path()).arg("-c").arg("sleep 30").timeout(1))
            .unwrap();
        assert!(out.timed_out, "a hanging suite must not hang the swarm");
        assert!(out.duration_ms < 10_000);
    }

    #[test]
    fn environment_is_scrubbed() {
        std::env::set_var("SWARM_SECRET_CANARY", "leaked");
        let sb = LocalSandbox::new();
        let dir = tempfile::tempdir().unwrap();
        let out = sb
            .run(&RunSpec::new("sh", dir.path()).arg("-c").arg("echo ${SWARM_SECRET_CANARY:-absent}"))
            .unwrap();
        assert!(
            out.stdout.contains("absent"),
            "sandboxed code must not inherit our environment"
        );
    }

    #[test]
    fn output_is_capped() {
        let sb = LocalSandbox::new();
        let dir = tempfile::tempdir().unwrap();
        let out = sb
            .run(
                &RunSpec::new("sh", dir.path())
                    .arg("-c")
                    .arg("yes abcdefghij | head -c 3000000")
                    .timeout(30),
            )
            .unwrap();
        assert!(out.stdout.len() <= MAX_CAPTURE + 32);
    }
}
