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

- Creates symlinks for all standard git hooks in `~/.local/libexec/lhm/hooks/`, each pointing to the `lhm` binary
- Sets `git config --global core.hooksPath ~/.local/libexec/lhm/hooks`
- Writes a default `~/.local/etc/lefthook.yaml` if no user config exists

### `lhm install --system`

Same as `lhm install` but targets a system-wide location (requires root):

- Creates symlinks in `/usr/local/libexec/lhm/hooks/`
- Sets `git config --system core.hooksPath /usr/local/libexec/lhm/hooks`
- Writes a default `/usr/local/etc/lefthook.yaml` if no system config exists

### `lhm disable`

Unsets `git config --global core.hooksPath`, disabling lhm. The hook symlinks in `~/.local/libexec/lhm/hooks/` are left in place so `lhm install` can re-enable quickly.

### `lhm disable --system`

Unsets `git config --system core.hooksPath` (requires root). The hook symlinks in `/usr/local/libexec/lhm/hooks/` are left in place.

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

When git triggers a hook, it invokes the symlink in the hooks directory. `lhm` detects the hook name from `argv[0]` and:

0. **lefthook not in PATH**: falls back to executing `.git/hooks/<hook>` directly (if it exists), bypassing all config merging
1. **No config at all** (no system, no global, no repo, no adapter): hook is skipped silently
2. **Configs exist**: merges all available layers in order (system, global, repo/adapter), runs `lefthook run <hook>` with `LEFTHOOK_CONFIG` pointing to the merged temp file

Config is resolved as a three-layer merge, where later layers override earlier ones:

1. **System** (`/usr/local/etc/lefthook.yaml`) — organizational baseline
2. **User global** (`~/.local/etc/lefthook.yaml`) — per-user overrides
3. **Repo** (`$REPO/lefthook.yaml` or adapter) — per-repo overrides

Any layer may be absent. When a repo has no lefthook config, the adapter system is used in its place (see below).

### Adapters

When a repo has no `lefthook.yaml`, lhm checks for other git hook managers and transparently adapts them. The generated adapter config is merged with `~/.local/etc/lefthook.yaml` using the standard merging system, so global hooks still apply.

Adapters are tried in this order (first match wins):

| Adapter | Detects | Behavior |
|---------|---------|----------|
| **pre-commit** | `.pre-commit-config.yaml` | Translates `repo: local` hooks into lefthook commands (`entry` + `args` → `run`, `types`/`types_or` → `glob`, `files`/`exclude` preserved). Remote repos are skipped. |
| **husky** | `.husky/` directory | Runs `.husky/<hook>` (if script exists) |
| **hooks-dir** | `.hooks/` or `git-hooks/` directory | Runs `<dir>/<hook>` (if script exists) and all `<dir>/<hook>-*` prefixed scripts as parallel lefthook commands. Checked in order (first match wins). `.git/hooks/` is intentionally excluded to avoid double-executing hooks already handled by dedicated adapters or lhm itself. |

### Debugging

Enable debug logging with `--debug` or `LHM_DEBUG=1`:

```sh
lhm --debug install
LHM_DEBUG=1 git commit
```
