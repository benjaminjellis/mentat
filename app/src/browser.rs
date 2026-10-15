use std::process::{Command, Stdio};

/// The platform's URL handler.
pub fn program() -> &'static str {
    if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

/// Open a URL with every stream detached, so a misbehaving handler cannot
/// write over the terminal Mentat is drawing on. Callers decide whether to
/// wait for it or to poll the child later.
pub fn open(url: &str) -> Command {
    let mut command = Command::new(program());
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}
