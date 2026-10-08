use ::linear::{Config, Issue, Progress, Snapshot, SyncResult};
use std::{
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
};

enum Update {
    Status(String),
    Done(Result<SyncResult, String>),
}

pub struct Linear {
    pub snapshot: Option<Snapshot>,
    pub selected: usize,
    pub restore_selection: Option<String>,
    pub status: String,
    pub busy: bool,
    pub tracked: Vec<String>,
    pub closed: Vec<String>,
    config: Option<Config>,
    receiver: Option<Receiver<Update>>,
    worker: Option<JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
}

impl Linear {
    pub fn new(config: Option<Config>, tracked: Vec<String>) -> Self {
        let mut state = Self {
            snapshot: config.as_ref().and_then(::linear::cached),
            selected: 0,
            restore_selection: None,
            tracked,
            closed: Vec::new(),
            status: "Linear: not connected".into(),
            busy: false,
            config,
            receiver: None,
            worker: None,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        if state.snapshot.is_some() {
            state.status = "Linear: cached issues".into();
        }
        if state.config.is_some() {
            state.start(false);
        }
        state
    }

    pub fn issue(&self) -> Option<&Issue> {
        self.snapshot.as_ref()?.issues.get(self.selected)
    }

    pub fn select(&mut self, step: isize) {
        let count = self
            .snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.issues.len());
        if count > 0 {
            self.selected = self.selected.saturating_add_signed(step).min(count - 1);
        }
    }

    pub fn start(&mut self, connect: bool) {
        if self.busy {
            return;
        }
        let Some(config) = self.config.clone() else {
            self.status = "Set linear.api_key in config".into();
            return;
        };
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.cancel.store(false, Ordering::Relaxed);
        let cancel = self.cancel.clone();
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        self.busy = true;
        self.status = if connect {
            "Linear: open browser sign-in"
        } else {
            "Linear: refresh issues"
        }
        .into();
        let tracked = self.tracked.clone();
        self.worker = Some(thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(anyhow::Error::from)
                .and_then(|runtime| {
                    runtime.block_on(::linear::sync(
                        &config,
                        connect,
                        &tracked,
                        &cancel,
                        |progress| {
                            let status = match progress {
                                Progress::Status(message) => message,
                                Progress::Browser(url) => {
                                    let opener = if cfg!(target_os = "macos") {
                                        "open"
                                    } else {
                                        "xdg-open"
                                    };
                                    match Command::new(opener)
                                        .arg(&url)
                                        .stdin(Stdio::null())
                                        .stdout(Stdio::null())
                                        .stderr(Stdio::null())
                                        .status()
                                    {
                                        Ok(exit) if exit.success() => {
                                            "Complete Linear sign-in in your browser".into()
                                        }
                                        _ => {
                                            format!("Open this sign-in URL in your browser: {url}")
                                        }
                                    }
                                }
                            };
                            let _ = sender.send(Update::Status(status));
                        },
                    ))
                })
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(Update::Done(result));
        }));
    }

    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        let Some(receiver) = &self.receiver else {
            return false;
        };
        while let Ok(update) = receiver.try_recv() {
            changed = true;
            match update {
                Update::Status(message) => self.status = message,
                Update::Done(result) => {
                    self.busy = false;
                    match result {
                        Ok(result) => {
                            self.closed = result.closed;
                            let snapshot = result.snapshot;
                            let selected = self
                                .restore_selection
                                .take()
                                .or_else(|| self.issue().map(|issue| issue.id.clone()));
                            self.selected = selected
                                .and_then(|id| {
                                    snapshot.issues.iter().position(|issue| issue.id == id)
                                })
                                .unwrap_or(0);
                            self.status =
                                format!("Linear: {} assigned issues", snapshot.issues.len());
                            self.snapshot = Some(snapshot);
                        }
                        Err(error) => {
                            let suffix = if self.snapshot.is_some() {
                                " Cached issues remain available."
                            } else {
                                ""
                            };
                            self.status = format!("{error}{suffix}");
                        }
                    }
                }
            }
        }
        changed
    }
}

impl Drop for Linear {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_refresh_keeps_cached_issues() {
        let mut state = Linear::new(None, vec![]);
        state.snapshot = Some(Snapshot {
            viewer: "Cached".into(),
            issues: vec![],
        });
        let (sender, receiver) = mpsc::channel();
        state.receiver = Some(receiver);
        state.busy = true;
        sender.send(Update::Done(Err("Failed".into()))).unwrap();
        assert!(state.poll());
        assert!(!state.busy);
        assert_eq!(state.snapshot.as_ref().unwrap().viewer, "Cached");
        assert!(state.status.contains("Cached issues remain available"));
    }
}
