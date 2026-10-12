use ::linear::{Config, Issue, Progress, Snapshot, Store, SyncResult};
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
    Discovered(Result<::linear::Workspace, String>),
    Mapped(Result<::linear::Workspace, String>),
}

pub struct WorkspacePicker {
    pub available: Option<::linear::Workspace>,
    pub message: String,
    pub api_key: bool,
    pub saving: bool,
}

pub struct Linear {
    pub snapshot: Option<Snapshot>,
    pub picker: Option<WorkspacePicker>,
    pub workspace: Option<::linear::Workspace>,
    pub selected: usize,
    pub status: String,
    pub busy: bool,
    pub tracked: Vec<String>,
    pub closed: Vec<String>,
    config: Option<Config>,
    store: Store,
    receiver: Option<Receiver<Update>>,
    worker: Option<JoinHandle<()>>,
    retired_workers: Vec<JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
}

impl Linear {
    pub fn new(
        config: Option<Config>,
        tracked: Vec<String>,
        store: Store,
        snapshot: Option<Snapshot>,
        workspace: Option<::linear::Workspace>,
    ) -> Self {
        let mut state = Self {
            snapshot,
            picker: None,
            workspace,
            selected: 0,
            tracked,
            closed: Vec::new(),
            status: "Linear: not connected".into(),
            busy: false,
            config,
            store,
            receiver: None,
            worker: None,
            retired_workers: Vec::new(),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        if state.snapshot.is_some() {
            state.status = "Linear: cached issues".into();
        }
        if state.config.is_some() {
            if state.workspace.is_none() {
                state.open_picker();
            } else {
                state.start(false);
            }
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
        if self.busy || self.picker.is_some() {
            return;
        }
        let Some(config) = self.config.clone() else {
            self.status = "Set linear.api_key in config".into();
            return;
        };
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.cancel = Arc::new(AtomicBool::new(false));
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
        let store = self.store.clone();
        let span = tracing::Span::current();
        self.worker = Some(thread::spawn(move || {
            let _entered = span.enter();
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(anyhow::Error::from)
                .and_then(|runtime| {
                    runtime.block_on(::linear::sync(
                        &config,
                        &store,
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

    fn stop_worker(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            // Cancellation can overlap a database read. Never block the UI on it.
            self.retired_workers.push(worker);
        }
        self.receiver = None;
        self.busy = false;
    }

    pub fn open_picker(&mut self) {
        tracing::debug!("Opening Linear workspace picker");
        self.stop_worker();
        let config = self.config.clone();
        let api_key = config.as_ref().is_some_and(|c| c.api_key.is_some());
        self.picker = Some(WorkspacePicker {
            available: None,
            message: if api_key {
                "Finding your workspace…"
            } else if config.is_some() {
                "Choose a workspace during browser sign-in."
            } else {
                "Set linear.api_key in your Mentat config, then restart."
            }
            .into(),
            api_key,
            saving: false,
        });
        if !api_key {
            return;
        }
        let config = config.unwrap();
        self.launch_picker(move |_store, cancel| {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(anyhow::Error::from)
                .and_then(|runtime| runtime.block_on(::linear::api_key_workspace(&config, &cancel)))
                .map_err(|error| format!("{error:#}"));
            Update::Discovered(result)
        });
    }

    fn launch_picker(
        &mut self,
        operation: impl FnOnce(Store, Arc<AtomicBool>) -> Update + Send + 'static,
    ) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();
        let store = self.store.clone();
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        self.busy = true;
        let span = tracing::Span::current();
        self.worker = Some(thread::spawn(move || {
            let _entered = span.enter();
            let _ = sender.send(operation(store, cancel));
        }));
    }

    pub fn close_picker(&mut self) {
        // Once Enter commits a selection, wait for the save result rather than
        // hiding a potentially successful write behind a cancellation message.
        if self.picker.as_ref().is_some_and(|p| p.saving) {
            return;
        }
        self.stop_worker();
        self.picker = None;
    }

    pub fn confirm_workspace(&mut self) {
        if self.busy {
            return;
        }
        let Some(picker) = &mut self.picker else {
            return;
        };
        if !picker.api_key {
            if self.config.is_some() {
                self.picker = None;
                self.start(true);
            }
            return;
        }
        let Some(workspace) = picker.available.clone() else {
            return;
        };
        picker.saving = true;
        picker.message = "Saving workspace…".into();
        self.launch_picker(move |store, _cancel| {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(anyhow::Error::from)
                .and_then(|runtime| runtime.block_on(store.bind_workspace(&workspace, true)))
                .map(|()| workspace)
                .map_err(|error| format!("{error:#}"));
            Update::Mapped(result)
        });
    }

    pub fn poll(&mut self) -> bool {
        let mut index = 0;
        while index < self.retired_workers.len() {
            if self.retired_workers[index].is_finished() {
                let _ = self.retired_workers.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
        let mut changed = false;
        let Some(receiver) = &self.receiver else {
            return false;
        };
        let updates: Vec<_> = receiver.try_iter().collect();
        let mut refresh = false;
        for update in updates {
            changed = true;
            match update {
                Update::Status(message) => self.status = message,
                Update::Discovered(result) => {
                    self.busy = false;
                    if let Some(picker) = &mut self.picker {
                        match result {
                            Ok(workspace) => {
                                picker.available = Some(workspace);
                                picker.message =
                                    "Enter to use this workspace for this repository.".into();
                            }
                            Err(error) => {
                                tracing::warn!(
                                    operation = "workspace_discovery",
                                    "Linear operation failed"
                                );
                                picker.message = error;
                            }
                        }
                    }
                }
                Update::Mapped(result) => {
                    self.busy = false;
                    match result {
                        Ok(workspace) => {
                            self.workspace = Some(workspace);
                            self.picker = None;
                            refresh = true;
                        }
                        Err(error) => {
                            tracing::warn!(
                                operation = "workspace_mapping",
                                "Linear operation failed"
                            );
                            if let Some(picker) = &mut self.picker {
                                picker.saving = false;
                                picker.message = error;
                            }
                        }
                    }
                }
                Update::Done(result) => {
                    self.busy = false;
                    match result {
                        Ok(result) => {
                            self.workspace = Some(result.workspace.clone());
                            self.closed = result.closed;
                            let snapshot = result.snapshot;
                            let selected = self.issue().map(|issue| issue.id.clone());
                            self.selected = selected
                                .and_then(|id| {
                                    snapshot.issues.iter().position(|issue| issue.id == id)
                                })
                                .unwrap_or(0);
                            self.status = format!(
                                "Linear: {} — {} assigned issues",
                                result.workspace.name,
                                snapshot.issues.len()
                            );
                            self.snapshot = Some(snapshot);
                        }
                        Err(error) => {
                            tracing::warn!(
                                operation = "sync",
                                cached = self.snapshot.is_some(),
                                "Linear operation failed"
                            );
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
        if refresh {
            self.start(false);
        }
        changed
    }
}

impl Drop for Linear {
    fn drop(&mut self) {
        self.stop_worker();
        for worker in self.retired_workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn picker_only_saves_after_enter_and_preserves_mapping_on_cancel() {
        let temp = tempfile::tempdir().unwrap();
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(temp.path().join("mentat.db"))
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let store = Store::for_repository(pool.clone(), "/forge/exchange".into());
        let mut state = Linear::new(None, vec![], store.clone(), None, None);
        let workspace = ::linear::Workspace {
            id: "forge-id".into(),
            name: "Forge".into(),
        };
        state.open_picker();
        let (sender, receiver) = mpsc::channel();
        state.receiver = Some(receiver);
        state.busy = true;
        sender
            .send(Update::Discovered(Ok(workspace.clone())))
            .unwrap();
        state.poll();
        assert_eq!(
            state.picker.as_ref().unwrap().available,
            Some(workspace.clone())
        );
        assert!(store.workspace().await.unwrap().is_none());
        state.close_picker();
        assert!(state.picker.is_none());
        assert!(store.workspace().await.unwrap().is_none());
        state.picker = Some(WorkspacePicker {
            available: Some(workspace.clone()),
            api_key: true,
            saving: false,
            message: String::new(),
        });
        state.confirm_workspace();
        assert!(state.picker.as_ref().unwrap().saving);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while state.busy {
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            state.poll();
        }
        assert!(state.picker.is_none());
        assert_eq!(state.workspace, Some(workspace.clone()));
        assert_eq!(store.workspace().await.unwrap(), Some(workspace));
    }

    #[tokio::test]
    async fn picker_discovery_errors_stay_visible_without_changing_mapping() {
        let mut state = Linear::new(
            None,
            vec![],
            Store::new(sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap()),
            None,
            None,
        );
        state.open_picker();
        let (sender, receiver) = mpsc::channel();
        state.receiver = Some(receiver);
        state.busy = true;
        sender
            .send(Update::Discovered(Err(
                "Cannot identify the Linear workspace".into(),
            )))
            .unwrap();
        state.poll();
        assert!(!state.busy);
        assert!(state.workspace.is_none());
        let picker = state.picker.as_ref().unwrap();
        assert!(picker.available.is_none());
        assert!(picker.message.contains("Cannot identify"));
    }

    #[tokio::test]
    async fn failed_refresh_keeps_cached_issues() {
        let mut state = Linear::new(
            None,
            vec![],
            Store::new(sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap()),
            None,
            None,
        );
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
