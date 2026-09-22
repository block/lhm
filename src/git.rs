use std::path::Path;
use std::process::Command;

/// Repository-local variables exported by Git when it invokes a hook.
///
/// These variables override `-C`, so commands that explicitly target another
/// repository must remove them before selecting that repository.
const LOCAL_REPOSITORY_ENV_VARS: &[&str] = &[
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

/// Build a Git command whose repository is selected only by `root`.
pub fn command_in(root: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(root);
    for variable in LOCAL_REPOSITORY_ENV_VARS {
        command.env_remove(variable);
    }
    command
}
