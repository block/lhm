mod adapters;
mod config;
mod hooks;
mod immutable;
mod lhm_config;
mod merge;

use clap::{Parser, Subcommand};
use log::{debug, error, info, warn};
use serde_yaml::Value;
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use config::{
    ConfigOverrides, find_config, install_default_global_config, load_global_config, read_yaml, repo_config,
    write_merged_temp,
};
use hooks::{GIT_HOOKS, annotate_hooks, create_hook_scripts, is_hook_name};
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
    version,
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
    Install,
    /// Print the merged config that would be used, then exit
    DryRun,
    /// Remove global core.hooksPath, uninstalling lhm
    Uninstall,
    /// Disable repo-specific hooks for the current repo (keyed by git origin)
    Disable {
        /// Also unset the repo's local `core.hooksPath` if set, so lhm is
        /// actually invoked for hooks instead of being bypassed.
        #[arg(long)]
        force: bool,
    },
    /// Re-enable repo-specific hooks for the current repo
    Enable,
    /// Write the adapter-generated lefthook config to `.lefthook.yaml` in the
    /// current repo. Errors if a lefthook config already exists or no adapter
    /// is detected.
    Import,
    /// Run a git hook by name (used by hook wrapper scripts)
    RunHook {
        /// The git hook to run (e.g. pre-commit, pre-push)
        hook: String,
        /// Arguments forwarded from git
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// Directory where lhm's wrapper hook scripts live (set as `core.hooksPath`).
fn hooks_dir() -> PathBuf {
    home_dir().join(".local/libexec/lhm/hooks")
}

/// Directory where lhm seeds the default user-level lefthook config.
fn config_dir() -> PathBuf {
    home_dir().join(".config")
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logger(cli.debug);
    let overrides = ConfigOverrides::new(cli.global_config, cli.local_config);
    match cli.command {
        Commands::Install => install(),
        Commands::DryRun => dry_run(&overrides),
        Commands::Uninstall => uninstall(),
        Commands::Disable { force } => disable(force),
        Commands::Enable => enable(),
        Commands::Import => import(),
        Commands::RunHook { hook, args } => {
            if !is_hook_name(&hook) {
                error!("unknown hook: {hook}");
                return ExitCode::FAILURE;
            }
            let overrides = ConfigOverrides::from_env();
            run_hook(&hook, args, &overrides)
        }
    }
}

fn home_dir() -> PathBuf {
    env::var("HOME").map(PathBuf::from).expect("HOME not set")
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

/// Return the repo-local `core.hooksPath` value if set, else `None`.
/// A local override silently bypasses lhm's global `core.hooksPath`, so this
/// is used to warn the user when lhm isn't actually being invoked.
fn local_hooks_path(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "--local", "--get", "core.hooksPath"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Unset the repo-local `core.hooksPath`. Idempotent: succeeds if the key is
/// already absent (git exits 5 in that case).
fn unset_local_hooks_path(root: &Path) -> Result<(), String> {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "--local", "--unset", "core.hooksPath"])
        .status()
        .map_err(|e| format!("failed to run git: {e}"))?;
    match status.code() {
        Some(0) | Some(5) => Ok(()),
        code => Err(format!("git config --unset core.hooksPath failed (exit {code:?})")),
    }
}

fn install() -> ExitCode {
    let dir = hooks_dir();
    let binary = env::current_exe().expect("cannot determine lhm binary path");
    debug!("hooks dir: {}", dir.display());
    debug!("binary path: {}", binary.display());

    if let Err(e) = install_default_global_config(&config_dir()) {
        error!("{e}");
        return ExitCode::FAILURE;
    }

    if let Err(e) = create_hook_scripts(&dir, &binary) {
        error!("{e}");
        return ExitCode::FAILURE;
    }

    let status = Command::new("git")
        .args(["config", "--global", "core.hooksPath"])
        .arg(&dir)
        .status();

    match status {
        Ok(s) if s.success() => {
            info!("installed hooks to {}", dir.display());
            info!("set core.hooksPath = {}", dir.display());
            print_adapter_install_hints();
            ExitCode::SUCCESS
        }
        _ => {
            error!("failed to set core.hooksPath");
            ExitCode::FAILURE
        }
    }
}

/// Ask every known adapter for any one-time setup guidance it wants the user
/// to see, and print each line through the standard logger.
fn print_adapter_install_hints() {
    for hint in adapters::install_hints() {
        for line in hint.lines() {
            info!("{line}");
        }
    }
}

fn uninstall() -> ExitCode {
    let status = Command::new("git")
        .args(["config", "--global", "--unset", "core.hooksPath"])
        .status();

    match status {
        Ok(s) if s.success() => {
            info!("removed core.hooksPath, lhm uninstalled");
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

/// Common scaffolding for `disable`/`enable`: resolve the current repo's
/// origin, load the lhm config, mutate it, and persist if changed.
fn mutate_disabled_repos<F>(action: &str, mutate: F) -> ExitCode
where
    F: FnOnce(&mut lhm_config::LhmConfig, &str) -> bool,
{
    let Some(root) = repo_root() else {
        error!("not in a git repository");
        return ExitCode::FAILURE;
    };
    let Some(origin) = lhm_config::git_origin(&root) else {
        error!("repo has no `origin` remote; cannot {action}");
        return ExitCode::FAILURE;
    };
    let dir = config_dir();
    let mut cfg = match lhm_config::load(&dir) {
        Ok(c) => c,
        Err(e) => {
            error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let changed = mutate(&mut cfg, &origin);
    if changed {
        if let Err(e) = lhm_config::save(&dir, &cfg) {
            error!("{e}");
            return ExitCode::FAILURE;
        }
        info!("{action}d repo-specific hooks for {origin}");
    } else {
        info!("repo-specific hooks already {action}d for {origin}");
    }
    ExitCode::SUCCESS
}

fn disable(force: bool) -> ExitCode {
    let result = mutate_disabled_repos("disable", |cfg, origin| cfg.disable(origin));
    if result != ExitCode::SUCCESS {
        return result;
    }
    let Some(root) = repo_root() else {
        return result;
    };
    let Some(path) = local_hooks_path(&root) else {
        return result;
    };
    if force {
        match unset_local_hooks_path(&root) {
            Ok(()) => {
                info!("unset local core.hooksPath (was {path}); lhm will now be invoked for hooks");
                ExitCode::SUCCESS
            }
            Err(e) => {
                error!("{e}");
                ExitCode::FAILURE
            }
        }
    } else {
        warn!(
            "this repo has a local core.hooksPath = {path}; lhm is being bypassed entirely, \
             so `disable` has no effect on hooks. Re-run with --force to also unset it."
        );
        result
    }
}

fn enable() -> ExitCode {
    mutate_disabled_repos("enable", |cfg, origin| cfg.enable(origin))
}

fn import() -> ExitCode {
    let Some(root) = repo_root() else {
        error!("not in a git repository");
        return ExitCode::FAILURE;
    };
    match import_for_repo(&root) {
        Ok(path) => {
            info!("wrote {}", path.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// Generate the repo-fallback adapter config for `root` and write it to
/// `<root>/.lefthook.yaml`. Errors when a lefthook config already exists in
/// the repo, when no `RepoFallback` adapter is detected, or on I/O failure.
fn import_for_repo(root: &Path) -> Result<PathBuf, String> {
    if let Some(existing) = find_config(root, true) {
        return Err(format!("lefthook config already exists: {}", existing.display()));
    }
    let Some(config) = adapter_config_for(root, None) else {
        return Err("no adapter detected for this repo; nothing to import".to_string());
    };
    let content = serde_yaml::to_string(&config).map_err(|e| format!("failed to serialize config: {e}"))?;
    let path = root.join(".lefthook.yaml");
    std::fs::write(&path, content).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(path)
}

/// Build the repo-fallback adapter config. Used in place of a missing
/// `lefthook.yaml`. Picks the first detected `RepoFallback` adapter and
/// returns its config for `hook_name` (or the merged config across all
/// hooks when `hook_name` is `None`).
fn adapter_config_for(root: &Path, hook_name: Option<&str>) -> Option<Value> {
    let adapter = adapters::detect_repo_fallback_adapter(root)?;
    debug!("detected adapter: {}", adapter.name());
    config_for_adapter(adapter.as_ref(), root, hook_name)
}

/// Build the merged `Underlay`-layer config. Every detected `Underlay`
/// adapter contributes; results are merged together. Returns `None` if no
/// underlay adapter applies or none contributes anything for the request.
fn underlay_config_for(root: &Path, hook_name: Option<&str>) -> Option<Value> {
    let adapters = adapters::detect_underlay_adapters(root);
    if adapters.is_empty() {
        return None;
    }

    let mut combined: Option<Value> = None;
    for adapter in &adapters {
        debug!("detected underlay adapter: {}", adapter.name());
        if let Some(config) = config_for_adapter(adapter.as_ref(), root, hook_name) {
            combined = Some(match combined {
                Some(existing) => merge_configs(existing, config),
                None => config,
            });
        }
    }
    combined
}

/// Produce an `annotate_hooks`-decorated config for one adapter, either for a
/// single hook or for the full `GIT_HOOKS` set when `hook_name` is `None`.
fn config_for_adapter(adapter: &dyn adapters::Adapter, root: &Path, hook_name: Option<&str>) -> Option<Value> {
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

/// Merge an ordered list of config layers (underlay, user, repo/adapter).
/// Later layers override earlier ones using `merge_configs`.
fn merge_layers(layers: Vec<Value>) -> Option<Value> {
    layers.into_iter().reduce(merge_configs)
}

/// Remove `no_tty` from a config value so it cannot leak from a global layer
/// into every repo. Repos that need it should set it themselves.
fn strip_no_tty(mut value: Value) -> Value {
    if let Value::Mapping(ref mut m) = value {
        m.remove("no_tty");
    }
    value
}

/// Resolve underlay-adapter, user-global, repo, and repo-fallback-adapter
/// sources into a single merged config.
fn resolve_config(
    underlay: &Option<Value>,
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
    if let Some(v) = underlay {
        layers.push(v.clone());
    }
    if let Some(v) = global {
        let v = if local.is_some() {
            strip_no_tty(v.clone())
        } else {
            v.clone()
        };
        layers.push(v);
    }
    if let Some(v) = local {
        layers.push(v);
    }

    Ok(merge_layers(layers))
}

fn dry_run(overrides: &ConfigOverrides) -> ExitCode {
    let user_config_dir = config_dir();
    let global = match load_global_config(&user_config_dir, overrides) {
        Ok(v) => v,
        Err(e) => {
            error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let root = repo_root();
    let disabled = root
        .as_deref()
        .is_some_and(|r| lhm_config::is_repo_disabled(&user_config_dir, r));
    let repo = if disabled {
        None
    } else {
        root.as_deref().and_then(|r| repo_config(r, overrides))
    };

    let adapter_config = if !disabled && repo.is_none() {
        root.as_deref().and_then(|r| adapter_config_for(r, None))
    } else {
        None
    };
    let underlay = root.as_deref().and_then(|r| underlay_config_for(r, None));

    if disabled {
        debug!("repo-specific hooks disabled; using global + underlay only");
    }
    if let Some(ref p) = repo {
        debug!("repo config: {}", p.display());
    }
    if let Some(r) = root.as_deref()
        && let Some(path) = local_hooks_path(r)
    {
        warn!(
            "this repo has a local core.hooksPath = {path}; lhm is being bypassed entirely. \
             The merged config below is informational only."
        );
    }

    match resolve_config(&underlay, &global, &repo, &adapter_config) {
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
    which::which("lefthook").is_ok()
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
        .env("LHM", "1")
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

    let user_config_dir = config_dir();
    let global = match load_global_config(&user_config_dir, overrides) {
        Ok(v) => v,
        Err(e) => {
            error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let root = repo_root();
    let disabled = root
        .as_deref()
        .is_some_and(|r| lhm_config::is_repo_disabled(&user_config_dir, r));
    let repo = if disabled {
        None
    } else {
        root.as_deref().and_then(|r| repo_config(r, overrides))
    };

    debug!("repo root: {:?}", root);
    debug!("repo config: {:?}", repo);
    if disabled {
        debug!("repo-specific hooks disabled; using global + underlay only");
    }

    let adapter_config = if !disabled && repo.is_none() {
        root.as_deref().and_then(|r| adapter_config_for(r, Some(hook_name)))
    } else {
        None
    };
    let underlay = root.as_deref().and_then(|r| underlay_config_for(r, Some(hook_name)));

    let merged = match resolve_config(&underlay, &global, &repo, &adapter_config) {
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
        .env("LHM", "1")
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
    use std::io::Write;
    use std::process::Command;

    /// Write a script file with executable permissions and explicit sync+close
    /// before returning, avoiding ETXTBSY on Linux.
    #[cfg(unix)]
    fn write_test_script(path: &Path, content: &str) {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(path)
            .unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.sync_all().unwrap();
    }

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
        write_test_script(&hook, "#!/bin/sh\nexit 0\n");

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
        write_test_script(&hook, "#!/bin/sh\nexit 1\n");

        let status = Command::new(&hook).status().expect("hook script should be executable");
        assert!(!status.success());
    }

    fn init_git_repo(dir: &Path) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["init", "-q"])
            .status()
            .expect("git init");
        assert!(status.success());
    }

    fn set_local_hooks_path(dir: &Path, value: &str) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["config", "--local", "core.hooksPath", value])
            .status()
            .expect("git config");
        assert!(status.success());
    }

    #[test]
    fn test_local_hooks_path_none_when_unset() {
        let dir = tempfile::tempdir().unwrap();
        init_git_repo(dir.path());
        assert!(local_hooks_path(dir.path()).is_none());
    }

    #[test]
    fn test_local_hooks_path_returns_value_when_set() {
        let dir = tempfile::tempdir().unwrap();
        init_git_repo(dir.path());
        set_local_hooks_path(dir.path(), ".husky/_");
        assert_eq!(local_hooks_path(dir.path()).as_deref(), Some(".husky/_"));
    }

    #[test]
    fn test_local_hooks_path_none_outside_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(local_hooks_path(dir.path()).is_none());
    }

    #[test]
    fn test_unset_local_hooks_path_removes_value() {
        let dir = tempfile::tempdir().unwrap();
        init_git_repo(dir.path());
        set_local_hooks_path(dir.path(), ".husky/_");
        assert!(local_hooks_path(dir.path()).is_some());

        unset_local_hooks_path(dir.path()).unwrap();
        assert!(local_hooks_path(dir.path()).is_none());
    }

    #[test]
    fn test_unset_local_hooks_path_idempotent_when_unset() {
        let dir = tempfile::tempdir().unwrap();
        init_git_repo(dir.path());
        // No core.hooksPath set: unset must still succeed.
        unset_local_hooks_path(dir.path()).unwrap();
    }

    #[test]
    fn test_hooks_dir_under_local() {
        assert!(hooks_dir().ends_with(".local/libexec/lhm/hooks"));
    }

    #[test]
    fn test_config_dir_under_home() {
        assert!(config_dir().ends_with(".config"));
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
    fn test_merge_layers_two_layers() {
        let user = yaml("pre-push:\n  commands:\n    usr:\n      run: usr-test\n");
        let repo = yaml("pre-push:\n  commands:\n    repo:\n      run: repo-test\n");
        let result = merge_layers(vec![user, repo]).unwrap();
        let out = to_yaml(&result);
        assert!(out.contains("usr-test"), "user preserved: {out}");
        assert!(out.contains("repo-test"), "repo preserved: {out}");
    }

    #[test]
    fn test_merge_layers_later_overrides_earlier() {
        let user = yaml("pre-push:\n  commands:\n    test:\n      run: usr-ver\n");
        let repo = yaml("pre-push:\n  commands:\n    test:\n      run: repo-ver\n");
        let result = merge_layers(vec![user, repo]).unwrap();
        let out = to_yaml(&result);
        assert!(out.contains("repo-ver"), "repo overrides user: {out}");
        assert!(!out.contains("usr-ver"), "user overridden: {out}");
    }

    #[test]
    fn test_resolve_config_both_layers() {
        let dir = tempfile::tempdir().unwrap();
        let repo_path = dir.path().join("lefthook.yml");
        fs::write(&repo_path, "pre-commit:\n  commands:\n    fmt:\n      run: repo-fmt\n").unwrap();

        let global = Some(yaml("pre-push:\n  commands:\n    usr-test:\n      run: usr-test\n"));
        let result = resolve_config(&None, &global, &Some(repo_path), &None)
            .unwrap()
            .unwrap();
        let out = to_yaml(&result);
        assert!(out.contains("usr-test"), "user layer present: {out}");
        assert!(out.contains("repo-fmt"), "repo layer present: {out}");
    }

    #[test]
    fn test_resolve_config_global_only() {
        let global = Some(yaml("pre-push:\n  commands:\n    t:\n      run: usr\n"));
        let result = resolve_config(&None, &global, &None, &None).unwrap().unwrap();
        let out = to_yaml(&result);
        assert!(out.contains("usr"), "global-only config: {out}");
    }

    #[test]
    fn test_resolve_config_none() {
        let result = resolve_config(&None, &None, &None, &None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_resolve_config_adapter_used_when_no_repo() {
        let global = Some(yaml("pre-push:\n  commands:\n    t:\n      run: usr\n"));
        let adapter = Some(yaml("pre-commit:\n  commands:\n    fmt:\n      run: adapter-fmt\n"));
        let result = resolve_config(&None, &global, &None, &adapter).unwrap().unwrap();
        let out = to_yaml(&result);
        assert!(out.contains("usr"), "global preserved: {out}");
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

    #[test]
    fn test_resolve_config_underlay_then_global_then_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo_path = dir.path().join("lefthook.yml");
        fs::write(
            &repo_path,
            "pre-push:\n  commands:\n    extra:\n      run: repo-extra\n",
        )
        .unwrap();

        let underlay = Some(yaml(
            "pre-push:\n  commands:\n    git-lfs:\n      run: git lfs pre-push {0}\n",
        ));
        let global = Some(yaml("pre-push:\n  commands:\n    t:\n      run: usr\n"));
        let result = resolve_config(&underlay, &global, &Some(repo_path), &None)
            .unwrap()
            .unwrap();
        let out = to_yaml(&result);
        assert!(out.contains("git lfs pre-push"), "underlay preserved: {out}");
        assert!(out.contains("usr"), "global preserved: {out}");
        assert!(out.contains("repo-extra"), "repo preserved: {out}");
    }

    #[test]
    fn test_resolve_config_repo_overrides_underlay() {
        let dir = tempfile::tempdir().unwrap();
        let repo_path = dir.path().join("lefthook.yml");
        fs::write(
            &repo_path,
            "pre-push:\n  commands:\n    git-lfs:\n      run: custom-lfs\n",
        )
        .unwrap();

        let underlay = Some(yaml(
            "pre-push:\n  commands:\n    git-lfs:\n      run: git lfs pre-push {0}\n",
        ));
        let result = resolve_config(&underlay, &None, &Some(repo_path), &None)
            .unwrap()
            .unwrap();
        let out = to_yaml(&result);
        assert!(out.contains("custom-lfs"), "repo overrides underlay: {out}");
        assert!(!out.contains("git lfs pre-push"), "underlay version replaced: {out}");
    }

    #[test]
    fn test_resolve_config_strips_no_tty_from_global_when_local_present() {
        let dir = tempfile::tempdir().unwrap();
        let repo_path = dir.path().join("lefthook.yml");
        fs::write(&repo_path, "pre-commit:\n  commands:\n    fmt:\n      run: repo-fmt\n").unwrap();

        let global = Some(yaml("no_tty: true\npre-push:\n  commands:\n    t:\n      run: usr\n"));
        let result = resolve_config(&None, &global, &Some(repo_path), &None)
            .unwrap()
            .unwrap();
        let out = to_yaml(&result);
        assert!(!out.contains("no_tty"), "no_tty stripped from global: {out}");
        assert!(out.contains("usr"), "other keys preserved: {out}");
    }

    #[test]
    fn test_resolve_config_keeps_no_tty_in_global_when_no_local() {
        let global = Some(yaml("no_tty: true\npre-push:\n  commands:\n    t:\n      run: usr\n"));
        let result = resolve_config(&None, &global, &None, &None).unwrap().unwrap();
        let out = to_yaml(&result);
        assert!(out.contains("no_tty: true"), "no_tty kept when no local: {out}");
    }

    #[test]
    fn test_resolve_config_no_tty_in_repo_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let repo_path = dir.path().join("lefthook.yml");
        fs::write(
            &repo_path,
            "no_tty: true\npre-commit:\n  commands:\n    fmt:\n      run: repo-fmt\n",
        )
        .unwrap();

        let result = resolve_config(&None, &None, &Some(repo_path), &None).unwrap().unwrap();
        let out = to_yaml(&result);
        assert!(out.contains("no_tty: true"), "repo no_tty preserved: {out}");
    }

    #[test]
    fn test_strip_no_tty() {
        let val = yaml("no_tty: true\noutput:\n  - success\n");
        let stripped = strip_no_tty(val);
        let out = to_yaml(&stripped);
        assert!(!out.contains("no_tty"), "no_tty removed: {out}");
        assert!(out.contains("success"), "other keys kept: {out}");
    }

    #[test]
    fn test_strip_no_tty_absent() {
        let val = yaml("output:\n  - success\n");
        let stripped = strip_no_tty(val.clone());
        assert_eq!(to_yaml(&stripped), to_yaml(&val));
    }

    #[test]
    fn test_import_for_repo_writes_adapter_config() {
        let dir = tempfile::tempdir().unwrap();
        // hooks-dir adapter: `.hooks/` with an executable pre-commit script.
        let hooks = dir.path().join(".hooks");
        fs::create_dir_all(&hooks).unwrap();
        write_test_script(&hooks.join("pre-commit"), "#!/bin/sh\nexit 0\n");

        let written = import_for_repo(dir.path()).expect("import should succeed");
        assert_eq!(written, dir.path().join(".lefthook.yaml"));

        let content = fs::read_to_string(&written).unwrap();
        assert!(content.contains("pre-commit:"), "hook present: {content}");
        assert!(content.contains(".hooks/pre-commit"), "script path present: {content}");
    }

    #[test]
    fn test_import_for_repo_errors_when_config_exists() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join(".hooks");
        fs::create_dir_all(&hooks).unwrap();
        write_test_script(&hooks.join("pre-commit"), "#!/bin/sh\nexit 0\n");
        let existing = dir.path().join("lefthook.yml");
        fs::write(&existing, "pre-commit:\n").unwrap();

        let err = import_for_repo(dir.path()).expect_err("should error when config exists");
        assert!(err.contains("already exists"), "err mentions existence: {err}");
        assert!(err.contains("lefthook.yml"), "err includes path: {err}");
        assert!(
            !dir.path().join(".lefthook.yaml").exists(),
            ".lefthook.yaml not written"
        );
    }

    #[test]
    fn test_import_for_repo_errors_when_dotted_config_exists() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join(".hooks");
        fs::create_dir_all(&hooks).unwrap();
        write_test_script(&hooks.join("pre-commit"), "#!/bin/sh\nexit 0\n");
        fs::write(dir.path().join(".lefthook.yaml"), "pre-commit:\n").unwrap();

        let err = import_for_repo(dir.path()).expect_err("should error when .lefthook.yaml exists");
        assert!(err.contains("already exists"), "err mentions existence: {err}");
    }

    #[test]
    fn test_import_for_repo_errors_when_no_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let err = import_for_repo(dir.path()).expect_err("should error when no adapter");
        assert!(err.contains("no adapter"), "err mentions missing adapter: {err}");
    }
}
