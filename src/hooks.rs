use log::{debug, warn};
use serde_yaml::Value;
use std::fs;
use std::path::Path;

use crate::immutable::{clear_immutable, set_immutable};

pub const GIT_HOOKS: &[&str] = &[
    "applypatch-msg",
    "commit-msg",
    "post-applypatch",
    "post-checkout",
    "post-commit",
    "post-merge",
    "post-rewrite",
    "pre-applypatch",
    "pre-commit",
    "pre-merge-commit",
    "pre-push",
    "pre-rebase",
    "prepare-commit-msg",
];

pub fn is_hook_name(name: &str) -> bool {
    GIT_HOOKS.contains(&name)
}

/// Hooks where commands mutate shared state and must not run in parallel.
/// - `pre-commit` / `pre-merge-commit`: formatters mutate the working tree/index
/// - `prepare-commit-msg` / `commit-msg` / `applypatch-msg`: edit a single message file
const SERIAL_HOOKS: &[&str] = &[
    "applypatch-msg",
    "commit-msg",
    "pre-commit",
    "pre-merge-commit",
    "prepare-commit-msg",
];

/// Client-side hooks where git pipes data on stdin: pre-push gets ref info,
/// post-rewrite gets the rewritten-commit list. Each command in these hooks
/// gets `use_stdin: true` so lefthook plumbs git's stdin through to the
/// underlying script. lefthook caches stdin and replays it to every command
/// that opts in, so this works correctly under `parallel: true`.
const STDIN_HOOKS: &[&str] = &["pre-push", "post-rewrite"];

/// Generate the content of a shell wrapper script that invokes `lhm run-hook`.
/// The hook name is baked into the script, so renaming the file won't break dispatch.
///
/// `LHM_GLOBAL_CONFIG` / `LHM_LOCAL_CONFIG` are unset before exec so they cannot
/// leak from the developer's shell into the hook invocation. They are intended
/// as a `dry-run` convenience; honoring them during normal hook execution lets
/// a hostile repo's `.envrc` (via direnv or similar) redirect lhm's config
/// sources to attacker-controlled YAML that lefthook then runs.
fn hook_script(binary: &Path, hook_name: &str) -> String {
    format!(
        "#!/bin/sh\nunset LHM_GLOBAL_CONFIG LHM_LOCAL_CONFIG\nexec \"{}\" run-hook {} \"$@\"\n",
        binary.display(),
        hook_name,
    )
}

/// Write shell wrapper scripts for every standard git hook into `dir`.
/// Each script invokes `lhm run-hook <hook_name>`, making hook identity
/// independent of the filename — immune to renaming by other tools.
pub fn create_hook_scripts(dir: &Path, binary: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;

    remove_stale_hooks(dir);

    let mut immutable_failures = 0usize;
    let mut last_immutable_err: Option<String> = None;

    for hook in GIT_HOOKS {
        let path = dir.join(hook);
        if path.exists() {
            // Best-effort: a previous lhm install may have set the immutable
            // flag. Ignore errors here so a future-incompatible flag setting
            // doesn't block reinstall; the subsequent remove/write will surface
            // a real failure if the file truly can't be replaced.
            let _ = clear_immutable(&path);
        }
        let _ = fs::remove_file(&path);
        let content = hook_script(binary, hook);
        fs::write(&path, &content).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
        set_executable(&path)?;
        if let Err(e) = set_immutable(&path) {
            immutable_failures += 1;
            last_immutable_err = Some(e);
        }
    }

    if immutable_failures > 0 {
        let err = last_immutable_err.unwrap_or_default();
        warn!(
            "could not mark {immutable_failures} hook script(s) immutable; other tools may silently overwrite them ({err})"
        );
    }

    Ok(())
}

/// Mark a file as executable (owner rwx, group/other rx).
#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("failed to chmod {}: {e}", path.display()))
}

/// Remove any entries in the hooks dir that aren't in the current `GIT_HOOKS` list.
fn remove_stale_hooks(dir: &Path) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !GIT_HOOKS.contains(&name_str) {
            debug!("removing stale hook: {name_str}");
            let path = entry.path();
            let _ = clear_immutable(&path);
            let _ = fs::remove_file(path);
        }
    }
}

/// Annotate adapter-generated config with lefthook settings:
/// - `parallel: true` on hooks that don't mutate shared state
/// - `stage_fixed: true` on each command within `pre-commit` and `pre-merge-commit` hooks
/// - `use_stdin: true` on each command within hooks where git pipes data on stdin
pub fn annotate_hooks(config: Value) -> Value {
    let Value::Mapping(mut root) = config else {
        return config;
    };
    for (key, val) in &mut root {
        if let (Some(name), Value::Mapping(hook_map)) = (key.as_str(), val)
            && is_hook_name(name)
        {
            if !SERIAL_HOOKS.contains(&name) {
                hook_map.insert(Value::String("parallel".to_string()), Value::Bool(true));
            }
            if name == "pre-commit" || name == "pre-merge-commit" {
                set_command_flag(hook_map, "stage_fixed");
            }
            if STDIN_HOOKS.contains(&name) {
                set_command_flag(hook_map, "use_stdin");
            }
        }
    }
    Value::Mapping(root)
}

/// Set `<flag>: true` on every command in a hook mapping.
fn set_command_flag(hook_map: &mut serde_yaml::Mapping, flag: &str) {
    let commands_key = Value::String("commands".to_string());
    if let Some(Value::Mapping(commands)) = hook_map.get_mut(&commands_key) {
        for (_cmd_name, cmd_val) in commands.iter_mut() {
            if let Value::Mapping(cmd_map) = cmd_val {
                cmd_map.insert(Value::String(flag.to_string()), Value::Bool(true));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn yaml(s: &str) -> Value {
        serde_yaml::from_str(s).unwrap()
    }

    fn to_yaml(v: &Value) -> String {
        serde_yaml::to_string(v).unwrap()
    }

    #[test]
    fn test_is_hook_name() {
        assert!(is_hook_name("pre-commit"));
        assert!(is_hook_name("commit-msg"));
        assert!(is_hook_name("pre-push"));
        assert!(is_hook_name("post-merge"));
        assert!(!is_hook_name("lhm"));
        assert!(!is_hook_name("cargo"));
        assert!(!is_hook_name(""));
    }

    #[test]
    fn test_hook_script_content() {
        let content = hook_script(Path::new("/usr/local/bin/lhm"), "pre-commit");
        assert_eq!(
            content,
            "#!/bin/sh\nunset LHM_GLOBAL_CONFIG LHM_LOCAL_CONFIG\nexec \"/usr/local/bin/lhm\" run-hook pre-commit \"$@\"\n"
        );
    }

    #[test]
    fn test_hook_script_embeds_hook_name_not_filename() {
        let content = hook_script(Path::new("/bin/lhm"), "prepare-commit-msg");
        assert!(content.contains("run-hook prepare-commit-msg"));
    }

    #[test]
    fn test_hook_script_unsets_lhm_config_env_vars() {
        // Closes the direnv attack: a hostile repo's .envrc cannot redirect
        // the config lhm consumes during hook execution by exporting
        // LHM_GLOBAL_CONFIG / LHM_LOCAL_CONFIG.
        let content = hook_script(Path::new("/bin/lhm"), "pre-commit");
        let unset_line = "unset LHM_GLOBAL_CONFIG LHM_LOCAL_CONFIG";
        let exec_line = "exec \"/bin/lhm\"";
        let unset_idx = content.find(unset_line).expect("unset line present");
        let exec_idx = content.find(exec_line).expect("exec line present");
        assert!(
            unset_idx < exec_idx,
            "unset must precede exec; content was: {content}"
        );
    }

    /// Clear immutable flag from every hook in `dir` so the tempdir can be
    /// cleaned up on drop. No-op on platforms where immutability isn't set.
    fn clear_immutable_hooks(dir: &Path) {
        for hook in GIT_HOOKS {
            let _ = crate::immutable::clear_immutable(&dir.join(hook));
        }
    }

    #[test]
    fn test_create_hook_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join("hooks");
        let fake_binary = Path::new("/usr/local/bin/lhm");

        create_hook_scripts(&hooks, fake_binary).unwrap();

        for hook in GIT_HOOKS {
            let path = hooks.join(hook);
            assert!(path.is_file(), "{hook} should exist");

            let content = fs::read_to_string(&path).unwrap();
            assert!(content.starts_with("#!/bin/sh\n"), "{hook} has shebang");
            assert!(
                content.contains(&format!("run-hook {hook}")),
                "{hook} script invokes run-hook with correct name: {content}"
            );

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&path).unwrap().permissions().mode();
                assert_eq!(mode & 0o755, 0o755, "{hook} is executable");
            }
        }

        clear_immutable_hooks(&hooks);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_create_hook_scripts_marks_files_immutable_on_macos() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join("hooks");
        let fake_binary = Path::new("/usr/local/bin/lhm");

        create_hook_scripts(&hooks, fake_binary).unwrap();

        let path = hooks.join("pre-commit");
        assert!(
            fs::write(&path, "clobber").is_err(),
            "write to immutable hook should fail"
        );
        assert!(
            fs::remove_file(&path).is_err(),
            "unlinking an immutable hook should fail"
        );

        // Re-running install must succeed: it should clear immutability,
        // rewrite, and re-set it.
        create_hook_scripts(&hooks, fake_binary).unwrap();
        assert!(fs::write(&path, "clobber").is_err(), "still immutable after reinstall");

        clear_immutable_hooks(&hooks);
    }

    #[test]
    fn test_create_hook_scripts_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join("hooks");
        fs::create_dir_all(&hooks).unwrap();

        fs::write(hooks.join("pre-commit"), "old content").unwrap();

        let fake_binary = Path::new("/usr/local/bin/lhm");
        create_hook_scripts(&hooks, fake_binary).unwrap();

        let content = fs::read_to_string(hooks.join("pre-commit")).unwrap();
        assert!(content.contains("run-hook pre-commit"), "overwritten: {content}");

        clear_immutable_hooks(&hooks);
    }

    #[cfg(unix)]
    #[test]
    fn test_create_hook_scripts_replaces_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join("hooks");
        fs::create_dir_all(&hooks).unwrap();

        let target = dir.path().join("fake-binary");
        fs::write(&target, "original binary content").unwrap();
        symlink(&target, hooks.join("pre-commit")).unwrap();

        let fake_binary = Path::new("/usr/local/bin/lhm");
        create_hook_scripts(&hooks, fake_binary).unwrap();

        let meta = hooks.join("pre-commit").symlink_metadata().unwrap();
        assert!(meta.file_type().is_file(), "symlink replaced with regular file");
        assert!(!meta.file_type().is_symlink(), "no longer a symlink");

        let content = fs::read_to_string(hooks.join("pre-commit")).unwrap();
        assert!(
            content.contains("run-hook pre-commit"),
            "has wrapper content: {content}"
        );

        let target_content = fs::read_to_string(&target).unwrap();
        assert_eq!(
            target_content, "original binary content",
            "symlink target not clobbered"
        );

        clear_immutable_hooks(&hooks);
    }

    #[test]
    fn test_create_hook_scripts_removes_stale_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join("hooks");
        fs::create_dir_all(&hooks).unwrap();

        let stale = ["reference-transaction", "fsmonitor-watchman", "update"];
        for name in &stale {
            fs::write(hooks.join(name), "old").unwrap();
        }

        let fake_binary = Path::new("/usr/local/bin/lhm");
        create_hook_scripts(&hooks, fake_binary).unwrap();

        for name in &stale {
            assert!(!hooks.join(name).exists(), "stale hook {name} should be removed");
        }
        for hook in GIT_HOOKS {
            assert!(hooks.join(hook).is_file());
        }

        clear_immutable_hooks(&hooks);
    }

    #[test]
    fn test_annotate_hooks_parallel_on_safe_hooks() {
        let config = yaml("pre-push:\n  commands:\n    foo:\n      run: echo hi\noutput:\n  - success\n");
        let result = annotate_hooks(config);
        let out = to_yaml(&result);
        assert!(out.contains("parallel: true"), "injects parallel: {out}");
        assert!(out.contains("output:"), "non-hook keys preserved: {out}");
    }

    #[test]
    fn test_annotate_hooks_no_parallel_on_serial_hooks() {
        for hook in SERIAL_HOOKS {
            let config = yaml(&format!("{hook}:\n  commands:\n    foo:\n      run: echo hi\n"));
            let result = annotate_hooks(config);
            let out = to_yaml(&result);
            assert!(!out.contains("parallel"), "no parallel on {hook}: {out}");
        }
    }

    #[test]
    fn test_annotate_hooks_stage_fixed_on_pre_commit_hooks() {
        for hook in &["pre-commit", "pre-merge-commit"] {
            let config = yaml(&format!(
                "{hook}:\n  commands:\n    foo:\n      run: echo hi\n    bar:\n      run: echo bye\n"
            ));
            let result = annotate_hooks(config);
            let out = to_yaml(&result);
            assert!(
                out.contains("stage_fixed: true"),
                "injects stage_fixed on {hook}: {out}"
            );
            assert!(!out.contains("parallel"), "no parallel on {hook}: {out}");
            assert_eq!(
                out.matches("stage_fixed").count(),
                2,
                "both commands get stage_fixed on {hook}: {out}"
            );
        }
    }

    #[test]
    fn test_annotate_hooks_no_stage_fixed_on_pre_push() {
        let config = yaml("pre-push:\n  commands:\n    foo:\n      run: echo hi\n");
        let result = annotate_hooks(config);
        let out = to_yaml(&result);
        assert!(!out.contains("stage_fixed"), "no stage_fixed on pre-push: {out}");
    }

    #[test]
    fn test_annotate_hooks_use_stdin_on_stdin_hooks() {
        for hook in STDIN_HOOKS {
            let config = yaml(&format!(
                "{hook}:\n  commands:\n    foo:\n      run: echo hi\n    bar:\n      run: echo bye\n"
            ));
            let result = annotate_hooks(config);
            let out = to_yaml(&result);
            assert_eq!(
                out.matches("use_stdin: true").count(),
                2,
                "both commands get use_stdin on {hook}: {out}"
            );
        }
    }

    #[test]
    fn test_annotate_hooks_no_use_stdin_on_other_hooks() {
        for hook in &["pre-commit", "commit-msg", "post-commit", "post-checkout"] {
            let config = yaml(&format!("{hook}:\n  commands:\n    foo:\n      run: echo hi\n"));
            let result = annotate_hooks(config);
            let out = to_yaml(&result);
            assert!(!out.contains("use_stdin"), "no use_stdin on {hook}: {out}");
        }
    }

    #[test]
    fn test_annotate_hooks_skips_non_hook_keys() {
        let config = yaml("output:\n  - success\n");
        let result = annotate_hooks(config);
        let out = to_yaml(&result);
        assert!(!out.contains("parallel"), "no parallel on non-hook: {out}");
    }
}
