//! Filesystem layout. Everything lives under UNNES_HOME (default ~/.config/unnes),
//! overridable via the UNNES_HOME env var (XDG-aware).

use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;

/// Resolved home directory for config + state.
#[derive(Debug, Clone)]
pub struct UnnesHome {
    pub root: PathBuf,
}

impl Default for UnnesHome {
    fn default() -> Self {
        Self::discover()
    }
}

impl UnnesHome {
    /// Resolve the home dir: $UNNES_HOME > $XDG_CONFIG_HOME/unnes > ~/.config/unnes.
    pub fn discover() -> Self {
        let root = env::var_os("UNNES_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .map(|p| p.join("unnes"))
            })
            .unwrap_or_else(|| {
                let home = env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."));
                home.join(".config").join("unnes")
            });
        Self { root }
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }
    pub fn profiles_dir(&self) -> PathBuf {
        self.root.join("profiles")
    }
    /// Cookie jar for one profile (written by the fetcher, 0600).
    pub fn profile_jar_file(&self, profile: &str) -> PathBuf {
        self.profiles_dir().join(format!("{profile}.json"))
    }
    /// Login metadata for one profile (landing URL, timestamp).
    pub fn profile_meta_file(&self, profile: &str) -> PathBuf {
        self.profiles_dir().join(format!("{profile}.meta.json"))
    }
    pub fn snapshots_dir(&self) -> PathBuf {
        self.root.join("snapshots")
    }
    pub fn debug_dir(&self) -> PathBuf {
        self.root.join("debug")
    }
    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }
    pub fn changelog_file(&self) -> PathBuf {
        self.root.join("changelog.jsonl")
    }
    pub fn state_file(&self) -> PathBuf {
        self.root.join("state.json")
    }
    pub fn lock_file(&self) -> PathBuf {
        self.root.join(".lock")
    }
    pub fn credentials_file(&self) -> PathBuf {
        self.root.join("credentials.env")
    }

    /// Create the directory tree; safe to call repeatedly.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.root,
            &self.profiles_dir(),
            &self.snapshots_dir(),
            &self.debug_dir(),
        ] {
            fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_relative_to_root() {
        let home = UnnesHome { root: PathBuf::from("/tmp/unnes-test") };
        assert_eq!(home.config_file(), PathBuf::from("/tmp/unnes-test/config.toml"));
        assert_eq!(home.profiles_dir(), PathBuf::from("/tmp/unnes-test/profiles"));
        assert_eq!(home.snapshots_dir(), PathBuf::from("/tmp/unnes-test/snapshots"));
        assert_eq!(home.debug_dir(), PathBuf::from("/tmp/unnes-test/debug"));
        assert_eq!(home.changelog_file(), PathBuf::from("/tmp/unnes-test/changelog.jsonl"));
        assert_eq!(home.credentials_file(), PathBuf::from("/tmp/unnes-test/credentials.env"));
    }
}
