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
- Marks each wrapper script as immutable (best-effort) so other tools that try to overwrite it fail loudly instead of silently replacing it. Uses `chflags(UF_IMMUTABLE)` on macOS/BSD and `chattr +i` semantics (`FS_IMMUTABLE_FL`) on Linux. macOS works for non-root user installs; Linux requires `CAP_LINUX_IMMUTABLE` (typically root) and is a silent no-op otherwise. Re-running `lhm install` clears and re-applies the flag.
- Sets `git config --global core.hooksPath ~/.local/libexec/lhm/hooks`
- Writes a default `~/.config/lefthook.yaml` if no user config exists

### `lhm uninstall`

Unsets `git config --global core.hooksPath`, uninstalling lhm. The hook scripts in `~/.local/libexec/lhm/hooks/` are left in place so `lhm install` can re-enable quickly.

### `lhm disable` / `lhm enable`

`lhm disable` suppresses **repo-specific** hooks (the repo's own `lefthook.yaml` and any repo-fallback adapter like pre-commit/husky/hooks-dir) for the current repo. The user-global config and underlay adapters (e.g. `git-lfs`) still run. `lhm enable` reverses it.

The disabled set is keyed by the repo's `origin` remote URL and persisted in `~/.config/lhm.yaml`:

```yaml
disabled_repos:
  - git@github.com:foo/bar.git
  - https://github.com/baz/qux
```

Origin URLs are used verbatim — no normalization between `git@` and `https://` forms. If a repo has no `origin` remote, `lhm disable` errors out.

### `lhm dry-run`

Prints the merged config that would be used for the current repo, then exits. Useful for verifying what hooks will run.

```sh
lhm dry-run
```

### `lhm import`

Writes the repo-fallback adapter's generated config to `.lefthook.yaml` in the current repo, so you can switch from `pre-commit`/`husky`/`hooks-dir` to a native lefthook config and edit it directly. Only the adapter output is written — the user-global config and underlay adapters are not included.

Errors if a lefthook config already exists in the repo (any of `lefthook.<ext>`, `.lefthook.<ext>`, `.config/lefthook.<ext>`) or if no repo-fallback adapter is detected.

```sh
lhm import
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
1. **No config at all** (no underlay adapter, no global, no repo, no repo-fallback adapter): hook is skipped silently
2. **Configs exist**: merges all available layers in order (underlay, global, repo/adapter), runs `lefthook run <hook>` with `LEFTHOOK_CONFIG` pointing to the merged temp file

Config is resolved as a three-layer merge, where later layers override earlier ones:

1. **Underlay adapters** — always-on baselines for tools that need their hooks to run regardless of the repo's own configuration (e.g. `git-lfs`). See *Adapters* below.
2. **User global** (`~/.config/lefthook.yaml`) — per-user defaults
3. **Repo** (`$REPO/lefthook.yaml` or repo-fallback adapter) — per-repo overrides

Any layer may be absent. When a repo has no lefthook config, the repo-fallback adapter system is used in its place (see below).

When a repo or adapter config is present, the `no_tty` setting is automatically stripped from the user-global config before merging. This prevents a global config from disabling TTY for all repos — each repo should opt into `no_tty` explicitly. When there is no local layer, `no_tty` is kept so it still takes effect for global-only setups.

### Adapters

lhm has two categories of adapters:

- **Repo-fallback adapters** stand in for a missing `lefthook.yaml`. Only the first detected one is used.
- **Underlay adapters** detect always-on tools and merge into a low-priority layer beneath the user-global config, so the user or repo can still override anything they generate.

#### Repo-fallback adapters

When a repo has no `lefthook.yaml`, lhm checks for other git hook managers and transparently adapts them. The generated adapter config is merged with `~/.config/lefthook.yaml` using the standard merging system, so global hooks still apply.

Tried in this order (first match wins):

| Adapter | Detects | Behavior |
|---------|---------|----------|
| **pre-commit** | `.pre-commit-config.yaml` **and** `pre-commit` in `PATH` | Parses config to determine which stages have hooks, then delegates to `pre-commit run --hook-stage <stage>`. All hook types (local and remote) are supported. When no `stages` or `default_stages` is set, defaults to the `pre-commit` stage. If `pre-commit` isn't installed, the adapter declines and lhm falls through to the next adapter. |
| **husky** | `.husky/` directory | Runs `.husky/<hook>` (if script exists) |
| **hooks-dir** | `.hooks/` or `git-hooks/` directory | Runs `<dir>/<hook>` (if script exists and is executable) and all `<dir>/<hook>-*` prefixed executable scripts as parallel lefthook commands. Non-executable files are silently ignored. Checked in order (first match wins). `.git/hooks/` is intentionally excluded to avoid double-executing hooks already handled by dedicated adapters or lhm itself. |

For the `husky` and `hooks-dir` adapters, git's hook arguments (e.g. `<remote-name> <remote-url>` for `pre-push`, the message file path for `commit-msg`) are forwarded to the script via lefthook's `{0}` template, so scripts receive them positionally just as git would deliver. The `pre-commit` adapter does not forward positional args because `pre-commit run --hook-stage` does not consume them.

#### Underlay adapters

| Adapter | Detects | Behavior |
|---------|---------|----------|
| **git-lfs** | `git-lfs` in PATH **and** the repo uses LFS (root `.gitattributes` declares `filter=lfs`, or the repo's git config has any `lfs.*` entry) | Injects `git lfs <hook> "$@"` commands for `pre-push`, `post-checkout`, `post-commit`, and `post-merge`. Detection is per-repo: non-LFS repos pay no cost. The user or repo can override or skip these by defining a command named `git-lfs` in their own `lefthook.yaml`. |

Lefthook has its own built-in LFS support that fires for those four hooks whenever `skip_lfs` isn't set, but it runs on every repo regardless of whether the repo actually uses LFS, which is noticeably slow. Our default global config sets `skip_lfs: true` to opt out, and the `git-lfs` underlay adapter then re-introduces the LFS commands only in repos that actually use LFS.

If you have a repo-local hooks-dir or husky script (e.g. `.hooks/post-merge`, `.husky/pre-push`) that already shells out to `git lfs <hook>`, the underlay will run LFS a second time alongside your script. Either remove the `git lfs <hook>` line from your script (lhm now handles it) or override the underlay in your repo's `lefthook.yaml`:

```yaml
post-merge:
  commands:
    git-lfs:
      skip: true
```

Because lhm now owns `core.hooksPath`, **don't** run plain `git lfs install` (it would try to write hook scripts into lhm's hooks dir, which is marked immutable, and fail with `Operation not permitted`). Run `git lfs install --skip-repo` instead to set up just the global LFS smudge/clean filters — lhm handles the hooks. `lhm install` prints a hint about this when it detects `git-lfs` in PATH without the filters configured.

Stdin from git is plumbed through to scripts for hooks where git pipes data on stdin: `pre-push` (ref info) and `post-rewrite` (the rewritten-commit list). Each command in those hooks gets `use_stdin: true` in the merged config; lefthook caches stdin and replays it to every command, so this works correctly under `parallel: true`.

### Environment

lhm sets `LHM=1` in the environment when it invokes lefthook (or a `.git/hooks/<hook>` fallback). Scripts can branch on it to detect whether they're running under lhm:

```sh
if [ "${LHM:-}" = "1" ]; then
  # lhm is handling git-lfs hooks; skip our manual `git lfs <hook>` call
fi
```

### Debugging

Enable debug logging with `--debug` or `LHM_DEBUG=1`:

```sh
lhm --debug install
LHM_DEBUG=1 git commit
```
