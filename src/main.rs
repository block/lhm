mod adapters;
mod config;
mod hooks;
mod immutable;
mod merge;

use clap::{Parser, Subcommand};
use log::{debug, error, info};
use serde_yaml::Value;
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use config::{
    ConfigOverrides, install_default_global_config, load_global_config, read_yaml, repo_config, write_merged_temp,
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
    /// Remove global core.hooksPath, disabling lhm
    Disable,
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
        Commands::Disable => disable(),
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
            ExitCode::SUCCESS
        }
        _ => {
            error!("failed to set core.hooksPath");
            ExitCode::FAILURE
        }
    }
}

fn disable() -> ExitCode {
    let status = Command::new("git")
        .args(["config", "--global", "--unset", "core.hooksPath"])
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

/// Merge an ordered list of config layers (user, repo/adapter).
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

/// Resolve user-global, repo, and adapter sources into a single merged config.
fn resolve_config(
    global: &Option<Value>,
    repo: &Option<PathBuf>,
    adapter_config: &Option<Value>,
) -> Result<Option<Value>, String> {
    let repo_val = match repo {
        Some(r) => Some(read_yaml(r)?),
        None => None,
    };

    let local = repo_val.or_else(|| adapter_config.clone());

    let mut layers = Vec::with_capacity(2);
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
    let repo = root.as_deref().and_then(|r| repo_config(r, overrides));

    let adapter_config = if repo.is_none() {
        root.as_deref().and_then(|r| adapter_config_for(r, None))
    } else {
        None
    };

    if let Some(ref p) = repo {
        debug!("repo config: {}", p.display());
    }

    match resolve_config(&global, &repo, &adapter_config) {
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

    let user_config_dir = config_dir();
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

    let merged = match resolve_config(&global, &repo, &adapter_config) {
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
        let result = resolve_config(&global, &Some(repo_path), &None).unwrap().unwrap();
        let out = to_yaml(&result);
        assert!(out.contains("usr-test"), "user layer present: {out}");
        assert!(out.contains("repo-fmt"), "repo layer present: {out}");
    }

    #[test]
    fn test_resolve_config_global_only() {
        let global = Some(yaml("pre-push:\n  commands:\n    t:\n      run: usr\n"));
        let result = resolve_config(&global, &None, &None).unwrap().unwrap();
        let out = to_yaml(&result);
        assert!(out.contains("usr"), "global-only config: {out}");
    }

    #[test]
    fn test_resolve_config_none() {
        let result = resolve_config(&None, &None, &None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_resolve_config_adapter_used_when_no_repo() {
        let global = Some(yaml("pre-push:\n  commands:\n    t:\n      run: usr\n"));
        let adapter = Some(yaml("pre-commit:\n  commands:\n    fmt:\n      run: adapter-fmt\n"));
        let result = resolve_config(&global, &None, &adapter).unwrap().unwrap();
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
        let result = resolve_config(&None, &Some(repo_path), &adapter).unwrap().unwrap();
        let out = to_yaml(&result);
        assert!(out.contains("repo-fmt"), "repo wins over adapter: {out}");
        assert!(!out.contains("adapter-fmt"), "adapter ignored: {out}");
    }

    #[test]
    fn test_resolve_config_strips_no_tty_from_global_when_local_present() {
        let dir = tempfile::tempdir().unwrap();
        let repo_path = dir.path().join("lefthook.yml");
        fs::write(&repo_path, "pre-commit:\n  commands:\n    fmt:\n      run: repo-fmt\n").unwrap();

        let global = Some(yaml("no_tty: true\npre-push:\n  commands:\n    t:\n      run: usr\n"));
        let result = resolve_config(&global, &Some(repo_path), &None).unwrap().unwrap();
        let out = to_yaml(&result);
        assert!(!out.contains("no_tty"), "no_tty stripped from global: {out}");
        assert!(out.contains("usr"), "other keys preserved: {out}");
    }

    #[test]
    fn test_resolve_config_keeps_no_tty_in_global_when_no_local() {
        let global = Some(yaml("no_tty: true\npre-push:\n  commands:\n    t:\n      run: usr\n"));
        let result = resolve_config(&global, &None, &None).unwrap().unwrap();
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

        let result = resolve_config(&None, &Some(repo_path), &None).unwrap().unwrap();
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
}
