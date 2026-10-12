# Mentat

## Install

From the root of this checkout, install the binary with Cargo:

```sh
cargo install --path app --locked
```

This installs `mentat` into `$CARGO_HOME/bin`, normally `~/.cargo/bin`. Make sure that directory is on your `PATH`, then run it from your repository:

```sh
mentat
mentat --cwd /path/to/repo
```


## Workspaces

Run Mentat from the repository you want to work on, or select it with `--cwd`.
Every ticket opened in that instance uses that repository; Linear tickets do not
select a repository automatically.

New jj workspaces and Git worktrees live in
`~/code/mentat-workspaces/<repo>-<id>/<ticket>/`. The stable ID distinguishes
repository paths and backends, including repositories with the same name.
To choose another disk or directory, set `root` in your existing `[workspace]`
section in `~/.config/mentat/config.toml` (or `$XDG_CONFIG_HOME/mentat/config.toml`):

```toml
[workspace]
kind = "jj" # or "git"
root = "~/code/mentat-workspaces"
```

The root must be absolute or start with `~/`. Changing it affects new workspaces;
existing workspaces keep their recorded paths and conversations. Mentat's database
and locks remain in `$XDG_STATE_HOME/mentat` (default `~/.local/state/mentat`).
Working copies still share their source repository, so jj workspace commits remain
visible in that repository's log regardless of where their directories live.

## Linear workspace per repository

Start Mentat in your repository and press `l` to open the Linear workspace picker.
With `linear.api_key`, the picker discovers the workspace available to your key;
press Enter to map it to this repository, `r` to retry, or Esc to cancel. It also
opens automatically for an unconfigured repo. With OAuth, Enter opens browser
sign-in to choose the workspace. The workspace ID and name are saved against
the canonical repository path in `mentat.db`, shared by the Git and jj backends.
Press `s` to see the mapped workspace and repository.

Each repository keeps its own cached issues and OAuth credentials. A new repo
must be connected explicitly; it does not inherit the old global issue cache or
login. Refreshes and reconnections verify the saved workspace ID and reject
credentials for a different workspace. There is no team or project filtering:
all your open assigned issues in the mapped workspace are shown. Reassigning an
already mapped repo to another Linear workspace is not yet exposed in the UI.

## Logs

Mentat writes structured JSON logs to `$XDG_STATE_HOME/mentat/logs`, defaulting to
`~/.local/state/mentat/logs`. Press `s` to see the log directory. Files are named
`mentat.YYYY-MM-DD.jsonl`, rotate daily in UTC, and retain at most seven matching
files. Older files are cleaned up on startup and when a new daily file is opened;
no external cleanup job is needed. The log directory is private (mode `700`).

Logging defaults to `info`. For debugging, launch with:

```sh
MENTAT_LOG=debug mentat
```

Supported levels are `off`, `error`, `warn`, `info`, `debug`, and `trace`. Logs
include application version, process ID, operation timings, startup and shutdown,
agent preparation, workspace lifecycle, and categorized failures. Logs omit API
keys, OAuth tokens, ticket titles/bodies, terminal output, and raw remote errors.
Dependency logging is disabled even at debug/trace level. File writes use a
bounded background queue and flush on normal shutdown; under sustained overload,
logs may be dropped to keep the UI responsive.
