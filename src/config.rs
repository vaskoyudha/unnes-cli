//! config.toml model: defaults embedded, pages are config-driven (never hardcoded selectors).

use std::fs;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::paths::UnnesHome;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub notify: Notify,
    pub pages: Vec<Page>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    pub base_url: String,
    pub default_interval: u64,
    /// On session expiry, retry with the scripted Google re-login
    /// (saved profile) before failing; default true.
    #[serde(default = "default_true")]
    pub auto_relogin: bool,
    pub min_interval: u64,
    pub jitter_fraction: f64,
    pub user_agent: String,
    pub adaptive: Vec<AdaptiveWindow>,
}

/// Declared high-activity windows (e.g. grade-release days) during which the
/// watcher polls at an aggressive interval, then relaxes to the default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveWindow {
    pub label: Option<String>,
    /// Local wall-clock window start, format "MM-DD HH:MM" (24h).
    pub start: String,
    /// Local wall-clock window end, format "MM-DD HH:MM" (24h).
    pub end: String,
    /// Interval (seconds) used while inside the window.
    pub interval: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Notify {
    /// Optional command run on change; changelog entry JSON is piped to its stdin.
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    pub id: String,
    pub url: String,
    /// Override the default poll interval (seconds).
    pub interval: Option<u64>,
    /// CSS selector producing one element per record (table row / card).
    pub selector: Option<String>,
    /// Record key for diffing (default: first field of each record).
    pub key_field: Option<String>,
    /// Extra regex patterns (Rust regex syntax) stripped before hashing.
    #[serde(default)]
    pub normalize: Vec<String>,
    /// Render the page in the persistent browser session first (Livewire /
    /// iframe-SSO apps like akademik KRS or Elena) instead of plain HTTP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<bool>,
    /// Gateway app id to prime the app session (76 = akademik, 30 = elena,
    /// 64 = student portal) before fetching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sso_app: Option<String>,
    /// Crawl mode: selector yielding the <a> links to follow from this page;
    /// each linked page is fetched (render) and its rows become records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_selector: Option<String>,
    /// URL visited before the target (e.g. the akademik semester switcher).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_url: Option<String>,
    /// elena semester to open after SSO (default 20261, current Gasal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sso_semester: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Default for General {
    fn default() -> Self {
        Self {
            base_url: "https://apps.unnes.ac.id".to_string(),
            default_interval: 900,
            auto_relogin: true,
            min_interval: 60,
            jitter_fraction: 0.1,
            user_agent: "unnes-cli/0.1 (personal student automation; polite)".to_string(),
            adaptive: Vec::new(),
        }
    }
}

impl Default for Notify {
    fn default() -> Self {
        Self { command: None }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: General::default(),
            notify: Notify::default(),
            pages: Vec::new(),
        }
    }
}

impl Config {
    /// Parse a TOML string (used by the CLI and tests).
    pub fn try_from_str(s: &str) -> Result<Config> {
        let cfg: Config = toml::from_str(s)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load config.toml; missing file -> defaults.
    pub fn load(home: &UnnesHome) -> Result<Config> {
        let path = home.config_file();
        if !path.exists() {
            return Ok(Config::default());
        }
        let raw = fs::read_to_string(&path)?;
        Config::try_from_str(&raw)
    }

    /// Serialize to TOML (used by watch add/rm to persist config changes).
    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string(self)?)
    }

    pub fn page(&self, id: &str) -> Option<&Page> {
        self.pages.iter().find(|p| p.id == id)
    }

    pub fn validate(&self) -> Result<()> {
        let mut seen: Vec<&str> = Vec::new();
        for p in &self.pages {
            if p.id.trim().is_empty() {
                bail!("page id must not be empty");
            }
            if seen.contains(&p.id.as_str()) {
                bail!("duplicate page id: {}", p.id);
            }
            seen.push(&p.id);
            if p.url.trim().is_empty() {
                bail!("page '{}' has an empty url", p.id);
            }
            if let Some(iv) = p.interval {
                if iv < self.general.min_interval {
                    bail!(
                        "page '{}' interval {}s is below general.min_interval {}s",
                        p.id,
                        iv,
                        self.general.min_interval
                    );
                }
            }
        }
        if self.general.jitter_fraction < 0.0 || self.general.jitter_fraction > 0.5 {
            bail!("general.jitter_fraction must be within [0.0, 0.5]");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_empty() {
        let cfg = Config::try_from_str("").unwrap();
        assert_eq!(cfg.general.base_url, "https://apps.unnes.ac.id");
        assert!(cfg.general.auto_relogin);
        assert_eq!(cfg.general.default_interval, 900);
        assert!(cfg.pages.is_empty());
        assert!(cfg.notify.command.is_none());
    }

    #[test]
    fn parses_pages_and_notify() {
        let s = r#"
[notify]
command = "/usr/bin/notify-send unnes"

[[pages]]
id = "grades"
url = "https://student.unnes.ac.id/grades"
selector = "table tbody tr"
key_field = "code"
interval = 600
"#;
        let cfg = Config::try_from_str(s).unwrap();
        assert_eq!(cfg.pages.len(), 1);
        assert_eq!(cfg.notify.command.as_deref(), Some("/usr/bin/notify-send unnes"));
        let g = cfg.page("grades").unwrap();
        assert_eq!(g.selector.as_deref(), Some("table tbody tr"));
        assert_eq!(g.key_field.as_deref(), Some("code"));
        assert_eq!(g.interval, Some(600));
    }

    #[test]
    fn rejects_duplicate_page_ids() {
        let s = r#"
[[pages]]
id = "grades"
url = "https://x/grades"

[[pages]]
id = "grades"
url = "https://x/grades2"
"#;
        let err = Config::try_from_str(s).unwrap_err();
        assert!(err.to_string().contains("duplicate page id"));
    }

    #[test]
    fn rejects_interval_below_min() {
        let s = r#"
[general]
min_interval = 60

[[pages]]
id = "grades"
url = "https://x"
interval = 10
"#;
        let err = Config::try_from_str(s).unwrap_err();
        assert!(err.to_string().contains("below general.min_interval"));
    }

    #[test]
    fn toml_roundtrip_preserves_pages() {
        let cfg = Config::try_from_str(
            r#"[[pages]]
id = "a"
url = "https://x/a"
normalize = ["t=[0-9]+"]
"#,
        )
        .unwrap();
        let s = cfg.to_toml().unwrap();
        let cfg2 = Config::try_from_str(&s).unwrap();
        assert_eq!(cfg2.pages[0].normalize, vec!["t=[0-9]+"]);
    }
}
