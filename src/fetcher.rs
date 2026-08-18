//! Bridge to the Node/TS fetcher arm (fetcher/dist/index.js).
//!
//! Protocol per fetcher/CONTRACT.md v1: spawn node dist/index.js per
//! operation, write ONE JSON job on stdin, read ONE JSON result line from
//! stdout. Anything else on stdout is a bug; user-facing progress from the
//! fetcher goes to stderr.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::paths::UnnesHome;

/// Script location: $UNNES_FETCHER > ./fetcher/dist/index.js > <exe>/../fetcher/dist/index.js.
fn fetcher_script() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("UNNES_FETCHER") {
        return Ok(PathBuf::from(p));
    }
    let cwd = std::env::current_dir().context("cannot read cwd")?;
    let from_cwd = cwd.join("fetcher").join("dist").join("index.js");
    if from_cwd.is_file() {
        return Ok(from_cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        let rel = exe
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("fetcher").join("dist").join("index.js"));
        if let Some(p) = rel {
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    bail!(
        "cannot locate fetcher/dist/index.js (set UNNES_FETCHER or run from the repo root)"
    )
}

/// One result line from the fetcher; tolerant to optional fields.
/// The fetcher speaks camelCase (CONTRACT.md), so map to snake_case here.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobResult {
    pub ok: bool,
    pub contract: Option<u64>,
    pub status: Option<u64>,
    pub final_url: Option<String>,
    #[serde(default)]
    pub session_expired: bool,
    #[serde(default)]
    pub challenge: bool,
    pub retry_after: Option<u64>,
    #[serde(default)]
    pub records: Vec<Value>,
    /// normalized page text (op=get only)
    pub normalized: Option<String>,
    /// op=batch: per-page results
    #[serde(default)]
    pub results: Vec<BatchPageResult>,
    pub landing_url: Option<String>,
    pub captured_cookies: Option<u64>,
    pub mode: Option<String>,
    pub error: Option<JobError>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobError {
    pub code: String,
    pub message: String,
}

/// One page's outcome inside an op=batch result.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchPageResult {
    pub url: String,
    pub ok: bool,
    pub final_url: Option<String>,
    #[serde(default)]
    pub session_expired: bool,
    #[serde(default)]
    pub records: Vec<Value>,
    pub error: Option<JobError>,
}

/// Result of op=batch.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchResult {
    pub ok: bool,
    #[serde(default)]
    pub results: Vec<BatchPageResult>,
    pub captured_cookies: Option<u64>,
    pub error: Option<JobError>,
}

/// Run one job against the fetcher; returns the parsed result.
pub fn run_job(home: &UnnesHome, profile: &str, job: Value) -> Result<JobResult> {
    let script = fetcher_script()?;
    let mut child = Command::new("node")
        .arg(&script)
        .env("UNNES_HOME", &home.root)
        .env("UNNES_PROFILE", profile)
        .env("UNNES_USER_AGENT", "unnes-cli/0.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to spawn node; is Node.js >= 20 installed?")?;

    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("cannot open fetcher stdin"))?;
        let payload = serde_json::to_string(&job)?;
        stdin.write_all(payload.as_bytes())?;
        stdin.write_all(b"\n")?;
    }

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("cannot open fetcher stdout"))?;
    let mut raw = String::new();
    stdout.read_to_string(&mut raw)?;

    let status = child.wait().context("fetcher did not exit cleanly")?;
    if !status.success() {
        bail!("fetcher exited with {status}");
    }

    let line = raw.lines().next().ok_or_else(|| anyhow!("fetcher produced no output"))?;
    let result: JobResult =
        serde_json::from_str(line).with_context(|| format!("cannot parse fetcher result: {line}"))?;
    if result.contract != Some(1) {
        bail!("fetcher contract mismatch: expected 1, got {:?}", result.contract);
    }
    Ok(result)
}

/// Build the standard job envelope.
pub fn job(op: &str, profile: &str) -> Value {
    json!({
        "contract": 1,
        "op": op,
        "profile": profile,
        "baseUrl": "https://student.unnes.ac.id",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_envelope_has_contract_and_profile() {
        let home = UnnesHome { root: std::path::PathBuf::from("/tmp/unnes-test") };
        let j = job("get", "work");
        assert_eq!(j["contract"], 1);
        assert_eq!(j["op"], "get");
        assert_eq!(j["profile"], "work");
        assert!(j["baseUrl"].is_string());
        let _ = &home; // keep signature documented
    }
}

pub fn profile_name() -> String {
    std::env::var("UNNES_PROFILE").unwrap_or_else(|_| "default".to_string())
}
