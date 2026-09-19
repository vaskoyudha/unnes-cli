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
/// the executable) works from ANY directory. Idempotent and race-safe: the
/// tree is staged into a unique temp dir and renamed into place, so two
/// concurrent first runs can never interleave partial files and a third
/// process never observes a half-written tree (rename wins atomically, the
/// loser cleans up and uses the winner's install).
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
    let tmp = home.root.join(format!("fetcher.tmp.{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    copy_tree(source, &tmp)?;
    match fs::rename(&tmp, &dest) {
        Ok(()) => {}
        // Lost the race (rename onto the winner's non-empty dir fails):
        // use the winner's install when it is complete.
        Err(_) if script.is_file() => {
            let _ = fs::remove_dir_all(&tmp);
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e).with_context(|| format!("cannot install fetcher to {}", dest.display()));
        }
    }
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
    /// op=login mode=browser: persistent Google auth cookies kept (SID family
    /// with real expiry). Some(0) = chooser empty next login (see browser.ts).
    pub google_persistent: Option<u64>,
    /// op=submit: verification flags read from the real page state.
    #[serde(default)]
    pub submitted: bool,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub has_file: bool,
    /// op=download: saved file location + size + server filename.
    pub path: Option<String>,
    pub bytes: Option<u64>,
    pub filename: Option<String>,
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

/// One URL's outcome inside an op=batchget result.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchGetEntry {
    pub url: String,
    pub ok: bool,
    pub status: Option<u64>,
    pub final_url: Option<String>,
    #[serde(default)]
    pub session_expired: bool,
    #[serde(default)]
    pub records: Vec<Value>,
    pub normalized: Option<String>,
    pub error: Option<JobError>,
}

/// Result of op=batchget: N plain-HTTP GETs served by one node spawn
/// sharing one jar (and one keep-alive pool). Top-level `ok` only means the
/// op executed; per-URL success lives on each entry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchGetResult {
    pub contract: Option<u64>,
    pub ok: bool,
    #[serde(default)]
    pub session_expired: bool,
    #[serde(default)]
    pub results: Vec<BatchGetEntry>,
    pub error: Option<JobError>,
}

/// Guard: SIGTERM the fetcher child if we abandon it early. Every `?` below
/// (stdin/stdout/parse failures) used to leak a live node+Chrome holding the
/// profile lock; Drop now reaps it (node's own shutdown guard flushes first).
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

/// Run one job against the fetcher; returns the first stdout line.
/// Keeps the KillOnDrop guard and stderr behavior for every op.
fn run_raw_job(home: &UnnesHome, profile: &str, job: Value) -> Result<String> {
    let script = fetcher_script(home)?;
    let mut child = KillOnDrop(
        Command::new("node")
            .arg(&script)
            .env("UNNES_HOME", &home.root)
            .env("UNNES_PROFILE", profile)
            .env("UNNES_USER_AGENT", "unnes-cli/0.1")
            .env("NODE_NO_WARNINGS", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn node; is Node.js >= 20 installed?")?,
    );

    {
        let mut stdin = child
            .0
            .stdin
            .take()
            .ok_or_else(|| anyhow!("cannot open fetcher stdin"))?;
        let payload = serde_json::to_string(&job)?;
        stdin.write_all(payload.as_bytes())?;
        stdin.write_all(b"\n")?;
    }

    // Stream stderr live for every browser-backed op (login/page/crawl/batch/
    // submit/open): their [browser]/[submit] trails prove WHERE a failure
    // happened. Plain-HTTP ops keep buffered stderr (shown on process error).
    // Only the headed login prints to the terminal; the rest is captured for
    // the error report.
    let op = job.get("op").and_then(|v| v.as_str()).unwrap_or("");
    let stream_stderr = matches!(op, "login" | "page" | "crawl" | "batch" | "submit" | "open");
    let is_browser_login = op == "login"
        && job.get("mode").and_then(|v| v.as_str()) == Some("browser");

    let stderr_handle = child.0.stderr.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            if stream_stderr {
                let mut chunk = [0u8; 512];
                while let Ok(n) = s.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    if is_browser_login {
                        let _ = std::io::stderr().write_all(&chunk[..n]);
                        let _ = std::io::stderr().flush();
                    }
                    if let Ok(text) = std::str::from_utf8(&chunk[..n]) {
                        buf.push_str(text);
                    }
                }
            } else {
                let _ = s.read_to_string(&mut buf);
            }
            buf
        })
    });

    let mut stdout = child
        .0
        .stdout
        .take()
        .ok_or_else(|| anyhow!("cannot open fetcher stdout"))?;
    let mut raw = String::new();
    stdout.read_to_string(&mut raw)?;

    let stderr_raw = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    let status = child.0.wait().context("fetcher did not exit cleanly")?;
    // Reaped normally: disarm the kill guard (forget, don't run it).
    std::mem::forget(child);
    if !status.success() {
        let extra = if stderr_raw.trim().is_empty() {
            String::new()
        } else {
            format!(": {}", stderr_raw.trim())
        };
        bail!("fetcher exited with {status}{extra}");
    }

    let line = raw.lines().next().ok_or_else(|| anyhow!("fetcher produced no output"))?;
    Ok(line.to_string())
}

/// Run one job against the fetcher; returns the parsed result.
pub fn run_job(home: &UnnesHome, profile: &str, job: Value) -> Result<JobResult> {
    let line = run_raw_job(home, profile, job)?;
    let result: JobResult =
        serde_json::from_str(&line).with_context(|| format!("cannot parse fetcher result: {line}"))?;
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

/// Run one op=batchget job: `urls` is (url, extract-spec) pairs; a
/// `Value::Null` extract means "no extraction, normalized only".
/// One node spawn serves all URLs sharing one jar — use this instead of
/// looping `run_job` per URL.
pub fn run_batchget(home: &UnnesHome, profile: &str, urls: &[(String, Value)]) -> Result<BatchGetResult> {
    let mut j = job("batchget", profile);
    j["urls"] = json!(urls.iter().map(|(u, ex)| json!({"url": u, "extract": ex})).collect::<Vec<_>>());
    let line = run_raw_job(home, profile, j)?;
    let result: BatchGetResult =
        serde_json::from_str(&line).with_context(|| format!("cannot parse fetcher result: {line}"))?;
    if result.contract != Some(1) {
        bail!("fetcher contract mismatch: expected 1, got {:?}", result.contract);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batchget_result_maps_per_url_shapes() {
        let raw = serde_json::json!({
            "contract": 1, "op": "batchget", "ok": true, "sessionExpired": true,
            "results": [
                {"url": "https://x/a", "ok": true, "status": 200, "sessionExpired": false, "records": [{"a": "1"}]},
                {"url": "https://x/b", "ok": true, "status": 200, "sessionExpired": true, "records": []},
                {"url": "https://x/c", "ok": false, "error": {"code": "timeout", "message": "boom"}},
            ],
        });
        let r: BatchGetResult = serde_json::from_value(raw).unwrap();
        assert!(r.ok);
        assert!(r.session_expired);
        assert_eq!(r.results.len(), 3);
        assert_eq!(r.results[0].records.len(), 1);
        assert!(r.results[1].session_expired);
        assert_eq!(r.results[2].error.as_ref().unwrap().code, "timeout");
    }

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
