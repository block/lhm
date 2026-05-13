mod git_lfs;
mod hooks_dir;
mod husky;
mod pre_commit;

use serde_yaml::Value;
use std::path::Path;

pub use git_lfs::GitLfsAdapter;
pub use hooks_dir::HooksDirAdapter;
pub use husky::HuskyAdapter;
pub use pre_commit::PreCommitAdapter;

/// How an adapter slots into the config-merge pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterLayer {
    /// Replaces a missing repo `lefthook.yaml`. Only the first detected
    /// `RepoFallback` adapter is used; never applied when the repo has its
    /// own lefthook config.
    RepoFallback,
    /// Always-on baseline that sits below the user-global layer. Every
    /// detected `Underlay` adapter contributes, and user/repo configs can
    /// override anything it generates.
    Underlay,
}

/// Adapter for translating a third-party tool's git hooks into lefthook config.
///
/// `RepoFallback` adapters (pre-commit, husky, hooks-dir) detect a repo-level
/// hook manager and stand in when there's no native lefthook config.
/// `Underlay` adapters (git-lfs) detect tools that always need to run
/// regardless of the repo's own hook manager.
pub trait Adapter {
    /// Human-readable name of this adapter (e.g. "pre-commit", "git-lfs").
    fn name(&self) -> &str;

    /// Which merge layer this adapter participates in.
    fn layer(&self) -> AdapterLayer {
        AdapterLayer::RepoFallback
    }

    /// Returns `true` if this adapter's tool is present and relevant for `root`.
    fn detect(&self, root: &Path) -> bool;

    /// Generate a lefthook config `Value` for the given hook name.
    ///
    /// Returns `None` if this adapter has nothing to run for the given hook
    /// (e.g. no matching hook script exists).
    fn generate_config(&self, root: &Path, hook_name: &str) -> Option<Value>;

    /// Guidance to print at the end of `lhm install` when this adapter wants
    /// the user to take a one-time action (e.g. configure git-lfs filters,
    /// remove a stale repo-local hook). Returns `None` when no action is
    /// needed. Called with looser criteria than `detect` because `lhm install`
    /// runs outside any particular repo.
    fn install_hint(&self) -> Option<String> {
        None
    }
}

/// All known adapters in priority order. `RepoFallback` order matters for
/// first-match-wins; `Underlay` order doesn't affect correctness because all
/// detected `Underlay` adapters are merged together.
fn all_adapters() -> Vec<Box<dyn Adapter>> {
    vec![
        Box::new(PreCommitAdapter),
        Box::new(HuskyAdapter),
        Box::new(HooksDirAdapter),
        Box::new(GitLfsAdapter),
    ]
}

/// Detect the first applicable `RepoFallback` adapter for the given repo root.
pub fn detect_repo_fallback_adapter(root: &Path) -> Option<Box<dyn Adapter>> {
    all_adapters()
        .into_iter()
        .filter(|a| a.layer() == AdapterLayer::RepoFallback)
        .find(|a| a.detect(root))
}

/// Detect every applicable `Underlay` adapter for the given repo root.
pub fn detect_underlay_adapters(root: &Path) -> Vec<Box<dyn Adapter>> {
    all_adapters()
        .into_iter()
        .filter(|a| a.layer() == AdapterLayer::Underlay)
        .filter(|a| a.detect(root))
        .collect()
}

/// Collect `install_hint`s from every known adapter, regardless of layer or
/// repo-level detection. Used by `lhm install` to surface one-time setup
/// guidance.
pub fn install_hints() -> Vec<String> {
    all_adapters().iter().filter_map(|a| a.install_hint()).collect()
}

#[cfg(test)]
pub(super) mod test_support {
    //! Test helpers shared across adapter modules.
    use std::env;
    use std::ffi::OsString;
    use std::sync::{Mutex, MutexGuard};

    /// Process-wide guard against concurrent `PATH` mutation. `which::which`
    /// reads `PATH` on every call, so PATH-mutating tests must serialize.
    pub static PATH_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard that prepends a directory containing a stub executable
    /// to `PATH` for the duration of one test. Holds the process-wide
    /// `PATH_LOCK` to prevent concurrent tests from racing.
    pub struct StubBinary {
        _dir: tempfile::TempDir,
        original_path: OsString,
        _lock: MutexGuard<'static, ()>,
    }

    impl StubBinary {
        /// Create a stub executable named `binary` (a no-op shell script)
        /// in a temporary directory and prepend that directory to `PATH`.
        #[cfg(unix)]
        pub fn create(binary: &str) -> Self {
            use std::fs;
            use std::os::unix::fs::PermissionsExt;

            let lock = PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(binary);
            fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();

            let original_path = env::var_os("PATH").unwrap_or_default();
            let mut new_path = OsString::from(dir.path());
            new_path.push(":");
            new_path.push(&original_path);
            // SAFETY: test-only, serialized by `PATH_LOCK`.
            unsafe { env::set_var("PATH", &new_path) };

            Self {
                _dir: dir,
                original_path,
                _lock: lock,
            }
        }
    }

    impl Drop for StubBinary {
        fn drop(&mut self) {
            // SAFETY: test-only, serialized by `PATH_LOCK`.
            unsafe { env::set_var("PATH", &self.original_path) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_detect_adapter_pre_commit() {
        let _stub = test_support::StubBinary::create("pre-commit");
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".pre-commit-config.yaml"), "repos: []\n").unwrap();
        let adapter = detect_repo_fallback_adapter(dir.path()).unwrap();
        assert_eq!(adapter.name(), "pre-commit");
    }

    #[test]
    fn test_detect_adapter_husky() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".husky")).unwrap();
        let adapter = detect_repo_fallback_adapter(dir.path()).unwrap();
        assert_eq!(adapter.name(), "husky");
    }

    #[test]
    fn test_detect_adapter_hooks_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".hooks")).unwrap();
        let adapter = detect_repo_fallback_adapter(dir.path()).unwrap();
        assert_eq!(adapter.name(), "hooks-dir");
    }

    #[test]
    fn test_detect_adapter_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_repo_fallback_adapter(dir.path()).is_none());
    }

    #[test]
    fn test_detect_adapter_priority_pre_commit_over_husky() {
        let _stub = test_support::StubBinary::create("pre-commit");
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".pre-commit-config.yaml"), "repos: []\n").unwrap();
        fs::create_dir_all(dir.path().join(".husky")).unwrap();
        let adapter = detect_repo_fallback_adapter(dir.path()).unwrap();
        assert_eq!(adapter.name(), "pre-commit");
    }

    #[test]
    fn test_detect_adapter_falls_through_when_pre_commit_missing() {
        // .pre-commit-config.yaml + .husky/, but no `pre-commit` binary in
        // PATH — lhm should fall through to the husky adapter rather than
        // pick pre-commit (which would then fail at runtime). Hold the PATH
        // lock to keep concurrent StubBinary tests from injecting a stub.
        let _path_lock = test_support::PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".pre-commit-config.yaml"), "repos: []\n").unwrap();
        fs::create_dir_all(dir.path().join(".husky")).unwrap();
        if which::which("pre-commit").is_ok() {
            // Skip when the host happens to have pre-commit installed.
            return;
        }
        let adapter = detect_repo_fallback_adapter(dir.path()).unwrap();
        assert_eq!(adapter.name(), "husky");
    }

    #[test]
    fn test_underlay_excluded_from_repo_fallback_detection() {
        // A tempdir with nothing repo-fallback-relevant must not return
        // GitLfsAdapter even if git-lfs is in PATH and somehow detected.
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_repo_fallback_adapter(dir.path()).is_none());
    }
}
