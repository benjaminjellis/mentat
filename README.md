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

## Ticket details

Press `i` in the sidebar to open the selected ticket's details and comments.
Press `e` with the ticket open to edit it.
Use the arrow keys or `j` and `k` to scroll the ticket details.
Use `Ctrl-Up` and `Ctrl-Down` to scroll the edit form.
Press `Esc` to return from the edit form to the ticket details.
Press `Ctrl-G` to return to the sidebar.
In the sidebar, `e` opens the configured editor.

## Linear bookmarks

Select a ticket in the sidebar and press `b` to set a jj bookmark from its Linear branch name.
Mentat sets the bookmark at `@` in the ticket workspace, then tracks it on `origin`.
Mentat creates the workspace if it does not exist.
The command can update an existing bookmark.
Press `r` to refresh issues if the branch name is absent or out of date.

Press `p` in the sidebar to run `jj git push` in the selected ticket's workspace.
The command uses the existing jj remote and bookmark settings.
Mentat shows the command result in the activity panel.
