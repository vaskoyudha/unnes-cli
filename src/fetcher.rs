//! Bridge to the Node/TS fetcher arm (fetcher/dist/index.js).
//!
//! Protocol per fetcher/CONTRACT.md v1: spawn node dist/index.js per
//! operation, write ONE JSON job on stdin, read ONE JSON result line from
//! stdout. Anything else on stdout is a bug; user-facing progress from the
//! fetcher goes to stderr.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::paths::UnnesHome;

/// Recursively copy a directory tree (symlinks dereferenced). Keeps the
/// dist + node_modules layout intact so Node resolves "playwright" from the
/// copied location too.
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// One-time self-install: copy a discoverable fetcher tree into
/// $UNNES_HOME/fetcher, so the installed binary (cargo install ships only
/// the executable) works from ANY directory. Idempotent.
pub fn bootstrap_fetcher(home: &UnnesHome, source: &Path) -> Result<PathBuf> {
    let dest = home.root.join("fetcher");
    let script = dest.join("dist").join("index.js");
    if script.is_file() {
        return Ok(script);
    }
    eprintln!(
        "unnes: memasang fetcher ke {} (sekali saja, ~50 MB)...",
        dest.display()
    );
    copy_tree(source, &dest)?;
    if !script.is_file() {
        bail!("bootstrap incomplete: {} missing", script.display());
    }
    Ok(script)
}

/// Script location, searched in this order:
///   1. $UNNES_FETCHER (explicit override)
///   2. $UNNES_HOME/fetcher/dist/index.js (installed copy, works from any cwd)
///   3. a discoverable source tree (./fetcher in the cwd, or <exe>/../fetcher)
///      - which is self-installed into $UNNES_HOME ONCE, so the binary keeps
///        working from any directory afterwards.
/// When neither an installed copy nor a source tree exists, the error tells
/// the user to run once from the repo checkout (that run performs the install).
fn fetcher_script(home: &UnnesHome) -> Result<PathBuf> {
    if let Ok(p) = std::env::var("UNNES_FETCHER") {
        return Ok(PathBuf::from(p));
    }
    let installed = home.root.join("fetcher").join("dist").join("index.js");
    if installed.is_file() {
        return Ok(installed);
    }
    let cwd = std::env::current_dir().context("cannot read cwd")?;
    let usable = |dir: &Path| {
        if dir.join("dist").join("index.js").is_file() {
            Some(dir.to_path_buf())
        } else {
            None
        }
    };
    let source = usable(&cwd.join("fetcher")).or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().and_then(|p| p.parent()).map(|p| p.join("fetcher")))
            .and_then(|p| usable(&p))
    });
    if let Some(src) = source {
        return bootstrap_fetcher(home, &src);
    }
    bail!(
        "cannot locate fetcher/dist/index.js - run once from the repo checkout (unnes installs it into {}) or set UNNES_FETCHER",
        home.root.display()
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
    /// op=submit: human-readable outcome
    pub message: Option<String>,
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
    let script = fetcher_script(home)?;
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

    #[test]
    fn copy_tree_copies_nested_layout() {
        let tag = format!("{}-cpytree", std::process::id());
        let src = std::env::temp_dir().join(&tag).join("src");
        let dst = std::env::temp_dir().join(&tag).join("dst");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
        fs::create_dir_all(src.join("dist")).unwrap();
        fs::create_dir_all(src.join("node_modules/pkg")).unwrap();
        fs::write(src.join("dist/index.js"), "//x").unwrap();
        fs::write(src.join("node_modules/pkg/readme.txt"), "hi").unwrap();
        copy_tree(&src, &dst).unwrap();
        assert!(dst.join("dist/index.js").is_file());
        assert!(dst.join("node_modules/pkg/readme.txt").is_file());
        assert_eq!(fs::read_to_string(dst.join("node_modules/pkg/readme.txt")).unwrap(), "hi");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&dst);
    }

    #[test]
    fn bootstrap_installs_fetcher_and_is_idempotent() {
        let tag = format!("{}-bootstrap", std::process::id());
        let src = std::env::temp_dir().join(&tag).join("fetcher-src");
        let home = UnnesHome { root: std::env::temp_dir().join(&tag).join("home") };
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&home.root);
        fs::create_dir_all(src.join("dist")).unwrap();
        fs::write(src.join("dist/index.js"), "module.exports=1").unwrap();
        let first = bootstrap_fetcher(&home, &src).unwrap();
        assert_eq!(first, home.root.join("fetcher").join("dist").join("index.js"));
        assert!(first.is_file());
        // second call: no re-copy, same path
        let second = bootstrap_fetcher(&home, &src).unwrap();
        assert_eq!(first, second);
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&home.root);
    }
}

pub fn profile_name() -> String {
    std::env::var("UNNES_PROFILE").unwrap_or_else(|_| "default".to_string())
}
