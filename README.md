# lhm - Merges global and repo lefthook configs

## Install

### Homebrew

```sh
brew install block/tap/lhm
```

### Shell script

```sh
curl -fsSL https://raw.githubusercontent.com/block/lefthookmerge/main/install.sh | sh
```

## Overview

This tool is designed to merge global lefthook config with per repo config. `lhm install` configures global
`core.hooksPath` to call `lhm` which dynamically merges the global and repo lefthook configs, if they exist,
using lefthooks' `extends` [mechanism](https://lefthook.dev/configuration/extends.html).

All standard lefthook config file names are supported: `lefthook.<ext>`, `.lefthook.<ext>` (and `.config/lefthook.<ext>`
for repo configs), where `<ext>` is `yml`, `yaml`, `json`, `jsonc`, or `toml`.

## How it works

### `lhm install`

- Creates shell wrapper scripts for all standard git hooks in `~/.local/libexec/lhm/hooks/`, each invoking `lhm run-hook <hook>`
- Sets `git config --global core.hooksPath ~/.local/libexec/lhm/hooks`
- Writes a default `~/.config/lefthook.yaml` if no user config exists

### `lhm disable`

Unsets `git config --global core.hooksPath`, disabling lhm. The hook scripts in `~/.local/libexec/lhm/hooks/` are left in place so `lhm install` can re-enable quickly.

### `lhm dry-run`

Prints the merged config that would be used for the current repo, then exits. Useful for verifying what hooks will run.

```sh
lhm dry-run
```

### Config overrides

The global and local (repo) config paths can be overridden via CLI flags or environment variables. CLI flags are available on `dry-run`; env vars work everywhere, including during hook invocations.

| Override | CLI flag | Environment variable |
|----------|----------|---------------------|
| Global config | `--global-config <path>` | `LHM_GLOBAL_CONFIG` |
| Local config | `--local-config <path>` | `LHM_LOCAL_CONFIG` |

CLI flags take precedence over env vars. When set, the override path is used directly instead of searching for `lefthook.<ext>` files.

```sh
lhm --global-config ~/custom-global.yaml dry-run
LHM_LOCAL_CONFIG=./other.yml git commit
```

### Hook execution

When git triggers a hook, it runs the wrapper script in the hooks directory. Each script calls `lhm run-hook <hook>`, where the hook name is baked into the script content — making it immune to filename renaming by other tools that inject themselves into `core.hooksPath`.

0. **lefthook not in PATH**: falls back to executing `.git/hooks/<hook>` directly (if it exists), bypassing all config merging
1. **No config at all** (no global, no repo, no adapter): hook is skipped silently
2. **Configs exist**: merges all available layers in order (global, repo/adapter), runs `lefthook run <hook>` with `LEFTHOOK_CONFIG` pointing to the merged temp file

Config is resolved as a two-layer merge, where later layers override earlier ones:

1. **User global** (`~/.config/lefthook.yaml`) — per-user defaults
2. **Repo** (`$REPO/lefthook.yaml` or adapter) — per-repo overrides

Either layer may be absent. When a repo has no lefthook config, the adapter system is used in its place (see below).

When a repo or adapter config is present, the `no_tty` setting is automatically stripped from the user-global config before merging. This prevents a global config from disabling TTY for all repos — each repo should opt into `no_tty` explicitly. When there is no local layer, `no_tty` is kept so it still takes effect for global-only setups.

### Adapters

When a repo has no `lefthook.yaml`, lhm checks for other git hook managers and transparently adapts them. The generated adapter config is merged with `~/.config/lefthook.yaml` using the standard merging system, so global hooks still apply.

Adapters are tried in this order (first match wins):

| Adapter | Detects | Behavior |
|---------|---------|----------|
| **pre-commit** | `.pre-commit-config.yaml` | Parses config to determine which stages have hooks, then delegates to `pre-commit run --hook-stage <stage>`. All hook types (local and remote) are supported. When no `stages` or `default_stages` is set, defaults to the `pre-commit` stage. |
| **husky** | `.husky/` directory | Runs `.husky/<hook>` (if script exists) |
| **hooks-dir** | `.hooks/` or `git-hooks/` directory | Runs `<dir>/<hook>` (if script exists and is executable) and all `<dir>/<hook>-*` prefixed executable scripts as parallel lefthook commands. Non-executable files are silently ignored. Checked in order (first match wins). `.git/hooks/` is intentionally excluded to avoid double-executing hooks already handled by dedicated adapters or lhm itself. |

For the `husky` and `hooks-dir` adapters, git's hook arguments (e.g. `<remote-name> <remote-url>` for `pre-push`, the message file path for `commit-msg`) are forwarded to the script via lefthook's `{0}` template, so scripts receive them positionally just as git would deliver. The `pre-commit` adapter does not forward positional args because `pre-commit run --hook-stage` does not consume them.

Stdin from git is plumbed through to scripts for hooks where git pipes data on stdin: `pre-push` (ref info) and `post-rewrite` (the rewritten-commit list). Each command in those hooks gets `use_stdin: true` in the merged config; lefthook caches stdin and replays it to every command, so this works correctly under `parallel: true`.

### Debugging

Enable debug logging with `--debug` or `LHM_DEBUG=1`:

```sh
lhm --debug install
LHM_DEBUG=1 git commit
```
