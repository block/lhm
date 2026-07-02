//! Invoke `git` with a sanitized environment.

use std::ffi::{OsStr, OsString};
use std::process::Command;

/// Build a `git` command with every `GIT_*` environment variable removed.
///
/// lhm shells out to git to read and modify repository state. When lhm runs as
/// a git hook — or when its own test suite is driven by an outer `git`
/// invocation — git exports variables such as `GIT_DIR`, `GIT_WORK_TREE`, and
/// `GIT_CONFIG_PARAMETERS` (the last carries `-c` overrides like
/// `url.<base>.insteadOf`). Inherited by our child `git` processes, they
/// silently redirect which repository is queried or rewrite the URLs git
/// reports, so a command no longer reflects the directory we asked about.
///
/// Clearing the entire `GIT_` namespace — rather than a hand-maintained list of
/// specific variables — makes every git call depend only on the arguments we
/// pass.
pub fn command() -> Command {
    with_git_env_removed(Command::new("git"), std::env::vars_os().map(|(key, _)| key))
}

/// Remove every `GIT_*` variable in `keys` from `cmd`'s environment. Split out
/// from [`command`] so the filtering can be tested without mutating the real
/// process environment.
fn with_git_env_removed(mut cmd: Command, keys: impl Iterator<Item = OsString>) -> Command {
    for key in keys.filter(|key| is_git_var(key)) {
        cmd.env_remove(&key);
    }
    cmd
}

/// `true` if `key` names a variable in git's `GIT_` namespace. A bare `GIT` and
/// unrelated vars like `GITHUB_TOKEN` are deliberately excluded.
fn is_git_var(key: &OsStr) -> bool {
    key.as_encoded_bytes().starts_with(b"GIT_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_git_var() {
        assert!(is_git_var(OsStr::new("GIT_DIR")));
        assert!(is_git_var(OsStr::new("GIT_CONFIG_PARAMETERS")));
        assert!(is_git_var(OsStr::new("GIT_WORK_TREE")));
        assert!(is_git_var(OsStr::new("GIT_")));
        // Not the git namespace:
        assert!(!is_git_var(OsStr::new("GIT")));
        assert!(!is_git_var(OsStr::new("GITHUB_TOKEN")));
        assert!(!is_git_var(OsStr::new("PATH")));
    }

    #[test]
    fn test_command_removes_only_git_vars() {
        let keys = ["GIT_DIR", "GIT_CONFIG_PARAMETERS", "PATH", "GITHUB_TOKEN"]
            .into_iter()
            .map(OsString::from);
        let cmd = with_git_env_removed(Command::new("git"), keys);

        // `env_remove` shows up in `get_envs` as a `None` value for the key.
        let removed: Vec<OsString> = cmd
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_owned())
            .collect();

        assert!(removed.contains(&OsString::from("GIT_DIR")));
        assert!(removed.contains(&OsString::from("GIT_CONFIG_PARAMETERS")));
        assert!(!removed.contains(&OsString::from("PATH")));
        assert!(!removed.contains(&OsString::from("GITHUB_TOKEN")));
    }
}
