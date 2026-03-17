use serde::Deserialize;
use serde_yaml::{Mapping, Value};
use std::fs;
use std::path::Path;

use super::Adapter;

/// Adapter for the [pre-commit](https://pre-commit.com/) hook manager.
///
/// Parses `.pre-commit-config.yaml` to determine which stages have hooks,
/// then delegates to `pre-commit run --hook-stage <stage>` for execution.
pub struct PreCommitAdapter;

impl Adapter for PreCommitAdapter {
    fn name(&self) -> &str {
        "pre-commit"
    }

    fn detect(&self, root: &Path) -> bool {
        root.join(".pre-commit-config.yaml").is_file()
    }

    fn generate_config(&self, root: &Path, hook_name: &str) -> Option<Value> {
        let content = fs::read_to_string(root.join(".pre-commit-config.yaml")).ok()?;
        let config: PreCommitConfig = serde_yaml::from_str(&content).ok()?;

        if !has_hooks_for_stage(&config, hook_name) {
            return None;
        }

        let mut cmd = Mapping::new();
        cmd.insert(
            str_val("run"),
            str_val(&format!("pre-commit run --hook-stage {hook_name}")),
        );

        let cmd_name = format!("pre-commit-{hook_name}");
        let mut commands = Mapping::new();
        commands.insert(str_val(&cmd_name), Value::Mapping(cmd));

        let mut hook_mapping = Mapping::new();
        hook_mapping.insert(str_val("commands"), Value::Mapping(commands));

        let mut root_mapping = Mapping::new();
        root_mapping.insert(str_val(hook_name), Value::Mapping(hook_mapping));

        Some(Value::Mapping(root_mapping))
    }
}

// ---------------------------------------------------------------------------
// .pre-commit-config.yaml schema (subset)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PreCommitConfig {
    #[serde(default)]
    repos: Vec<Repo>,
    #[serde(default)]
    default_stages: Vec<String>,
}

#[derive(Deserialize)]
struct Repo {
    #[serde(default)]
    hooks: Vec<Hook>,
}

#[derive(Deserialize)]
struct Hook {
    #[serde(default)]
    stages: Vec<String>,
}

// ---------------------------------------------------------------------------
// Stage matching
// ---------------------------------------------------------------------------

const DEFAULT_STAGE: &str = "pre-commit";

/// Check whether any hook in the config should run for the given git hook stage.
fn has_hooks_for_stage(config: &PreCommitConfig, hook_name: &str) -> bool {
    config
        .repos
        .iter()
        .flat_map(|r| &r.hooks)
        .any(|hook| hook_matches_stage(hook, &config.default_stages, hook_name))
}

/// Check whether a hook should run for the given git hook stage.
///
/// Per-hook `stages` takes precedence over `default_stages`.
/// When neither is set, defaults to the `pre-commit` stage only.
fn hook_matches_stage(hook: &Hook, default_stages: &[String], hook_name: &str) -> bool {
    if !hook.stages.is_empty() {
        return hook.stages.iter().any(|s| s == hook_name);
    }
    if !default_stages.is_empty() {
        return default_stages.iter().any(|s| s == hook_name);
    }
    hook_name == DEFAULT_STAGE
}

fn str_val(s: &str) -> Value {
    Value::String(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> PreCommitAdapter {
        PreCommitAdapter
    }

    fn write_config(dir: &Path, content: &str) {
        fs::write(dir.join(".pre-commit-config.yaml"), content).unwrap();
    }

    // -- detection --

    #[test]
    fn test_detect_with_config() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "repos: []\n");
        assert!(adapter().detect(dir.path()));
    }

    #[test]
    fn test_detect_without_config() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!adapter().detect(dir.path()));
    }

    #[test]
    fn test_detect_directory_not_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".pre-commit-config.yaml")).unwrap();
        assert!(!adapter().detect(dir.path()));
    }

    // -- stage matching --

    #[test]
    fn test_hook_matches_stage_explicit() {
        let hook = Hook {
            stages: vec!["pre-commit".into()],
        };
        assert!(hook_matches_stage(&hook, &[], "pre-commit"));
        assert!(!hook_matches_stage(&hook, &[], "pre-push"));
    }

    #[test]
    fn test_hook_matches_stage_default_stages() {
        let hook = Hook { stages: vec![] };
        let defaults = vec!["pre-push".to_string()];
        assert!(hook_matches_stage(&hook, &defaults, "pre-push"));
        assert!(!hook_matches_stage(&hook, &defaults, "pre-commit"));
    }

    #[test]
    fn test_hook_matches_stage_no_stages_defaults_to_pre_commit() {
        let hook = Hook { stages: vec![] };
        assert!(hook_matches_stage(&hook, &[], "pre-commit"));
        assert!(!hook_matches_stage(&hook, &[], "pre-push"));
        assert!(!hook_matches_stage(&hook, &[], "commit-msg"));
    }

    #[test]
    fn test_hook_stages_override_default() {
        let hook = Hook {
            stages: vec!["pre-push".into()],
        };
        let defaults = vec!["pre-commit".to_string()];
        assert!(!hook_matches_stage(&hook, &defaults, "pre-commit"));
        assert!(hook_matches_stage(&hook, &defaults, "pre-push"));
    }

    // -- config generation --

    #[test]
    fn test_generate_delegates_to_pre_commit() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"
repos:
  - repo: https://github.com/pre-commit/pre-commit-hooks
    rev: v4.0.0
    hooks:
      - id: trailing-whitespace
"#,
        );

        let config = adapter().generate_config(dir.path(), "pre-commit").unwrap();
        let out = serde_yaml::to_string(&config).unwrap();
        assert!(
            out.contains("pre-commit run --hook-stage pre-commit"),
            "delegates to pre-commit: {out}"
        );
        assert!(out.contains("pre-commit-pre-commit"), "command name: {out}");
    }

    #[test]
    fn test_generate_includes_remote_repos() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"
repos:
  - repo: https://github.com/psf/black
    rev: "22.10.0"
    hooks:
      - id: black
"#,
        );

        assert!(
            adapter().generate_config(dir.path(), "pre-commit").is_some(),
            "remote repos are now supported"
        );
    }

    #[test]
    fn test_generate_respects_stages() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"
repos:
  - repo: local
    hooks:
      - id: fmt
        entry: fmt
        language: system
        stages: [pre-push]
"#,
        );

        assert!(adapter().generate_config(dir.path(), "pre-commit").is_none());
        let config = adapter().generate_config(dir.path(), "pre-push").unwrap();
        let out = serde_yaml::to_string(&config).unwrap();
        assert!(out.contains("pre-commit run --hook-stage pre-push"), "{out}");
        assert!(out.contains("pre-commit-pre-push"), "command name: {out}");
    }

    #[test]
    fn test_generate_respects_default_stages() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"
default_stages: [pre-commit]
repos:
  - repo: local
    hooks:
      - id: fmt
        entry: fmt
        language: system
"#,
        );

        assert!(adapter().generate_config(dir.path(), "pre-commit").is_some());
        assert!(adapter().generate_config(dir.path(), "pre-push").is_none());
    }

    #[test]
    fn test_generate_hook_stages_override_default() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"
default_stages: [pre-commit]
repos:
  - repo: local
    hooks:
      - id: fmt
        entry: fmt
        language: system
        stages: [pre-push]
"#,
        );

        assert!(adapter().generate_config(dir.path(), "pre-commit").is_none());
        assert!(adapter().generate_config(dir.path(), "pre-push").is_some());
    }

    #[test]
    fn test_generate_no_stages_defaults_to_pre_commit() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"
repos:
  - repo: local
    hooks:
      - id: fmt
        entry: fmt
        language: system
"#,
        );

        assert!(adapter().generate_config(dir.path(), "pre-commit").is_some());
        assert!(
            adapter().generate_config(dir.path(), "pre-push").is_none(),
            "should not match pre-push when no stages set"
        );
    }

    #[test]
    fn test_generate_empty_repos() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "repos: []\n");
        assert!(adapter().generate_config(dir.path(), "pre-commit").is_none());
    }

    #[test]
    fn test_generate_multiple_stages() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"
repos:
  - repo: local
    hooks:
      - id: fmt
        entry: fmt
        language: system
        stages: [pre-commit, pre-push]
"#,
        );

        assert!(adapter().generate_config(dir.path(), "pre-commit").is_some());
        assert!(adapter().generate_config(dir.path(), "pre-push").is_some());
        assert!(adapter().generate_config(dir.path(), "commit-msg").is_none());
    }
}
