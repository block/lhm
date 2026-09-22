#![cfg(unix)]

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

const GIT_LOCAL_ENV_VARS: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

fn write_executable(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn run_git(repo: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(args);
    for variable in GIT_LOCAL_ENV_VARS {
        command.env_remove(variable);
    }
    let status = command.status().unwrap();
    assert!(status.success(), "git command failed: {args:?}");
}

fn run_lhm(repo: &Path, home: &Path, path: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lhm"))
        .current_dir(repo)
        .env("HOME", home)
        .env("PATH", path)
        .args(args)
        .output()
        .unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "lhm failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn disable_suppresses_lfs_in_dry_run_and_hook_execution() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    let config_dir = home.join(".config");
    let system_config_dir = temp.path().join("system-config");
    let bin_dir = temp.path().join("bin");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&config_dir).unwrap();
    fs::create_dir_all(&system_config_dir).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();

    run_git(&repo, &["init", "-q"]);
    run_git(&repo, &["remote", "add", "origin", "git@example.com:team/repo.git"]);
    fs::write(
        repo.join(".gitattributes"),
        "*.bin filter=lfs diff=lfs merge=lfs -text\n",
    )
    .unwrap();

    let user_config = config_dir.join("lefthook.yaml");
    fs::write(
        &user_config,
        "pre-push:\n  commands:\n    user-check:\n      run: echo user\n",
    )
    .unwrap();

    write_executable(&bin_dir.join("git-lfs"), "#!/bin/sh\nexit 0\n");
    write_executable(
        &bin_dir.join("lefthook"),
        "#!/bin/sh\ncp \"$LEFTHOOK_CONFIG\" \"$LHM_CAPTURE\"\n",
    );
    let path = env::join_paths(
        std::iter::once(bin_dir.clone()).chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap();
    let path = path.to_string_lossy();
    let common_args = [
        "--user-config",
        user_config.to_str().unwrap(),
        "--system-config",
        system_config_dir.to_str().unwrap(),
    ];

    let output = run_lhm(&repo, &home, &path, &["disable"]);
    assert_success(&output);

    let output = run_lhm(&repo, &home, &path, &[&common_args[..], &["dry-run"]].concat());
    assert_success(&output);
    let dry_run = String::from_utf8(output.stdout).unwrap();
    assert!(dry_run.contains("user-check"), "user hook should remain: {dry_run}");
    assert!(!dry_run.contains("git lfs"), "LFS should be disabled: {dry_run}");

    let captured_config = temp.path().join("hook-config.yaml");
    let output = Command::new(env!("CARGO_BIN_EXE_lhm"))
        .current_dir(&repo)
        .env("HOME", &home)
        .env("PATH", path.as_ref())
        .env("LHM_CAPTURE", &captured_config)
        .args(common_args)
        .args(["run-hook", "pre-push"])
        .output()
        .unwrap();
    assert_success(&output);
    let hook_config = fs::read_to_string(&captured_config).unwrap();
    assert!(
        hook_config.contains("user-check"),
        "user hook should remain: {hook_config}"
    );
    assert!(
        !hook_config.contains("git lfs"),
        "LFS should be disabled: {hook_config}"
    );

    let output = run_lhm(&repo, &home, &path, &["enable"]);
    assert_success(&output);
    let output = run_lhm(&repo, &home, &path, &[&common_args[..], &["dry-run"]].concat());
    assert_success(&output);
    let dry_run = String::from_utf8(output.stdout).unwrap();
    assert!(
        dry_run.contains("git lfs pre-push"),
        "LFS should be re-enabled: {dry_run}"
    );
}
