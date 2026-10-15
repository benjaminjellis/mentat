use crate::{codex_session, config::Config, settings::AgentKind};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
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
    if config.trust_workspaces
        && let Err(error) = trust(&config.cwd)
    {
        // A workspace that stays untrusted only costs the question; still start.
        crate::logging::failure("claude_trust", &error);
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

/// Claude Code asks about each new directory, and every ticket workspace is one.
/// Mentat cut these from a repository the user already works in, so record the
/// answer the user configured rather than losing the ticket to the question.
fn trust(cwd: &Path) -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .context("Cannot locate the home directory")?;
    trust_in(&home, cwd)
}

fn trust_in(home: &Path, cwd: &Path) -> Result<()> {
    let path = home.join(".claude.json");
    let Ok(text) = fs::read_to_string(&path) else {
        // Claude Code has not run yet; it will ask once and remember the answer.
        return Ok(());
    };
    let mut root: Value = serde_json::from_str(&text).context("Cannot read ~/.claude.json")?;
    let key = cwd
        .to_str()
        .context("Workspace path must be UTF-8")?
        .to_owned();
    let projects = root
        .as_object_mut()
        .context("~/.claude.json is not an object")?
        .entry("projects")
        .or_insert_with(|| json!({}));
    let entry = projects
        .as_object_mut()
        .context("Claude projects are not an object")?
        .entry(key)
        .or_insert_with(|| json!({}));
    let entry = entry
        .as_object_mut()
        .context("Claude project entry is not an object")?;
    if entry.get("hasTrustDialogAccepted").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    entry.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
    // Replace the file in one step so a concurrent reader never sees half of it.
    let temporary = path.with_extension("json.mentat");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    std::io::Write::write_all(&mut file, serde_json::to_string(&root)?.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, &path)?;
    tracing::info!(workspace = %cwd.display(), "Recorded workspace trust for Claude Code");
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
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn recorded_trust_keeps_the_rest_of_the_claude_configuration() {
        let home = tempfile::tempdir().unwrap();
        let workspace = home.path().join("checkouts/FOR-1");
        // No configuration yet: Claude Code asks once and remembers the answer.
        trust_in(home.path(), &workspace).unwrap();
        assert!(!home.path().join(".claude.json").exists());
        let path = home.path().join(".claude.json");
        fs::write(
            &path,
            r#"{"userID":"keep","projects":{"/other":{"hasTrustDialogAccepted":true,"lastCost":1}}}"#,
        )
        .unwrap();
        trust_in(home.path(), &workspace).unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["userID"], "keep");
        assert_eq!(value["projects"]["/other"]["lastCost"], 1);
        assert_eq!(
            value["projects"][workspace.to_str().unwrap()]["hasTrustDialogAccepted"],
            true
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // A second run leaves the file alone.
        let before = fs::read_to_string(&path).unwrap();
        trust_in(home.path(), &workspace).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

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
