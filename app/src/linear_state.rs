use ::linear::{Config, Issue, Progress, Snapshot, Store, SyncResult};
use std::future::Future;
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

enum Update {
    Edited(String, Box<Result<::linear::EditResult, String>>),
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
    pub edited: Vec<(String, Result<::linear::EditResult, String>)>,
    pub snapshot: Option<Snapshot>,
    pub picker: Option<WorkspacePicker>,
    pub workspace: Option<::linear::Workspace>,
    pub selected: usize,
    pub search_query: String,
    pub status_filter: Option<String>,
    pub status: String,
    /// When the current operation started. `None` means idle; the UI animates
    /// from this instant rather than keeping a frame counter of its own.
    busy_since: Option<Instant>,
    editing_busy: bool,
    picker_pending: bool,
    pub tracked: Vec<String>,
    pub closed: Vec<String>,
    config: Option<Config>,
    store: Store,
    receiver: Option<Receiver<Update>>,
    worker: Option<JoinHandle<()>>,
    retired_workers: Vec<JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
}

/// Run one async operation on a runtime of its own. Workers are plain threads,
/// so each owns its reactor for the lifetime of a single request.
fn blocking<T>(operation: impl Future<Output = anyhow::Result<T>>) -> Result<T, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| runtime.block_on(operation))
        .map_err(|error| format!("{error:#}"))
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
            edited: vec![],
            snapshot,
            picker: None,
            workspace,
            selected: 0,
            search_query: String::new(),
            status_filter: None,
            tracked,
            closed: Vec::new(),
            status: "Linear: not connected".into(),
            busy_since: None,
            editing_busy: false,
            picker_pending: false,
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
        self.issues().nth(self.selected)
    }

    pub fn issues(&self) -> impl Iterator<Item = &Issue> {
        let query = self.search_query.trim().to_lowercase();
        self.snapshot
            .iter()
            .flat_map(|snapshot| &snapshot.issues)
            .filter(move |issue| {
                self.status_filter
                    .as_ref()
                    .is_none_or(|status| issue.state.name == *status)
                    && (query.is_empty()
                        || issue.title.to_lowercase().contains(&query)
                        || issue.identifier.to_lowercase().contains(&query))
            })
    }

    pub fn statuses(&self) -> Vec<String> {
        self.snapshot
            .iter()
            .flat_map(|snapshot| &snapshot.issues)
            .map(|issue| issue.state.name.clone())
            .chain(self.status_filter.iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn filter_status(&mut self, status: Option<String>) {
        let selected = self.issue().map(|issue| issue.id.clone());
        self.status_filter = status;
        self.restore_selection(selected.as_deref());
    }

    pub fn search(&mut self, query: String) {
        let selected = self.issue().map(|issue| issue.id.clone());
        self.search_query = query;
        self.restore_selection(selected.as_deref());
    }

    pub fn update_issue(&mut self, updated: Issue) {
        let selected = self.issue().map(|issue| issue.id.clone());
        if let Some(issue) = self.snapshot.as_mut().and_then(|snapshot| {
            snapshot
                .issues
                .iter_mut()
                .find(|issue| issue.id == updated.id)
        }) {
            *issue = updated;
        }
        self.restore_selection(selected.as_deref());
    }

    fn restore_selection(&mut self, id: Option<&str>) {
        let selected = self
            .issues()
            .position(|issue| Some(issue.id.as_str()) == id)
            .unwrap_or(0);
        self.selected = selected;
    }

    pub fn busy(&self) -> bool {
        self.busy_since.is_some()
    }

    /// Elapsed time in the current operation, or `None` when idle.
    pub fn busy_elapsed(&self) -> Option<std::time::Duration> {
        self.busy_since.map(|started| started.elapsed())
    }

    pub fn select(&mut self, step: isize) {
        let count = self.issues().count();
        if count > 0 {
            self.selected = self.selected.saturating_add_signed(step).min(count - 1);
        }
    }

    pub fn start(&mut self, connect: bool) {
        if self.busy() || self.picker.is_some() {
            return;
        }
        let Some(config) = self.config.clone() else {
            self.status = "Set linear.api_key in config".into();
            return;
        };
        self.status = if connect {
            "Linear: open browser sign-in"
        } else {
            "Linear: refresh issues"
        }
        .into();
        let tracked = self.tracked.clone();
        self.spawn_worker(move |store, cancel, sender| {
            let progress = |progress| {
                let status = match progress {
                    Progress::Status(message) => message,
                    Progress::Browser(url) => match crate::browser::open(&url).status() {
                        Ok(exit) if exit.success() => {
                            "Complete Linear sign-in in your browser".into()
                        }
                        _ => format!("Open this sign-in URL in your browser: {url}"),
                    },
                };
                let _ = sender.send(Update::Status(status));
            };
            let result = blocking(::linear::sync(
                &config, &store, connect, &tracked, &cancel, progress,
            ));
            let _ = sender.send(Update::Done(result));
        });
    }

    pub fn edit(&mut self, id: String, operation: ::linear::EditOperation) -> bool {
        if self.busy() || self.picker.is_some() {
            return false;
        }
        let Some(config) = self.config.clone() else {
            return false;
        };
        self.editing_busy = true;
        self.spawn_worker(move |store, cancel, sender| {
            let result = blocking(::linear::edit(&config, &store, &id, operation, &cancel));
            let _ = sender.send(Update::Edited(id, Box::new(result)));
        });
        true
    }

    fn stop_worker(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            // Cancellation can overlap a database read. Never block the UI on it.
            self.retired_workers.push(worker);
        }
        self.receiver = None;
        self.busy_since = None;
        self.editing_busy = false;
    }

    pub fn open_picker(&mut self) {
        if self.editing_busy {
            // Finish the current operation before changing its workspace, but
            // retain the user's request even during an automatic comment load.
            self.picker_pending = true;
            return;
        }
        self.picker_pending = false;
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
        self.spawn_worker(move |_store, cancel, sender| {
            let result = blocking(::linear::api_key_workspace(&config, &cancel));
            let _ = sender.send(Update::Discovered(result));
        });
    }

    /// Run one Linear operation off the UI thread. Any worker still running is
    /// joined first, so only the newest one can report into the fresh channel.
    fn spawn_worker(
        &mut self,
        operation: impl FnOnce(Store, Arc<AtomicBool>, Sender<Update>) + Send + 'static,
    ) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        let store = self.store.clone();
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        self.busy_since = Some(Instant::now());
        let span = tracing::Span::current();
        self.worker = Some(thread::spawn(move || {
            let _entered = span.enter();
            operation(store, cancel, sender);
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
        if self.busy() {
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
        self.spawn_worker(move |store, _cancel, sender| {
            let result = blocking(async {
                store.bind_workspace(&workspace, true).await?;
                Ok(workspace)
            });
            let _ = sender.send(Update::Mapped(result));
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
        // Take the channel so the loop can mutate `self`; nothing inside replaces
        // it, because a refresh is deferred until after the drain.
        let Some(receiver) = self.receiver.take() else {
            return false;
        };
        let mut refresh = false;
        while let Ok(update) = receiver.try_recv() {
            changed = true;
            match update {
                Update::Edited(id, result) => {
                    self.busy_since = None;
                    self.editing_busy = false;
                    self.edited.push((id, *result));
                }
                Update::Status(message) => self.status = message,
                Update::Discovered(result) => {
                    self.busy_since = None;
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
                    self.busy_since = None;
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
                    self.busy_since = None;
                    match result {
                        Ok(result) => {
                            self.workspace = Some(result.workspace.clone());
                            self.closed = result.closed;
                            let snapshot = result.snapshot;
                            let selected = self.issue().map(|issue| issue.id.clone());
                            self.status = format!(
                                "Linear: {} — {} assigned issues",
                                result.workspace.name,
                                snapshot.issues.len()
                            );
                            self.snapshot = Some(snapshot);
                            self.restore_selection(selected.as_deref());
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
        // The channel belongs to its worker: put it back only while that worker is
        // still the current one and nothing has installed a replacement channel.
        if self.worker.is_some() && self.receiver.is_none() {
            self.receiver = Some(receiver);
        }
        if self.picker_pending && !self.editing_busy {
            self.open_picker();
        } else if refresh {
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
    async fn combined_filters_preserve_selection_across_refresh_and_issue_edits() {
        let mut snapshot = Snapshot {
            viewer: "Test".into(),
            issues: (1..=3)
                .map(|id| Issue {
                    id: id.to_string(),
                    identifier: format!("APP-{id}"),
                    title: format!("Ticket {id}"),
                    description: None,
                    url: String::new(),
                    branch_name: None,
                    state: ::linear::Named {
                        name: if id == 1 { "Todo" } else { "Started" }.into(),
                    },
                    team: ::linear::Named { name: "App".into() },
                })
                .collect(),
        };
        let mut state = Linear::new(
            None,
            vec![],
            Store::new(sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap()),
            Some(snapshot.clone()),
            None,
        );
        state.search("  tIcKeT  ".into());
        state.select(1);
        state.filter_status(Some("Started".into()));
        assert_eq!(state.issues().count(), 2);
        assert_eq!(state.statuses(), ["Started", "Todo"]);
        assert_eq!(state.issue().unwrap().identifier, "APP-2");
        snapshot.issues.reverse();
        let (sender, receiver) = mpsc::channel();
        state.receiver = Some(receiver);
        sender
            .send(Update::Done(Ok(SyncResult {
                snapshot,
                closed: vec![],
                workspace: ::linear::Workspace {
                    id: "test".into(),
                    name: "Test".into(),
                },
            })))
            .unwrap();
        state.poll();
        assert_eq!(state.issue().unwrap().identifier, "APP-2");
        let mut updated = state.issue().unwrap().clone();
        updated.title = "Renamed".into();
        state.update_issue(updated);
        assert_eq!(state.issue().unwrap().identifier, "APP-3");
        state.search("app-2".into());
        assert_eq!(state.issue().unwrap().title, "Renamed");
        let mut updated = state.issue().unwrap().clone();
        updated.state.name = "Todo".into();
        state.update_issue(updated);
        assert!(state.issue().is_none());
        state.filter_status(None);
        assert_eq!(state.issue().unwrap().identifier, "APP-2");
        state.search("missing".into());
        state.select(1);
        assert!(state.issue().is_none());
        state.search(String::new());
        assert_eq!(state.issues().count(), 3);
    }

    #[tokio::test]
    async fn workspace_picker_waits_for_edit_completion_without_losing_the_request() {
        let mut state = Linear::new(
            None,
            vec![],
            Store::new(sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap()),
            None,
            None,
        );
        let (sender, receiver) = mpsc::channel();
        state.receiver = Some(receiver);
        state.busy_since = Some(Instant::now());
        state.editing_busy = true;
        state.open_picker();
        assert!(state.picker.is_none());
        assert!(state.busy());
        assert!(!state.cancel.load(Ordering::Relaxed));
        sender
            .send(Update::Edited(
                "ticket".into(),
                Box::new(Err("Failed".into())),
            ))
            .unwrap();
        assert!(state.poll());
        assert!(state.picker.is_some());
        assert!(!state.busy());
        assert!(!state.picker_pending);
        assert_eq!(state.edited.len(), 1);
    }

    #[tokio::test]
    async fn comment_load_completion_and_failure_clear_busy_without_restarting() {
        let mut state = Linear::new(
            None,
            vec![],
            Store::new(sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap()),
            None,
            None,
        );
        for result in [
            Ok(::linear::EditResult {
                details: None,
                submitted: false,
                warning: None,
            }),
            Err("Linear request timed out".into()),
        ] {
            let (sender, receiver) = mpsc::channel();
            state.receiver = Some(receiver);
            state.busy_since = Some(Instant::now());
            state.editing_busy = true;
            sender
                .send(Update::Edited("ticket".into(), Box::new(result)))
                .unwrap();
            assert!(state.poll());
            assert!(!state.busy());
            assert!(!state.editing_busy);
            assert_eq!(state.edited.len(), 1);
            state.edited.clear();
            assert!(!state.poll());
            assert!(!state.busy());
        }
    }
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
        state.busy_since = Some(Instant::now());
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
        while state.busy() {
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
        state.busy_since = Some(Instant::now());
        sender
            .send(Update::Discovered(Err(
                "Cannot identify the Linear workspace".into(),
            )))
            .unwrap();
        state.poll();
        assert!(!state.busy());
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
        state.busy_since = Some(Instant::now());
        sender.send(Update::Done(Err("Failed".into()))).unwrap();
        assert!(state.poll());
        assert!(!state.busy());
        assert_eq!(state.snapshot.as_ref().unwrap().viewer, "Cached");
        assert!(state.status.contains("Cached issues remain available"));
    }
}
