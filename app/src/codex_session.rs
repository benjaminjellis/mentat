use crate::config::Config;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Obtain an ID through the structured API before the interactive CLI starts.
/// Never infer identity from terminal text, timestamps, or `resume --last`.
pub fn prepare(config: &mut Config, cancel: Arc<AtomicBool>) -> Result<()> {
    if !config.agent_mode {
        return Ok(());
    }
    let explicit = config.resume.clone();
    let id = if let Some(id) = explicit {
        validate_id(&id)?;
        let mut rpc = Rpc::start(&config.cwd, cancel.clone())?;
        let response = rpc.request(
            "thread/read",
            json!({"threadId": id, "includeTurns": false}),
        )?;
        ensure!(
            response.pointer("/thread/id").and_then(Value::as_str) == Some(&id),
            "Codex returned a different conversation ID"
        );
        id
    } else {
        let mut rpc = Rpc::start(&config.cwd, cancel.clone())?;
        let response = rpc.request(
            "thread/start",
            json!({"cwd": config.cwd, "ephemeral": false}),
        )?;
        let id = response
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .context("Codex did not return a conversation ID")?
            .to_owned();
        validate_id(&id)?;
        // Save a human-readable name through the supported API before closing the helper.
        rpc.request(
            "thread/name/set",
            json!({"threadId": id, "name": format!("Mentat: {}", config.cwd.display())}),
        )?;
        id
    };
    config.command = vec![
        "codex".into(),
        "resume".into(),
        "--no-daemon".into(),
        id.clone().into(),
        "--cd".into(),
        config.cwd.as_os_str().to_owned(),
    ];
    config.resume = Some(id);
    Ok(())
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 36
            && id.bytes().enumerate().all(|(i, c)| {
                if [8, 13, 18, 23].contains(&i) {
                    c == b'-'
                } else {
                    c.is_ascii_hexdigit()
                }
            }),
        "Codex returned an unsupported conversation ID; expected a UUID"
    );
    Ok(())
}

struct Rpc {
    child: Child,
    input: Option<ChildStdin>,
    output: Receiver<Value>,
    reader: Option<JoinHandle<()>>,
    next_id: u64,
    signals: Signals,
}

struct Signals {
    stop: Arc<AtomicBool>,
    ids: Vec<signal_hook::SigId>,
}

impl Signals {
    fn new(stop: Arc<AtomicBool>) -> Result<Self> {
        let mut signals = Self {
            stop,
            ids: Vec::new(),
        };
        for signal in [
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
        ] {
            signals
                .ids
                .push(signal_hook::flag::register(signal, signals.stop.clone())?);
        }
        Ok(signals)
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

impl Rpc {
    fn start(cwd: &Path, cancel: Arc<AtomicBool>) -> Result<Self> {
        let signals = Signals::new(cancel)?;
        let mut child = Command::new("codex")
            .args(["app-server", "--listen", "stdio://"])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .context("Cannot start Codex session helper")?;
        let input = child.stdin.take();
        let stdout = child.stdout.take().expect("piped stdout");
        let (sender, output) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if let Ok(value) = serde_json::from_str(&line)
                    && sender.send(value).is_err()
                {
                    break;
                }
            }
        });
        let mut rpc = Self {
            child,
            input,
            output,
            reader: Some(reader),
            next_id: 0,
            signals,
        };
        rpc.request(
            "initialize",
            json!({"clientInfo": {"name": "mentat", "version": env!("CARGO_PKG_VERSION")}}),
        )?;
        rpc.send(json!({"method": "initialized"}))?;
        Ok(rpc)
    }

    fn send(&mut self, value: Value) -> Result<()> {
        let input = self.input.as_mut().context("Codex session helper closed")?;
        serde_json::to_writer(&mut *input, &value)?;
        input.write_all(b"\n")?;
        input.flush()?;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let started = Instant::now();
        tracing::debug!(method, "Codex helper request started");
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"id": id, "method": method, "params": params}))?;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            ensure!(
                !self.signals.stop.load(Ordering::Relaxed),
                "Codex session setup cancelled"
            );
            ensure!(
                Instant::now() < deadline,
                "Codex session helper did not reply; check `codex doctor`"
            );
            let response = match self.output.recv_timeout(Duration::from_millis(100)) {
                Ok(response) => response,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(error) => {
                    return Err(error).context("Codex session helper closed; check `codex doctor`");
                }
            };
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = response.get("error") {
                tracing::warn!(
                    method,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "Codex helper rejected request"
                );
                bail!(
                    "Codex {method} failed: {}",
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                );
            }
            tracing::debug!(
                method,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "Codex helper request finished"
            );
            return response
                .get("result")
                .cloned()
                .context("Codex omitted the result");
        }
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        drop(self.input.take());
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        // SAFETY: the helper owns a separate group. Terminate remaining helper descendants too.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_explicit_conversation_ids() {
        assert!(validate_id("01a11c20-c173-77f3-8512-3bee052093bc").is_ok());
        assert!(validate_id("--last").is_err());
    }
}
