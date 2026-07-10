use log::debug;
use serde_yaml::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

/// Config path overrides, sourced from CLI flags (with env-var fallback handled
/// by clap). `user_config`/`local_config` point at a single config file each;
/// `system_config_dirs` is the ordered list of directories searched for
/// system-wide configs (defaults to `/etc` then `/usr/local/etc`).
#[derive(Debug, Clone, Default)]
pub struct ConfigOverrides {
    pub user_config: Option<PathBuf>,
    pub local_config: Option<PathBuf>,
    pub system_config_dirs: Vec<PathBuf>,
}

impl ConfigOverrides {
    /// Build overrides from already-resolved CLI/env values.
    pub fn new(user_flag: Option<PathBuf>, local_flag: Option<PathBuf>, system_dirs: Vec<PathBuf>) -> Self {
        Self {
            user_config: user_flag,
            local_config: local_flag,
            system_config_dirs: system_dirs,
        }
    }
}

pub const LEFTHOOK_EXTENSIONS: &[&str] = &["yml", "yaml", "json", "jsonc", "toml"];

/// Search for a lefthook config file in the given directory.
/// Checks `lefthook.<ext>`, `.lefthook.<ext>`, and optionally `.config/lefthook.<ext>`.
pub fn find_config(dir: &Path, check_dot_config: bool) -> Option<PathBuf> {
    for ext in LEFTHOOK_EXTENSIONS {
        let candidates = if check_dot_config {
            vec![
                dir.join(format!("lefthook.{ext}")),
                dir.join(format!(".lefthook.{ext}")),
                dir.join(format!(".config/lefthook.{ext}")),
            ]
        } else {
            vec![
                dir.join(format!("lefthook.{ext}")),
                dir.join(format!(".lefthook.{ext}")),
            ]
        };
        for candidate in candidates {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Find the user-level lefthook config in the given directory (e.g. `~/.config`).
pub fn user_config(home: &Path, overrides: &ConfigOverrides) -> Option<PathBuf> {
    if let Some(ref p) = overrides.user_config {
        debug!("using user config override: {}", p.display());
        return Some(p.clone());
    }
    find_config(home, false)
}

pub fn repo_config(root: &Path, overrides: &ConfigOverrides) -> Option<PathBuf> {
    if let Some(ref p) = overrides.local_config {
        debug!("using local config override: {}", p.display());
        return Some(p.clone());
    }
    find_config(root, true)
}

/// Load the user-level config from `~/.config/lefthook.yaml` (or override) if it exists.
pub fn load_user_config(dir: &Path, overrides: &ConfigOverrides) -> Result<Option<Value>, String> {
    match user_config(dir, overrides) {
        Some(path) => read_yaml(&path).map(Some),
        None => {
            debug!("no user config file found");
            Ok(None)
        }
    }
}

/// Load lefthook configs from each of `dirs`, in order. Each directory is
/// searched for a standard config file name (via `find_config`, without the
/// repo-only `.config/` variant); directories with no config are skipped.
///
/// The returned values keep `dirs` order — lowest priority first — so when the
/// caller merges them, a later directory overrides an earlier one.
fn load_configs_from_dirs(dirs: &[PathBuf]) -> Result<Vec<Value>, String> {
    let mut configs = Vec::with_capacity(dirs.len());
    for dir in dirs {
        if let Some(path) = find_config(dir, false) {
            debug!("system config: {}", path.display());
            configs.push(read_yaml(&path)?);
        }
    }
    Ok(configs)
}

/// Load the system-wide configs named by `overrides.system_config_dirs`
/// (defaults to `/etc` then `/usr/local/etc`), lowest priority first.
pub fn load_system_configs(overrides: &ConfigOverrides) -> Result<Vec<Value>, String> {
    load_configs_from_dirs(&overrides.system_config_dirs)
}

/// Maximum size of a config file lhm will parse. Defends against straight-up
/// huge inputs (a hostile repo can ship arbitrarily large YAML); does NOT
/// defeat alias-bomb expansion, which can blow up a sub-kilobyte source into
/// gigabytes of parsed structure — the underlying parser (unsafe-libyaml)
/// does not expose alias/depth limits today.
pub const MAX_CONFIG_SIZE: usize = 1024 * 1024;

pub fn read_yaml(path: &Path) -> Result<Value, String> {
    let content = fs::read_to_string(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    if content.len() > MAX_CONFIG_SIZE {
        return Err(format!(
            "config file {} exceeds {}-byte size limit ({} bytes)",
            path.display(),
            MAX_CONFIG_SIZE,
            content.len()
        ));
    }
    serde_yaml::from_str(&content).map_err(|e| format!("failed to parse {}: {e}", path.display()))
}

/// Serialize a merged config value to a temp file for lefthook.
pub fn write_merged_temp(merged: Value) -> Result<NamedTempFile, String> {
    let content = serde_yaml::to_string(&merged).map_err(|e| format!("failed to serialize config: {e}"))?;
    debug!("merged config:\n{content}");

    let mut tmp = tempfile::Builder::new()
        .suffix(".yml")
        .tempfile()
        .map_err(|e| format!("failed to create temp file: {e}"))?;
    write!(tmp, "{content}").map_err(|e| format!("failed to write temp config: {e}"))?;
    Ok(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn no_overrides() -> ConfigOverrides {
        ConfigOverrides::default()
    }

    fn to_yaml(v: &Value) -> String {
        serde_yaml::to_string(v).unwrap()
    }

    #[test]
    fn test_find_config_yaml() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lefthook.yaml"), "").unwrap();
        assert_eq!(find_config(dir.path(), false), Some(dir.path().join("lefthook.yaml")));
    }

    #[test]
    fn test_find_config_yml() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lefthook.yml"), "").unwrap();
        assert_eq!(find_config(dir.path(), false), Some(dir.path().join("lefthook.yml")));
    }

    #[test]
    fn test_find_config_toml() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lefthook.toml"), "").unwrap();
        assert_eq!(find_config(dir.path(), false), Some(dir.path().join("lefthook.toml")));
    }

    #[test]
    fn test_find_config_dotted() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".lefthook.json"), "").unwrap();
        assert_eq!(find_config(dir.path(), false), Some(dir.path().join(".lefthook.json")));
    }

    #[test]
    fn test_find_config_dot_config_subdir() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".config")).unwrap();
        fs::write(dir.path().join(".config/lefthook.toml"), "").unwrap();
        assert_eq!(
            find_config(dir.path(), true),
            Some(dir.path().join(".config/lefthook.toml"))
        );
        // Should not find .config/ variant when check_dot_config is false
        assert_eq!(find_config(dir.path(), false), None);
    }

    #[test]
    fn test_find_config_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(find_config(dir.path(), true), None);
    }

    #[test]
    fn test_find_config_prefers_yml_over_yaml() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lefthook.yml"), "").unwrap();
        fs::write(dir.path().join("lefthook.yaml"), "").unwrap();
        // yml comes first in LEFTHOOK_EXTENSIONS
        assert_eq!(find_config(dir.path(), false), Some(dir.path().join("lefthook.yml")));
    }

    #[test]
    fn test_load_user_config_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let result = load_user_config(dir.path(), &no_overrides()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_load_user_config_returns_some_when_exists() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".lefthook.yaml"),
            "pre-commit:\n  commands:\n    fmt:\n      run: echo hi\n",
        )
        .unwrap();
        let result = load_user_config(dir.path(), &no_overrides()).unwrap();
        assert!(result.is_some());
        let out = to_yaml(&result.unwrap());
        assert!(out.contains("pre-commit:"));
    }

    #[test]
    fn test_user_config_override() {
        let dir = tempfile::tempdir().unwrap();
        let override_path = dir.path().join("custom-global.yaml");
        fs::write(&override_path, "pre-push:\n  commands:\n    t:\n      run: echo t\n").unwrap();

        let overrides = ConfigOverrides {
            user_config: Some(override_path.clone()),
            ..Default::default()
        };
        let result = user_config(dir.path(), &overrides);
        assert_eq!(result, Some(override_path));
    }

    #[test]
    fn test_repo_config_override() {
        let dir = tempfile::tempdir().unwrap();
        let override_path = dir.path().join("custom-local.yaml");
        fs::write(&override_path, "pre-commit:\n  commands:\n    f:\n      run: echo f\n").unwrap();

        let overrides = ConfigOverrides {
            local_config: Some(override_path.clone()),
            ..Default::default()
        };
        let result = repo_config(dir.path(), &overrides);
        assert_eq!(result, Some(override_path));
    }

    #[test]
    fn test_cli_flags_set_overrides() {
        let o = ConfigOverrides::new(
            Some(PathBuf::from("/tmp/cli-user.yaml")),
            Some(PathBuf::from("/tmp/cli-local.yaml")),
            vec![PathBuf::from("/etc"), PathBuf::from("/usr/local/etc")],
        );
        assert_eq!(o.user_config, Some(PathBuf::from("/tmp/cli-user.yaml")));
        assert_eq!(o.local_config, Some(PathBuf::from("/tmp/cli-local.yaml")));
        assert_eq!(
            o.system_config_dirs,
            vec![PathBuf::from("/etc"), PathBuf::from("/usr/local/etc")]
        );
    }

    #[test]
    fn test_load_configs_from_dirs_in_order() {
        let etc = tempfile::tempdir().unwrap();
        let usr_local = tempfile::tempdir().unwrap();
        fs::write(etc.path().join("lefthook.yaml"), "no_tty: true\n").unwrap();
        fs::write(usr_local.path().join("lefthook.yml"), "skip_lfs: true\n").unwrap();

        let dirs = vec![etc.path().to_path_buf(), usr_local.path().to_path_buf()];
        let configs = load_configs_from_dirs(&dirs).unwrap();

        assert_eq!(configs.len(), 2);
        // Values keep `dirs` order (lowest priority first).
        assert!(to_yaml(&configs[0]).contains("no_tty"), "first entry is /etc");
        assert!(
            to_yaml(&configs[1]).contains("skip_lfs"),
            "second entry is /usr/local/etc"
        );
    }

    #[test]
    fn test_load_configs_from_dirs_skips_missing() {
        let empty = tempfile::tempdir().unwrap();
        let present = tempfile::tempdir().unwrap();
        fs::write(present.path().join("lefthook.yaml"), "skip_lfs: true\n").unwrap();

        // A directory with no config is skipped rather than erroring.
        let dirs = vec![empty.path().to_path_buf(), present.path().to_path_buf()];
        let configs = load_configs_from_dirs(&dirs).unwrap();

        assert_eq!(configs.len(), 1);
        assert!(to_yaml(&configs[0]).contains("skip_lfs"));
    }

    #[test]
    fn test_load_configs_from_dirs_empty() {
        assert!(load_configs_from_dirs(&[]).unwrap().is_empty());
    }

    #[test]
    fn test_load_configs_from_dirs_propagates_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lefthook.yaml"), "a: b: c\n").unwrap();
        let err = load_configs_from_dirs(&[dir.path().to_path_buf()]).unwrap_err();
        assert!(err.contains("lefthook.yaml"), "error names the file: {err}");
    }

    #[test]
    fn test_load_system_configs_uses_override_dirs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lefthook.yaml"), "skip_lfs: true\n").unwrap();
        let overrides = ConfigOverrides {
            system_config_dirs: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        let configs = load_system_configs(&overrides).unwrap();
        assert_eq!(configs.len(), 1);
        assert!(to_yaml(&configs[0]).contains("skip_lfs"));
    }

    #[test]
    fn test_read_yaml_rejects_oversized_input() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.yaml");
        // One byte over the limit is enough to trigger the check; build the
        // file as valid YAML padded with a long comment so a regression that
        // moved the check after the parse would still surface.
        let mut content = String::from("a: 1\n# ");
        content.push_str(&"x".repeat(MAX_CONFIG_SIZE));
        fs::write(&path, &content).unwrap();

        let err = read_yaml(&path).unwrap_err();
        assert!(err.contains("exceeds"), "error mentions size limit: {err}");
        assert!(err.contains("byte"), "error mentions bytes: {err}");
    }

    #[test]
    fn test_read_yaml_accepts_input_at_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.yaml");
        // Exactly at the limit should still parse.
        let mut content = String::from("a: 1\n");
        content.push_str(&"# ".repeat((MAX_CONFIG_SIZE - content.len()) / 2));
        assert!(content.len() <= MAX_CONFIG_SIZE);
        fs::write(&path, &content).unwrap();

        assert!(read_yaml(&path).is_ok());
    }
}
