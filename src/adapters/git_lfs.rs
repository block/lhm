use log::debug;
use serde_yaml::Value;
use std::path::Path;
use std::process::Stdio;

use super::{Adapter, AdapterLayer};

/// Multi-line guidance printed when git-lfs is installed but its smudge/clean
/// filters aren't configured in the global git config.
const INSTALL_HINT: &str = "\
detected git-lfs but global filter.lfs.clean is not set\n\
lhm now handles git-lfs hooks for you in LFS-using repos\n\
run `git lfs install --skip-repo` once to configure LFS smudge/clean filters (no hooks)";

/// Adapter for [git-lfs](https://git-lfs.com).
///
/// Detects repos that use git-lfs and injects lefthook commands that invoke
/// `git lfs <hook> "$@"` for each LFS hook (`pre-push`, `post-checkout`,
/// `post-commit`, `post-merge`).
///
/// This adapter is an `Underlay`: it merges below the user-global and repo
/// layers so its commands run alongside whatever else the user has
/// configured, but can still be overridden by name (`git-lfs:` command) if
/// the user wants to skip or replace them.
pub struct GitLfsAdapter;

/// LFS hooks and the positional args git passes to each.
const LFS_HOOKS: &[&str] = &["pre-push", "post-checkout", "post-commit", "post-merge"];

impl Adapter for GitLfsAdapter {
    fn name(&self) -> &str {
        "git-lfs"
    }

    fn layer(&self) -> AdapterLayer {
        AdapterLayer::Underlay
    }

    fn detect(&self, root: &Path) -> bool {
        if !git_lfs_in_path() {
            return false;
        }
        repo_uses_lfs(root)
    }

    fn generate_config(&self, _root: &Path, hook_name: &str) -> Option<Value> {
        if !LFS_HOOKS.contains(&hook_name) {
            return None;
        }
        let yaml = format!("{hook_name}:\n  commands:\n    git-lfs:\n      run: git lfs {hook_name} {{0}}\n");
        serde_yaml::from_str(&yaml).ok()
    }

    fn install_hint(&self) -> Option<String> {
        if !git_lfs_in_path() {
            return None;
        }
        if global_filter_lfs_clean_set() {
            return None;
        }
        Some(INSTALL_HINT.to_string())
    }
}

/// `true` if `filter.lfs.clean` is set in the user's global git config
/// (i.e. `git lfs install` has been run at least once with default scope).
fn global_filter_lfs_clean_set() -> bool {
    crate::git::command()
        .args(["config", "--global", "--get", "filter.lfs.clean"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `true` if `git-lfs` is on PATH.
fn git_lfs_in_path() -> bool {
    which::which("git-lfs").is_ok()
}

/// `true` if the repo at `root` uses LFS. Checks the cheap signals:
/// - root `.gitattributes` mentions `filter=lfs`
/// - repo git config has any `lfs.*` keys
fn repo_uses_lfs(root: &Path) -> bool {
    if gitattributes_uses_lfs(root) {
        debug!("git-lfs: detected via .gitattributes");
        return true;
    }
    if repo_has_lfs_config(root) {
        debug!("git-lfs: detected via repo git config");
        return true;
    }
    false
}

/// Scan root-level `.gitattributes` for any line declaring an LFS filter.
fn gitattributes_uses_lfs(root: &Path) -> bool {
    let path = root.join(".gitattributes");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return false;
    };
    content
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .any(|l| l.contains("filter=lfs"))
}

/// `true` if the repo at `root` has any `lfs.*` entries in its local git config.
fn repo_has_lfs_config(root: &Path) -> bool {
    crate::git::command()
        .args(["-C"])
        .arg(root)
        .args(["config", "--local", "--get-regexp", "^lfs\\."])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_gitattributes_uses_lfs_positive() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".gitattributes"),
            "*.psd filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();
        assert!(gitattributes_uses_lfs(dir.path()));
    }

    #[test]
    fn test_gitattributes_uses_lfs_ignores_comments() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".gitattributes"),
            "# this is a comment about filter=lfs\n*.bin text\n",
        )
        .unwrap();
        assert!(!gitattributes_uses_lfs(dir.path()));
    }

    #[test]
    fn test_gitattributes_uses_lfs_no_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!gitattributes_uses_lfs(dir.path()));
    }

    #[test]
    fn test_gitattributes_uses_lfs_unrelated() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitattributes"), "*.txt text\n").unwrap();
        assert!(!gitattributes_uses_lfs(dir.path()));
    }

    #[test]
    fn test_generate_config_returns_none_for_non_lfs_hook() {
        let dir = tempfile::tempdir().unwrap();
        assert!(GitLfsAdapter.generate_config(dir.path(), "pre-commit").is_none());
        assert!(GitLfsAdapter.generate_config(dir.path(), "commit-msg").is_none());
    }

    #[test]
    fn test_generate_config_for_pre_push() {
        let dir = tempfile::tempdir().unwrap();
        let config = GitLfsAdapter.generate_config(dir.path(), "pre-push").unwrap();
        let out = serde_yaml::to_string(&config).unwrap();
        assert!(out.contains("pre-push:"), "has hook key: {out}");
        assert!(out.contains("git lfs pre-push {0}"), "invokes git lfs: {out}");
        assert!(out.contains("git-lfs:"), "names the command: {out}");
    }

    #[test]
    fn test_generate_config_for_post_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let config = GitLfsAdapter.generate_config(dir.path(), "post-checkout").unwrap();
        let out = serde_yaml::to_string(&config).unwrap();
        assert!(out.contains("git lfs post-checkout {0}"));
    }

    #[test]
    fn test_generate_config_for_post_commit() {
        let dir = tempfile::tempdir().unwrap();
        let config = GitLfsAdapter.generate_config(dir.path(), "post-commit").unwrap();
        let out = serde_yaml::to_string(&config).unwrap();
        assert!(out.contains("git lfs post-commit {0}"));
    }

    #[test]
    fn test_generate_config_for_post_merge() {
        let dir = tempfile::tempdir().unwrap();
        let config = GitLfsAdapter.generate_config(dir.path(), "post-merge").unwrap();
        let out = serde_yaml::to_string(&config).unwrap();
        assert!(out.contains("git lfs post-merge {0}"));
    }

    #[test]
    fn test_layer_is_underlay() {
        assert_eq!(GitLfsAdapter.layer(), AdapterLayer::Underlay);
    }

    #[test]
    fn test_install_hint_const_mentions_skip_repo() {
        // The hint string is fixed; verify its content here so the
        // user-facing message is locked down by tests. The full
        // "when does it fire" logic exercises subprocesses that depend on
        // the host environment and is covered manually / via integration
        // tests.
        assert!(
            INSTALL_HINT.contains("git lfs install --skip-repo"),
            "hint mentions the safe install command: {INSTALL_HINT}",
        );
        assert!(INSTALL_HINT.contains("filter.lfs.clean"));
    }
}
