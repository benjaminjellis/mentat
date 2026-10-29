use anyhow::{Context, Result, bail, ensure};
use std::{
    io::Write,
    process::{Command, Stdio},
};

fn command(copy: bool) -> Result<Command> {
    if std::env::var_os("SSH_TTY").is_some() || std::env::var_os("SSH_CONNECTION").is_some() {
        bail!("Use the terminal paste shortcut to paste from the remote clipboard");
    }
    let mut command = if cfg!(target_os = "macos") {
        Command::new(if copy { "pbcopy" } else { "pbpaste" })
    } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        let mut command = Command::new(if copy { "wl-copy" } else { "wl-paste" });
        if !copy {
            command.arg("--no-newline");
        }
        command
    } else {
        let mut command = Command::new("xclip");
        command.args(["-selection", "clipboard", if copy { "-i" } else { "-o" }]);
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

pub fn copy(text: &str) -> Result<()> {
    let mut child = command(true)?
        .stdin(Stdio::piped())
        .spawn()
        .context("Cannot access the system clipboard")?;
    let result = child
        .stdin
        .take()
        .context("Cannot write to the system clipboard")?
        .write_all(text.as_bytes());
    let status = child.wait()?;
    result?;
    ensure!(status.success(), "Cannot copy to the system clipboard");
    Ok(())
}

pub fn paste() -> Result<String> {
    let output = command(false)?
        .stdout(Stdio::piped())
        .output()
        .context("Cannot access the system clipboard; use the terminal paste shortcut")?;
    ensure!(
        output.status.success(),
        "Cannot read the system clipboard; use the terminal paste shortcut"
    );
    String::from_utf8(output.stdout).context("The clipboard does not contain UTF-8 text")
}
