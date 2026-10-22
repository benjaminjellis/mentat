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

