#[cfg(not(unix))]
compile_error!("Mentat currently supports macOS and Linux only");

pub mod input;
pub mod pty;
mod replies;
