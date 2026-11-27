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

From the sidebar, select a ticket and use these workspace shortcuts:

| Shortcut | Action |
| --- | --- |
| `t` | Open the internal shell |
| `Shift-T` | Open the workspace in Ghostty (macOS) |
| `Shift-F` | Open the workspace in Finder (macOS) |

Mentat creates the ticket's workspace if it does not exist. Press `Ctrl-G` to return to the sidebar from a terminal pane.

