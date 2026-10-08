# Mentat

Mentat is a Rust terminal application for Linear tickets. Each ticket has a Jujutsu workspace and separate agent, editor, and shell terminals.
Mentat requires macOS or Linux, `jj`, and the selected agent and editor executables.

## Run

Start Mentat inside a Jujutsu repository:

```sh
cargo run
cargo run -- --cwd /path/to/repo
cargo run -- --claude
cargo run -- --codex
cargo run -- --shell
cargo run -- --editor
cargo run -- -- nvim --clean
```

The manager opens first. No agent, editor, or shell starts until you request it.
The `--shell`, `--editor`, and custom command options replace the primary agent command. They still use the selected ticket workspace.

| Key in the manager | Action |
| --- | --- |
| `j`, `k`, Up, Down | Select a ticket |
| Enter | Open or resume the selected ticket's agent |
| `c` | Open the agent and paste the ticket title and body as an unsent draft |
| `e` | Open the configured editor in the ticket workspace |
| `t` | Open a shell in the ticket workspace |
| `l` | Connect Linear or refresh when an API key is present |
| `r` | Refresh Linear |
| `s` | Show status and workspace path |
| `?` | Show help |
| `q`, Ctrl-C | Quit Mentat |

Ctrl-G returns from a child terminal to the manager. Ctrl-H selects the sidebar. Ctrl-L returns to the last live terminal.
Ctrl-J and Ctrl-K reserve navigation keys but have no action without a vertical pane.
Mentat forwards other child input, including Ctrl-C, to that child.
The sidebar disappears below 70 columns.

The `c` action waits for bracketed paste support. It never sends Enter after the ticket text.
Enter opens the agent without another copy of the ticket. Shells, editors, and custom primary commands do not receive ticket text.

## Configuration

Mentat creates `~/.config/mentat/config.toml` on normal startup on both macOS and Linux.
An absolute `XDG_CONFIG_HOME` changes the base directory. Mentat preserves existing settings and supplies defaults for absent settings.
If an existing file lacks `[agent]`, startup adds that section with the Codex default.
It does not copy configuration from previous macOS locations.

```toml
[agent]
kind = "codex" # codex or claude

[editor]
kind = "nvim" # nvim, vim, or emacs
# executable = "/custom/path/nvim"
args = []

[keybindings]
return_to_sidebar = "ctrl-g"
pane_navigation = true

[linear]
# api_key = "lin_api_..."
# client_id = "your-public-oauth-client-id"
```

CLI options `--codex` and `--claude` override the agent setting.
Editor presets run `nvim`, `vim`, or `emacs -nw`. An executable override replaces the program path; `args` appends arguments.
Mentat starts the editor only after `e`, or Enter in explicit `--editor` mode. It does not restore editor buffers.

For Emacs, set `return_to_sidebar = "ctrl-]"` and `pane_navigation = false` to preserve its Ctrl-G and Ctrl-H commands.
The `--escape` option overrides the return shortcut. Supported chords include control letters other than `i` and `m`, and `ctrl-]`, `ctrl-\\`, `ctrl-^`, `ctrl-_`.
Mentat rejects conflicts with enabled pane shortcuts. Legacy terminal encodings can make Ctrl-H equal Backspace and Ctrl-] equal Ctrl-5.

New configuration files use mode 600. Their directory uses mode 700. Mentat rejects configuration files with group or other permissions.
Invalid TOML or unknown settings stop startup. Diagnostics do not expose configuration values.
Restart Mentat after configuration edits.

## Linear

Set `linear.api_key` for direct access. An API key takes precedence over OAuth.
Alternatively, set `linear.client_id`, or the `LINEAR_CLIENT_ID` environment variable, and press `l`.
Register `http://127.0.0.1:43821/oauth/callback` as the OAuth callback URL.
Mentat uses read-only OAuth with S256 PKCE and callback state validation.
The loopback listener closes after consent, cancellation, or a three-minute timeout.

Mentat fetches assigned tickets except completed and canceled tickets. It refreshes every minute and follows pagination.
A separate worker runs requests with timeouts and cancellation. Failed refreshes retain cached tickets.
Mentat stores OAuth credentials and the ticket cache in the configuration directory as private files.
It does not reuse credentials from the ChatGPT Linear connector.

## Workspaces and state

The first tool request for a ticket creates a workspace through `jj workspace add -r @`.
The new workspace starts from the source workspace's current revision, including its current changes.
Every tool for that ticket starts in this workspace. Other tickets use separate workspaces.
A file lock prevents two Mentat instances from using the same ticket workspace concurrently.

Mentat stores state in `~/.local/state/mentat`, or `$XDG_STATE_HOME/mentat` with an absolute override:

- `mentat.db`: SQLite records for repositories, ticket workspaces, selection, and agent session IDs.
- `workspaces/`: managed Jujutsu workspace directories and locks.

SQLx runs embedded migrations automatically on startup through `sqlx::migrate!`.
Normal restarts preserve the database. During schema development, use a fresh test state directory rather than upgrade old development databases.
Mentat does not migrate earlier JSON conversation records.

Mentat checks tracked ticket states separately from the assigned list.
Only an explicit completed or canceled state schedules workspace removal. Reassignment and failed network requests do not trigger removal.
Cleanup waits for ticket terminals to exit and for the working revision to have no local changes.
Mentat validates the managed workspace path before removal, forgets the JJ workspace, and removes its database records.
It retries deferred cleanup while Mentat remains open.

The database keeps separate Codex and Claude session IDs per ticket and repository.
Codex uses a temporary app-server to obtain an explicit thread ID, then starts `codex resume --no-daemon ID --cd WORKSPACE`.
Claude starts with `claude --session-id UUID` and restores a conversation with `claude --resume UUID`.
The agent owns the conversation transcript. SQLite stores its ID, not a transcript copy.
An agent must persist a conversation before it can resume it. An empty Claude session may have no transcript yet.
Use `--resume ID` to adopt a conversation, or `--new-conversation` to start a new one for the first ticket agent you open.

## Shutdown and resource use

View changes preserve live child processes and terminal screens. They do not start another copy of a child.
The editor starts only on request. Restarts restore the ticket selection and workspace mappings, then wait for a tool request.

If an editor, shell, or custom primary command remains open, quit offers two choices:

- Esc returns to the tools so you can save and exit them.
- `f` force quits. This discards unsaved editor buffers and stops shell jobs.

On exit, Mentat captures child process groups, including background job groups, and sends SIGHUP.
After an 800 ms grace period, it sends SIGKILL to remaining groups, reaps direct children, and joins PTY readers.
Terminal disconnection triggers cleanup without an interactive prompt.
The input backend handles terminal EOF instead of repeatedly reading a disconnected terminal.

The event loop waits for input and draws only after a change. Each PTY reader waits for output.
Agent and editor activity still consumes the same resources those programs require in ordinary terminal tabs.
A child that deliberately detaches and reparents itself can escape process-tree cleanup. SIGKILL or a process abort also bypasses normal cleanup.

## Crates

- `app`: executable, configuration, application state, SQLx migrations, workspace coordination, and agent launch.
- `ui`: layout, ticket views, popups, terminal display, and cursor position.
- `terminal`: PTY ownership, process cleanup, emulator state, replies, and input encoding.
- `linear`: async GraphQL through `lineark-sdk`, OAuth, token refresh, and cache.

The library crates do not depend on one another. `app` coordinates them.
`ui` takes borrowed display data and a `vt100::Screen`. It owns no process or network worker.
`portable-pty` owns terminals, `vt100` parses output, and Ratatui renders through Crossterm.
The emulator supports common terminal input, resize, colors, alternate screens, and bracketed paste.
Mouse input, a scrollback UI, graphics, and advanced keyboard protocols remain outside this prototype.

## Checks

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo test -p terminal --test pty neovim -- --ignored --nocapture
cargo test -p terminal --test pty vim -- --ignored --nocapture
```

The standard suite requires `jj`, Unix PTYs, `ps`, and `/bin/sh`. Editor checks require the named local editor.
Tests use temporary repositories and fresh SQLite databases. They do not modify the user's configuration or state.
Live authenticated model requests and Linux execution need separate validation.
