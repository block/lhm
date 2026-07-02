//! User-wide lhm config (`~/.config/lhm.yaml`), distinct from the lefthook
//! config that lhm merges. Currently tracks the set of repos (keyed by
//! `origin` remote URL) for which repo-specific hooks have been disabled.

use log::debug;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

const LHM_CONFIG_FILENAME: &str = "lhm.yaml";

/// Deserialized contents of `~/.config/lhm.yaml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LhmConfig {
    /// Set of git origin URLs whose repo-specific hooks are disabled.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub disabled_repos: BTreeSet<String>,
}

impl LhmConfig {
    /// Whether `origin` is in the disabled set.
    pub fn is_disabled(&self, origin: &str) -> bool {
        self.disabled_repos.contains(origin)
    }

    /// Add `origin` to the disabled set. Returns `true` if the set changed.
    pub fn disable(&mut self, origin: &str) -> bool {
        self.disabled_repos.insert(origin.to_string())
    }

    /// Remove `origin` from the disabled set. Returns `true` if the set changed.
    pub fn enable(&mut self, origin: &str) -> bool {
        self.disabled_repos.remove(origin)
    }
}

/// Path to the lhm config file inside `config_dir` (typically `~/.config`).
pub fn lhm_config_path(config_dir: &Path) -> PathBuf {
    config_dir.join(LHM_CONFIG_FILENAME)
}

/// Load the lhm config from `config_dir`. Returns a default (empty) config if
/// the file is missing; errors only on read or parse failures.
pub fn load(config_dir: &Path) -> Result<LhmConfig, String> {
    let path = lhm_config_path(config_dir);
    if !path.exists() {
        return Ok(LhmConfig::default());
    }
    let content = fs::read_to_string(&path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    if content.trim().is_empty() {
        return Ok(LhmConfig::default());
    }
    serde_yaml::from_str(&content).map_err(|e| format!("failed to parse {}: {e}", path.display()))
}

/// Persist `cfg` to `config_dir/lhm.yaml`, creating the directory if needed.
pub fn save(config_dir: &Path, cfg: &LhmConfig) -> Result<(), String> {
    fs::create_dir_all(config_dir).map_err(|e| format!("failed to create {}: {e}", config_dir.display()))?;
    let path = lhm_config_path(config_dir);
    let content = serde_yaml::to_string(cfg).map_err(|e| format!("failed to serialize lhm config: {e}"))?;
    fs::write(&path, content).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

/// Return the URL of the `origin` remote for the repo at `root`. Returns
/// `None` if git fails or no `origin` remote is configured.
pub fn git_origin(root: &Path) -> Option<String> {
    let output = crate::git::command()
        .arg("-C")
        .arg(root)
        .args(["remote", "get-url", "origin"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Whether the repo at `root` is disabled per the lhm config in `config_dir`.
/// Fails open: returns `false` if the repo has no origin or the config can't
/// be loaded, so a corrupt config never blocks hooks.
pub fn is_repo_disabled(config_dir: &Path, root: &Path) -> bool {
    let Some(origin) = git_origin(root) else {
        return false;
    };
    match load(config_dir) {
        Ok(cfg) => {
            let disabled = cfg.is_disabled(&origin);
            if disabled {
                debug!(
                    "repo origin {origin} is disabled in {}",
                    lhm_config_path(config_dir).display()
                );
            }
            disabled
        }
        Err(e) => {
            debug!("failed to load lhm config: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo_with_origin(dir: &Path, url: &str) {
        let run = |args: &[&str]| {
            let status = crate::git::command()
                .arg("-C")
                .arg(dir)
                .args(args)
                .status()
                .expect("git invocation");
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["remote", "add", "origin", url]);
    }

    #[test]
    fn test_load_returns_default_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load(dir.path()).unwrap();
        assert!(cfg.disabled_repos.is_empty());
    }

    #[test]
    fn test_load_returns_default_when_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(lhm_config_path(dir.path()), "").unwrap();
        let cfg = load(dir.path()).unwrap();
        assert!(cfg.disabled_repos.is_empty());
    }

    #[test]
    fn test_save_then_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = LhmConfig::default();
        cfg.disable("git@github.com:foo/bar.git");
        cfg.disable("https://github.com/baz/qux");
        save(dir.path(), &cfg).unwrap();

        let loaded = load(dir.path()).unwrap();
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn test_save_creates_config_dir() {
        let parent = tempfile::tempdir().unwrap();
        let nested = parent.path().join("nested/config");
        let cfg = LhmConfig::default();
        save(&nested, &cfg).unwrap();
        assert!(nested.is_dir());
        assert!(lhm_config_path(&nested).is_file());
    }

    #[test]
    fn test_load_parse_error_propagates() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(lhm_config_path(dir.path()), "disabled_repos: not-a-list\n").unwrap();
        assert!(load(dir.path()).is_err());
    }

    #[test]
    fn test_disable_is_idempotent() {
        let mut cfg = LhmConfig::default();
        assert!(cfg.disable("git@github.com:foo/bar.git"));
        assert!(!cfg.disable("git@github.com:foo/bar.git"));
        assert_eq!(cfg.disabled_repos.len(), 1);
    }

    #[test]
    fn test_enable_removes_entry() {
        let mut cfg = LhmConfig::default();
        cfg.disable("git@github.com:foo/bar.git");
        assert!(cfg.enable("git@github.com:foo/bar.git"));
        assert!(!cfg.enable("git@github.com:foo/bar.git"));
        assert!(cfg.disabled_repos.is_empty());
    }

    #[test]
    fn test_is_disabled_lookup() {
        let mut cfg = LhmConfig::default();
        cfg.disable("a");
        assert!(cfg.is_disabled("a"));
        assert!(!cfg.is_disabled("b"));
    }

    #[test]
    fn test_serialized_yaml_omits_empty_set() {
        let cfg = LhmConfig::default();
        let s = serde_yaml::to_string(&cfg).unwrap();
        // skip_serializing_if keeps the empty file minimal
        assert!(!s.contains("disabled_repos"));
    }

    #[test]
    fn test_git_origin_returns_url() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_origin(dir.path(), "git@github.com:foo/bar.git");
        assert_eq!(git_origin(dir.path()).as_deref(), Some("git@github.com:foo/bar.git"));
    }

    #[test]
    fn test_git_origin_returns_none_when_no_remote() {
        let dir = tempfile::tempdir().unwrap();
        let status = crate::git::command()
            .arg("-C")
            .arg(dir.path())
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(status.success());
        assert!(git_origin(dir.path()).is_none());
    }

    #[test]
    fn test_git_origin_returns_none_outside_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(git_origin(dir.path()).is_none());
    }

    #[test]
    fn test_is_repo_disabled_true_when_origin_in_set() {
        let cfg_dir = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo_with_origin(repo.path(), "git@example.com:me/repo.git");

        let mut cfg = LhmConfig::default();
        cfg.disable("git@example.com:me/repo.git");
        save(cfg_dir.path(), &cfg).unwrap();

        assert!(is_repo_disabled(cfg_dir.path(), repo.path()));
    }

    #[test]
    fn test_is_repo_disabled_false_when_origin_absent() {
        let cfg_dir = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo_with_origin(repo.path(), "git@example.com:me/other.git");

        let mut cfg = LhmConfig::default();
        cfg.disable("git@example.com:me/repo.git");
        save(cfg_dir.path(), &cfg).unwrap();

        assert!(!is_repo_disabled(cfg_dir.path(), repo.path()));
    }

    #[test]
    fn test_is_repo_disabled_false_when_no_origin() {
        let cfg_dir = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let status = crate::git::command()
            .arg("-C")
            .arg(repo.path())
            .args(["init", "-q"])
            .status()
            .unwrap();
        assert!(status.success());

        let mut cfg = LhmConfig::default();
        cfg.disable("anything");
        save(cfg_dir.path(), &cfg).unwrap();

        assert!(!is_repo_disabled(cfg_dir.path(), repo.path()));
    }

    #[test]
    fn test_is_repo_disabled_false_when_config_missing() {
        let cfg_dir = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        init_repo_with_origin(repo.path(), "git@example.com:me/repo.git");
        assert!(!is_repo_disabled(cfg_dir.path(), repo.path()));
    }
}
