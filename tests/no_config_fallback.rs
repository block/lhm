//! Integration tests verifying that `lhm run-hook` falls back to `.git/hooks/`
//! when no lefthook configuration (system, global, repo, or adapter) is found.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// Build the lhm binary (debug) and return its path.
fn lhm_binary() -> std::path::PathBuf {
    let status = Command::new("cargo")
        .args(["build", "--quiet"])
        .status()
        .expect("cargo build failed");
    assert!(status.success(), "cargo build must succeed");

    let mut path = std::env::current_dir().unwrap();
    path.push("target/debug/lhm");
    assert!(path.is_file(), "binary must exist at {}", path.display());
    path
}

/// Create a minimal git repo in `dir` (git init + initial commit).
fn init_git_repo(dir: &Path) {
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-b", "main"]);
    run(&["commit", "--allow-empty", "-m", "init"]);
}

/// Write an executable shell script at `path`.
fn write_script(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    // Sync to avoid ETXTBSY on Linux.
    let f = fs::File::open(path).unwrap();
    f.sync_all().unwrap();
}

/// Create a directory containing a fake `lefthook` binary that only responds to
/// `--version` (so `lefthook_in_path()` returns true) but is never actually used
/// for running hooks.
fn fake_lefthook_dir(dir: &Path) -> std::path::PathBuf {
    let bin_dir = dir.join("fake-bin");
    fs::create_dir_all(&bin_dir).unwrap();
    write_script(&bin_dir.join("lefthook"), "#!/bin/sh\necho 'lefthook (fake) 0.0.0'\n");
    bin_dir
}

/// Build a PATH that puts `extra_dir` first, keeps `git` accessible, but
/// removes any real `lefthook` that might be installed on the host.
fn build_path(extra_dir: &Path) -> String {
    let system_path = std::env::var("PATH").unwrap_or_default();
    format!("{}:{}", extra_dir.display(), system_path)
}

#[test]
fn no_config_falls_back_to_git_hooks() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    // Place a hook that writes a marker file when executed.
    let marker = tmp.path().join("hook-ran");
    write_script(
        &repo.join(".git/hooks/pre-commit"),
        &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    );

    // Empty HOME so no global config is found.
    let fake_home = tmp.path().join("home");
    fs::create_dir_all(&fake_home).unwrap();

    let fake_bin = fake_lefthook_dir(tmp.path());
    let binary = lhm_binary();

    let output = Command::new(&binary)
        .args(["run-hook", "pre-commit"])
        .current_dir(&repo)
        .env("HOME", &fake_home)
        .env("XDG_CONFIG_HOME", fake_home.join(".config"))
        .env("PATH", build_path(&fake_bin))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("failed to run lhm");

    assert!(
        output.status.success(),
        "lhm run-hook should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        marker.exists(),
        ".git/hooks/pre-commit should have been executed (marker file missing). stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn no_config_no_git_hook_still_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    // No hook in .git/hooks/, no config anywhere.
    let fake_home = tmp.path().join("home");
    fs::create_dir_all(&fake_home).unwrap();

    let fake_bin = fake_lefthook_dir(tmp.path());
    let binary = lhm_binary();

    let output = Command::new(&binary)
        .args(["run-hook", "pre-commit"])
        .current_dir(&repo)
        .env("HOME", &fake_home)
        .env("XDG_CONFIG_HOME", fake_home.join(".config"))
        .env("PATH", build_path(&fake_bin))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("failed to run lhm");

    assert!(
        output.status.success(),
        "lhm run-hook should succeed even with no hook and no config, stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn no_config_falls_back_in_linked_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    // Place a hook in the main repo's `.git/hooks/` (the common git dir).
    let marker = tmp.path().join("hook-ran");
    write_script(
        &repo.join(".git/hooks/pre-commit"),
        &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    );

    // Create a linked worktree; inside it `.git` is a file, not a directory.
    let worktree = tmp.path().join("wt");
    let out = Command::new("git")
        .args(["worktree", "add", "-b", "wt-branch"])
        .arg(&worktree)
        .current_dir(&repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        worktree.join(".git").is_file(),
        "linked worktree's .git should be a file",
    );

    let fake_home = tmp.path().join("home");
    fs::create_dir_all(&fake_home).unwrap();
    let fake_bin = fake_lefthook_dir(tmp.path());
    let binary = lhm_binary();

    let output = Command::new(&binary)
        .args(["run-hook", "pre-commit"])
        .current_dir(&worktree)
        .env("HOME", &fake_home)
        .env("XDG_CONFIG_HOME", fake_home.join(".config"))
        .env("PATH", build_path(&fake_bin))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("failed to run lhm");

    assert!(
        output.status.success(),
        "lhm run-hook should succeed in worktree, stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        marker.exists(),
        "hook should have been executed via common git dir from worktree. stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn no_config_failing_git_hook_propagates_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    // Place a hook that exits with failure.
    write_script(&repo.join(".git/hooks/pre-commit"), "#!/bin/sh\nexit 1\n");

    let fake_home = tmp.path().join("home");
    fs::create_dir_all(&fake_home).unwrap();

    let fake_bin = fake_lefthook_dir(tmp.path());
    let binary = lhm_binary();

    let output = Command::new(&binary)
        .args(["run-hook", "pre-commit"])
        .current_dir(&repo)
        .env("HOME", &fake_home)
        .env("XDG_CONFIG_HOME", fake_home.join(".config"))
        .env("PATH", build_path(&fake_bin))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("failed to run lhm");

    assert!(
        !output.status.success(),
        "lhm run-hook should propagate .git/hooks/ failure",
    );
}
