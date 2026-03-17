mod adapters;
mod config;
mod hooks;
mod merge;

use clap::{Parser, Subcommand};
use log::{debug, error, info};
use serde_yaml::Value;
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use config::{
    ConfigOverrides, install_default_global_config, load_global_config, load_system_config, read_yaml, repo_config,
    write_merged_temp,
};
use hooks::{GIT_HOOKS, annotate_hooks, create_hook_symlinks, is_hook_name};
use merge::merge_configs;

fn init_logger(cli_debug: bool) {
    let debug_enabled = cli_debug || env::var("LHM_DEBUG").is_ok_and(|v| v == "1" || v == "true");

    let level = if debug_enabled {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    };

    env_logger::Builder::new()
        .filter_level(level)
        .format(|buf, record| {
            use std::io::Write;
            match record.level() {
                log::Level::Debug => writeln!(buf, "lhm: debug: {}", record.args()),
                log::Level::Info => writeln!(buf, "lhm: {}", record.args()),
                _ => writeln!(
                    buf,
                    "lhm: {}: {}",
                    record.level().as_str().to_lowercase(),
                    record.args()
                ),
            }
        })
        .init();
}

#[derive(Parser)]
#[command(
    name = "lhm",
    about = "\
Merges global and per-repo lefthook configs.

When invoked as a git hook (via symlink), lhm finds the global config \
(~/.lefthook.yaml) and repo config ($REPO/lefthook.yaml), merges them, \
and runs lefthook. If neither config exists, falls back to \
the adapter system.

Supported config names: lefthook.<ext>, .lefthook.<ext>, .config/lefthook.<ext>
Supported extensions: yml, yaml, json, jsonc, toml"
)]
struct Cli {
    /// Enable debug logging (also via LHM_DEBUG=1)
    #[arg(long, global = true)]
    debug: bool,

    /// Path to the global lefthook config (also via LHM_GLOBAL_CONFIG)
    #[arg(long, global = true)]
    global_config: Option<PathBuf>,

    /// Path to the local (repo) lefthook config (also via LHM_LOCAL_CONFIG)
    #[arg(long, global = true)]
    local_config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Configure global core.hooksPath to use lhm
    Install {
        /// Install system-wide (requires root). Uses /etc for config and git config --system.
        #[arg(long)]
        system: bool,
    },
    /// Print the merged config that would be used, then exit
    DryRun,
    /// Remove global core.hooksPath, disabling lhm
    Disable {
        /// Remove system-wide core.hooksPath (requires root). Uses git config --system.
        #[arg(long)]
        system: bool,
    },
}

const SYSTEM_BASE: &str = "/usr/local";

/// Whether install/disable targets the user (~/.local) or system (/usr/local).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallScope {
    User,
    System,
}

impl InstallScope {
    fn base_dir(&self) -> PathBuf {
        match self {
            Self::User => home_dir().join(".local"),
            Self::System => PathBuf::from(SYSTEM_BASE),
        }
    }

    fn git_config_flag(&self) -> &'static str {
        match self {
            Self::User => "--global",
            Self::System => "--system",
        }
    }

    fn hooks_dir(&self) -> PathBuf {
        self.base_dir().join("libexec/lhm/hooks")
    }

    fn config_dir(&self) -> PathBuf {
        self.base_dir().join("etc")
    }
}

fn main() -> ExitCode {
    let invoked_as = invoked_name();

    if is_hook_name(&invoked_as) {
        init_logger(false);
        debug!("invoked as hook: {invoked_as}");
        let overrides = ConfigOverrides::from_env();
        return run_hook(&invoked_as, env::args().skip(1).collect(), &overrides);
    }

    let cli = Cli::parse();
    init_logger(cli.debug);
    let overrides = ConfigOverrides::new(cli.global_config, cli.local_config);
    match cli.command {
        Commands::Install { system } => {
            let scope = if system {
                InstallScope::System
            } else {
                InstallScope::User
            };
            if system && let Err(e) = require_root() {
                error!("{e}");
                return ExitCode::FAILURE;
            }
            install(scope)
        }
        Commands::DryRun => dry_run(&overrides),
        Commands::Disable { system } => {
            let scope = if system {
                InstallScope::System
            } else {
                InstallScope::User
            };
            if system && let Err(e) = require_root() {
                error!("{e}");
                return ExitCode::FAILURE;
            }
            disable(scope)
        }
    }
}

fn invoked_name() -> String {
    env::args()
        .next()
        .as_deref()
        .and_then(|s| Path::new(s).file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

fn home_dir() -> PathBuf {
    env::var("HOME").map(PathBuf::from).expect("HOME not set")
}

/// Abort unless running as root (euid == 0).
fn require_root() -> Result<(), String> {
    // SAFETY: getuid() is a simple syscall with no memory concerns
    if unsafe { libc::geteuid() } != 0 {
        return Err("--system requires root privileges (try sudo)".to_string());
    }
    Ok(())
}

fn repo_root() -> Option<PathBuf> {
    Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
}

fn install(scope: InstallScope) -> ExitCode {
    let dir = scope.hooks_dir();
    let git_flag = scope.git_config_flag();
    let binary = env::current_exe().expect("cannot determine lhm binary path");
    debug!("scope: {scope:?}");
    debug!("hooks dir: {}", dir.display());
    debug!("binary path: {}", binary.display());

    if let Err(e) = install_default_global_config(&scope.config_dir()) {
        error!("{e}");
        return ExitCode::FAILURE;
    }

    if let Err(e) = create_hook_symlinks(&dir, &binary) {
        error!("{e}");
        return ExitCode::FAILURE;
    }

    let status = Command::new("git")
        .args(["config", git_flag, "core.hooksPath"])
        .arg(&dir)
        .status();

    match status {
        Ok(s) if s.success() => {
            info!("installed hooks to {}", dir.display());
            info!("set core.hooksPath = {}", dir.display());
            ExitCode::SUCCESS
        }
        _ => {
            error!("failed to set core.hooksPath");
            ExitCode::FAILURE
        }
    }
}

fn disable(scope: InstallScope) -> ExitCode {
    let git_flag = scope.git_config_flag();
    let status = Command::new("git")
        .args(["config", git_flag, "--unset", "core.hooksPath"])
        .status();

    match status {
        Ok(s) if s.success() => {
            info!("removed core.hooksPath, lhm disabled");
            ExitCode::SUCCESS
        }
        Ok(s) if s.code() == Some(5) => {
            info!("core.hooksPath was not set, nothing to do");
            ExitCode::SUCCESS
        }
        _ => {
            error!("failed to unset core.hooksPath");
            ExitCode::FAILURE
        }
    }
}

fn adapter_config_for(root: &Path, hook_name: Option<&str>) -> Option<Value> {
    let adapter = adapters::detect_adapter(root)?;
    debug!("detected adapter: {}", adapter.name());

    if let Some(name) = hook_name {
        let config = adapter.generate_config(root, name);
        if config.is_none() {
            debug!("adapter {} has no config for {name}", adapter.name());
        }
        return config.map(annotate_hooks);
    }

    let mut combined: Option<Value> = None;
    for name in GIT_HOOKS {
        if let Some(config) = adapter.generate_config(root, name) {
            combined = Some(match combined {
                Some(existing) => merge_configs(existing, config),
                None => config,
            });
        }
    }
    combined.map(annotate_hooks)
}

/// Merge an ordered list of config layers (system, user, repo/adapter).
/// Later layers override earlier ones using `merge_configs`.
fn merge_layers(layers: Vec<Value>) -> Option<Value> {
    layers.into_iter().reduce(merge_configs)
}

/// Resolve system, user-global, repo, and adapter sources into a single merged config.
fn resolve_config(
    system: &Option<Value>,
    global: &Option<Value>,
    repo: &Option<PathBuf>,
    adapter_config: &Option<Value>,
) -> Result<Option<Value>, String> {
    let repo_val = match repo {
        Some(r) => Some(read_yaml(r)?),
        None => None,
    };

    let local = repo_val.or_else(|| adapter_config.clone());

    let mut layers = Vec::with_capacity(3);
    if let Some(v) = system {
        layers.push(v.clone());
    }
    if let Some(v) = global {
        layers.push(v.clone());
    }
    if let Some(v) = local {
        layers.push(v);
    }

    Ok(merge_layers(layers))
}

fn dry_run(overrides: &ConfigOverrides) -> ExitCode {
    let system = match load_system_config() {
        Ok(v) => v,
        Err(e) => {
            error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let user_config_dir = InstallScope::User.config_dir();
    let global = match load_global_config(&user_config_dir, overrides) {
        Ok(v) => v,
        Err(e) => {
            error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let root = repo_root();
    let repo = root.as_deref().and_then(|r| repo_config(r, overrides));

    let adapter_config = if repo.is_none() {
        root.as_deref().and_then(|r| adapter_config_for(r, None))
    } else {
        None
    };

    if let Some(ref p) = repo {
        debug!("repo config: {}", p.display());
    }

    match resolve_config(&system, &global, &repo, &adapter_config) {
        Ok(Some(config)) => {
            print!("{}", serde_yaml::to_string(&config).unwrap_or_default());
            ExitCode::SUCCESS
        }
        Ok(None) => {
            debug!("no config to display");
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn lefthook_in_path() -> bool {
    Command::new("lefthook")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// Run the repo's `.git/hooks/<hook_name>` script directly.
/// Returns SUCCESS if the script doesn't exist (no hook to run).
fn run_git_hook(hook_name: &str, args: Vec<String>) -> ExitCode {
    let root = match repo_root() {
        Some(r) => r,
        None => return ExitCode::SUCCESS,
    };
    let hook_path = root.join(".git/hooks").join(hook_name);
    if !hook_path.is_file() {
        debug!("no .git/hooks/{hook_name} found, skipping");
        return ExitCode::SUCCESS;
    }
    debug!("running .git/hooks/{hook_name} directly (lefthook not in PATH)");
    let status = Command::new(&hook_path)
        .args(&args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    match status {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(e) => {
            error!("failed to run .git/hooks/{hook_name}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_hook(hook_name: &str, args: Vec<String>, overrides: &ConfigOverrides) -> ExitCode {
    if !lefthook_in_path() {
        debug!("lefthook not found in PATH, falling back to .git/hooks");
        return run_git_hook(hook_name, args);
    }

    let system = match load_system_config() {
        Ok(v) => v,
        Err(e) => {
            error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let user_config_dir = InstallScope::User.config_dir();
    let global = match load_global_config(&user_config_dir, overrides) {
        Ok(v) => v,
        Err(e) => {
            error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let root = repo_root();
    let repo = root.as_deref().and_then(|r| repo_config(r, overrides));

    debug!("repo root: {:?}", root);
    debug!("repo config: {:?}", repo);

    let adapter_config = if repo.is_none() {
        root.as_deref().and_then(|r| adapter_config_for(r, Some(hook_name)))
    } else {
        None
    };

    let merged = match resolve_config(&system, &global, &repo, &adapter_config) {
        Ok(Some(m)) => m,
        Ok(None) => {
            debug!("no config found, skipping hook");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let _temp = match write_merged_temp(merged) {
        Ok(t) => t,
        Err(e) => {
            error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let config_path = _temp.path();

    debug!("LEFTHOOK_CONFIG={}", config_path.display());
    debug!("running: lefthook run {hook_name} --no-auto-install {}", args.join(" "));

    let status = Command::new("lefthook")
        .arg("run")
        .arg(hook_name)
        .arg("--no-auto-install")
        .args(&args)
        .env("LEFTHOOK_CONFIG", config_path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();

    match status {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(e) => {
            error!("failed to run lefthook: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn yaml(s: &str) -> Value {
        serde_yaml::from_str(s).unwrap()
    }

    fn to_yaml(v: &Value) -> String {
        serde_yaml::to_string(v).unwrap()
    }

    #[test]
    fn test_run_git_hook_executes_script() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join(".git/hooks");
        fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let status = Command::new(&hook).status().expect("hook script should be executable");
        assert!(status.success());
    }

    #[test]
    fn test_run_git_hook_missing_hook_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let hook_path = dir.path().join(".git/hooks/pre-commit");
        assert!(!hook_path.exists());
    }

    #[test]
    fn test_run_git_hook_failing_script() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join(".git/hooks");
        fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let status = Command::new(&hook).status().expect("hook script should be executable");
        assert!(!status.success());
    }

    #[test]
    fn test_install_scope_user_git_flag() {
        assert_eq!(InstallScope::User.git_config_flag(), "--global");
    }

    #[test]
    fn test_install_scope_system_git_flag() {
        assert_eq!(InstallScope::System.git_config_flag(), "--system");
    }

    #[test]
    fn test_install_scope_system_hooks_dir() {
        let dir = InstallScope::System.hooks_dir();
        assert_eq!(dir, PathBuf::from("/usr/local/libexec/lhm/hooks"));
    }

    #[test]
    fn test_install_scope_user_hooks_dir_under_local() {
        let dir = InstallScope::User.hooks_dir();
        assert!(dir.ends_with(".local/libexec/lhm/hooks"));
    }

    #[test]
    fn test_install_scope_system_config_dir() {
        assert_eq!(InstallScope::System.config_dir(), PathBuf::from("/usr/local/etc"));
    }

    #[test]
    fn test_install_scope_user_config_dir() {
        assert!(InstallScope::User.config_dir().ends_with(".local/etc"));
    }

    #[test]
    fn test_install_scope_base_dir_shared_structure() {
        for scope in [InstallScope::User, InstallScope::System] {
            let base = scope.base_dir();
            assert_eq!(scope.hooks_dir(), base.join("libexec/lhm/hooks"));
            assert_eq!(scope.config_dir(), base.join("etc"));
        }
    }

    #[test]
    fn test_require_root_fails_for_non_root() {
        // Tests typically run as non-root
        if unsafe { libc::geteuid() } != 0 {
            assert!(require_root().is_err());
        }
    }

    #[test]
    fn test_merge_layers_empty() {
        assert!(merge_layers(vec![]).is_none());
    }

    #[test]
    fn test_merge_layers_single() {
        let v = yaml("pre-push:\n  commands:\n    test:\n      run: echo hi\n");
        let result = merge_layers(vec![v.clone()]);
        assert_eq!(result, Some(v));
    }

    #[test]
    fn test_merge_layers_three_layers() {
        let system = yaml("pre-push:\n  commands:\n    sys:\n      run: sys-test\n");
        let user = yaml("pre-push:\n  commands:\n    usr:\n      run: usr-test\n");
        let repo = yaml("pre-push:\n  commands:\n    repo:\n      run: repo-test\n");
        let result = merge_layers(vec![system, user, repo]).unwrap();
        let out = to_yaml(&result);
        assert!(out.contains("sys-test"), "system preserved: {out}");
        assert!(out.contains("usr-test"), "user preserved: {out}");
        assert!(out.contains("repo-test"), "repo preserved: {out}");
    }

    #[test]
    fn test_merge_layers_later_overrides_earlier() {
        let system = yaml("pre-push:\n  commands:\n    test:\n      run: sys-ver\n");
        let user = yaml("pre-push:\n  commands:\n    test:\n      run: usr-ver\n");
        let result = merge_layers(vec![system, user]).unwrap();
        let out = to_yaml(&result);
        assert!(out.contains("usr-ver"), "user overrides system: {out}");
        assert!(!out.contains("sys-ver"), "system overridden: {out}");
    }

    #[test]
    fn test_resolve_config_all_three_layers() {
        let dir = tempfile::tempdir().unwrap();
        let repo_path = dir.path().join("lefthook.yml");
        fs::write(&repo_path, "pre-commit:\n  commands:\n    fmt:\n      run: repo-fmt\n").unwrap();

        let system = Some(yaml("pre-push:\n  commands:\n    sys-lint:\n      run: sys-lint\n"));
        let global = Some(yaml("pre-push:\n  commands:\n    usr-test:\n      run: usr-test\n"));
        let result = resolve_config(&system, &global, &Some(repo_path), &None)
            .unwrap()
            .unwrap();
        let out = to_yaml(&result);
        assert!(out.contains("sys-lint"), "system layer present: {out}");
        assert!(out.contains("usr-test"), "user layer present: {out}");
        assert!(out.contains("repo-fmt"), "repo layer present: {out}");
    }

    #[test]
    fn test_resolve_config_system_only() {
        let system = Some(yaml("pre-push:\n  commands:\n    t:\n      run: sys\n"));
        let result = resolve_config(&system, &None, &None, &None).unwrap().unwrap();
        let out = to_yaml(&result);
        assert!(out.contains("sys"), "system-only config: {out}");
    }

    #[test]
    fn test_resolve_config_none() {
        let result = resolve_config(&None, &None, &None, &None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_resolve_config_adapter_used_when_no_repo() {
        let system = Some(yaml("pre-push:\n  commands:\n    t:\n      run: sys\n"));
        let adapter = Some(yaml("pre-commit:\n  commands:\n    fmt:\n      run: adapter-fmt\n"));
        let result = resolve_config(&system, &None, &None, &adapter).unwrap().unwrap();
        let out = to_yaml(&result);
        assert!(out.contains("sys"), "system preserved: {out}");
        assert!(out.contains("adapter-fmt"), "adapter used: {out}");
    }

    #[test]
    fn test_resolve_config_repo_beats_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let repo_path = dir.path().join("lefthook.yml");
        fs::write(&repo_path, "pre-commit:\n  commands:\n    fmt:\n      run: repo-fmt\n").unwrap();

        let adapter = Some(yaml("pre-commit:\n  commands:\n    fmt:\n      run: adapter-fmt\n"));
        let result = resolve_config(&None, &None, &Some(repo_path), &adapter)
            .unwrap()
            .unwrap();
        let out = to_yaml(&result);
        assert!(out.contains("repo-fmt"), "repo wins over adapter: {out}");
        assert!(!out.contains("adapter-fmt"), "adapter ignored: {out}");
    }
}
