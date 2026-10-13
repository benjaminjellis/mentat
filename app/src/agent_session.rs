use crate::{codex_session, config::Config, settings::AgentKind};
use anyhow::{Context, Result};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, TryRecvError},
};
use std::thread::{self, JoinHandle};

pub struct Startup {
    receiver: Receiver<Result<Config>>,
    worker: Option<JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
    pub paste: Option<linear::Issue>,
}

impl Startup {
    pub fn start(mut config: Config, paste: Option<linear::Issue>) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let (sender, receiver) = mpsc::channel();
        let span = tracing::Span::current();
        let worker = thread::spawn(move || {
            let _entered = span.enter();
            let started = std::time::Instant::now();
            let result = prepare(&mut config, worker_cancel).map(|()| config);
            tracing::debug!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                success = result.is_ok(),
                "Agent preparation finished"
            );
            let _ = sender.send(result);
        });
        Self {
            receiver,
            worker: Some(worker),
            cancel,
            paste,
        }
    }

    pub fn poll(&self) -> Result<Option<Config>> {
        match self.receiver.try_recv() {
            Ok(result) => result.map(Some),
            Err(TryRecvError::Empty) => Ok(None),
            Err(error) => Err(error).context("Agent startup stopped unexpectedly"),
        }
    }
}

impl Drop for Startup {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn prepare(config: &mut Config, cancel: Arc<AtomicBool>) -> Result<()> {
    if !config.agent_mode {
        return Ok(());
    }
    if config.agent == AgentKind::Codex {
        return codex_session::prepare(config, cancel);
    }
    let (id, resume) = match &config.resume {
        Some(id) if !config.new_conversation => (id.clone(), true),
        _ => (new_id()?, false),
    };
    config.command = vec![
        "claude".into(),
        if resume { "--resume" } else { "--session-id" }.into(),
        id.clone().into(),
    ];
    config.resume = Some(id);
    Ok(())
}

fn new_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("Cannot obtain a session ID"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = format!("{:032x}", u128::from_be_bytes(bytes));
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cli::Cli, settings::Settings};

    #[test]
    fn claude_uses_a_stable_id_for_resume() {
        let mut config = Config::resolve(
            Cli {
                agent: Some(AgentKind::Claude),
                editor: false,
                command: vec![],
                cwd: "/tmp".into(),
                escape: None,
                protect_child: false,
                agent_mode: true,
                resume: None,
                new_conversation: false,
            },
            Settings::default(),
        )
        .unwrap();
        prepare(&mut config, Arc::new(AtomicBool::new(false))).unwrap();
        let id = config.resume.clone().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(config.command[1], "--session-id");
        prepare(&mut config, Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(config.command[1], "--resume");
        assert_eq!(config.resume.as_deref(), Some(id.as_str()));
        config.new_conversation = true;
        prepare(&mut config, Arc::new(AtomicBool::new(false))).unwrap();
        assert_ne!(config.resume.as_deref(), Some(id.as_str()));
    }
}
