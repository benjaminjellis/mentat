use std::{
    fs,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use crate::settings::WorkspaceKind;
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use sqlx::{
    ConnectOptions, Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use tokio::runtime::Runtime;

pub struct Workspaces {
    pub source: PathBuf,
    kind: WorkspaceKind,
    repository: String,
    root: PathBuf,
    locks: PathBuf,
    pool: SqlitePool,
    runtime: Runtime,
}

#[derive(Clone)]
pub struct Workspace {
    pub ticket_id: String,
    pub path: PathBuf,
    pub name: String,
    pub cleanup: bool,
}

pub struct WorkspaceLock(fs::File);

impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        // Closing alone can leave the lock held by a concurrently forked child
        // until it execs. Release it explicitly when the owner is finished.
        let _ = self.0.unlock();
    }
}

impl Workspaces {
    pub fn open(
        source: &Path,
        state: &Path,
        workspace_root: Option<&Path>,
        kind: WorkspaceKind,
    ) -> Result<Self> {
        let source = PathBuf::from(match kind {
            WorkspaceKind::Jj => jj(source, &["workspace", "root"]),
            WorkspaceKind::Git => git(source, &["rev-parse", "--show-toplevel"]),
        }?)
        .canonicalize()?;
        // Keep existing JJ state compatible and isolate Git state in colocated repositories.
        let repository = match kind {
            WorkspaceKind::Jj => source.to_string_lossy().into_owned(),
            WorkspaceKind::Git => format!("git:{}", source.display()),
        };
        let database = prepare_database(state)?;
        // SQLx returns connections to the pool in spawned tasks. Keep those
        // tasks running between UI calls, including while Linear workers query.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        let options = SqliteConnectOptions::new()
            .filename(database)
            .busy_timeout(Duration::from_secs(5))
            .disable_statement_logging();
        let pool = runtime.block_on(
            SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(options),
        )?;
        runtime.block_on(sqlx::migrate!("./migrations").run(&pool))?;
        let root = match workspace_root {
            // Resolve relative roots against the repository, even when launched in a subdirectory.
            Some(configured) => source.join(configured),
            // A root of its own per repository keeps ticket directories named after the ticket.
            None => {
                let repository_name = path_component(
                    source
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("repo"),
                );
                source
                    .parent()
                    .context("Repository has no parent directory")?
                    .join(format!("{repository_name}-workspaces"))
            }
        };
        // Preserve the lock location for workspaces created by older versions.
        let locks = state.join("workspaces");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&locks)?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)?;
        Ok(Self {
            source,
            kind,
            repository,
            root,
            locks,
            pool,
            runtime,
        })
    }

    pub fn linear_state(
        &self,
        legacy_directory: &Path,
        config: Option<&linear::Config>,
    ) -> Result<(
        linear::Store,
        Option<linear::Snapshot>,
        Option<linear::Workspace>,
    )> {
        let store = linear::Store::for_repository(
            self.pool.clone(),
            self.source.to_string_lossy().into_owned(),
        );
        self.runtime
            .block_on(store.import_legacy(legacy_directory))?;
        let snapshot = config
            .map(|config| self.runtime.block_on(store.cached(config)))
            .transpose()?
            .flatten();
        let workspace = self.runtime.block_on(store.workspace())?;
        Ok((store, snapshot, workspace))
    }

    pub fn list(&self) -> Result<Vec<Workspace>> {
        let rows = self.runtime.block_on(
            sqlx::query(
                "SELECT ticket_id, path, name, cleanup FROM workspaces WHERE repository = ?",
            )
            .bind(&self.repository)
            .fetch_all(&self.pool),
        )?;
        rows.into_iter()
            .map(|row| {
                Ok(Workspace {
                    ticket_id: row.try_get("ticket_id")?,
                    path: PathBuf::from(row.try_get::<String, _>("path")?),
                    name: row.try_get("name")?,
                    cleanup: row.try_get("cleanup")?,
                })
            })
            .collect()
    }

    pub fn ensure(&self, ticket_id: &str, identifier: &str) -> Result<Workspace> {
        let started = std::time::Instant::now();
        tracing::debug!(ticket_id, backend = ?self.kind, "Opening ticket workspace");
        let ticket_name = path_component(identifier);
        // Workspace names and branches live in a namespace per repository, so the
        // ticket alone names them; only the directory can clash with another repository.
        let name = format!("mentat-{ticket_name}");
        let path = self.claim(&ticket_name)?;
        self.runtime.block_on(sqlx::query("INSERT INTO workspaces(repository, ticket_id, path, name) VALUES (?, ?, ?, ?) ON CONFLICT(repository, ticket_id) DO NOTHING")
            .bind(&self.repository).bind(ticket_id).bind(path.to_string_lossy().as_ref()).bind(&name).execute(&self.pool))?;
        let record = self
            .list()?
            .into_iter()
            .find(|w| w.ticket_id == ticket_id)
            .context("Missing workspace record")?;
        ensure!(
            !record.cleanup,
            "This ticket is closed; its workspace awaits cleanup"
        );
        self.validate_record(&record)?;
        if !record.path.try_exists()? {
            fs::create_dir_all(record.path.parent().context("Workspace has no parent")?)?;
            let destination = record
                .path
                .to_str()
                .context("Workspace path must be UTF-8")?;
            match self.kind {
                WorkspaceKind::Jj => {
                    jj(
                        &self.source,
                        &[
                            "workspace",
                            "add",
                            "--name",
                            &record.name,
                            "-r",
                            "main",
                            destination,
                        ],
                    )?;
                }
                WorkspaceKind::Git => {
                    let branches = git(
                        &self.source,
                        &[
                            "branch",
                            "--list",
                            &record.name,
                            "--format=%(refname:short)",
                        ],
                    )?;
                    if branches.trim() == record.name {
                        git(
                            &self.source,
                            &["worktree", "add", destination, &record.name],
                        )?;
                    } else {
                        git(
                            &self.source,
                            &["worktree", "add", "-b", &record.name, destination, "HEAD"],
                        )?;
                    }
                }
            }
        }
        self.validate(&record)?;
        tracing::info!(
            ticket_id,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "Ticket workspace ready"
        );
        Ok(record)
    }

    /// A shared workspace root, or the two backends of a colocated repository, can
    /// want one ticket directory twice. Fall back to a digest of the repository only
    /// then; the stored record keeps whichever directory a workspace already uses.
    fn claim(&self, ticket_name: &str) -> Result<PathBuf> {
        let path = self.root.join(ticket_name);
        let taken = self.runtime.block_on(
            sqlx::query("SELECT 1 FROM workspaces WHERE path = ? AND repository <> ? LIMIT 1")
                .bind(path.to_string_lossy().as_ref())
                .bind(&self.repository)
                .fetch_optional(&self.pool),
        )?;
        if taken.is_none() && !path.try_exists()? {
            return Ok(path);
        }
        let digest = Sha256::digest(self.repository.as_bytes());
        let repository_id = format!("{digest:x}");
        Ok(self
            .root
            .join(format!("{ticket_name}-{}", &repository_id[..8])))
    }

    pub fn lock(&self, record: &Workspace) -> Result<WorkspaceLock> {
        let path = self.locks.join(format!("{}.lock", record.name));
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.try_lock()
            .context("Another Mentat session uses this ticket workspace")?;
        Ok(WorkspaceLock(file))
    }

    fn validate_record(&self, record: &Workspace) -> Result<()> {
        ensure!(
            record.path.is_absolute()
                && record.path.parent().is_some()
                && record.path != self.source,
            "Invalid managed workspace path"
        );
        ensure!(
            self.list()?
                .iter()
                .any(|stored| stored.ticket_id == record.ticket_id
                    && stored.path == record.path
                    && stored.name == record.name
                    && stored.cleanup == record.cleanup),
            "Workspace does not match its stored record"
        );
        Ok(())
    }

    fn validate(&self, record: &Workspace) -> Result<()> {
        self.validate_record(record)?;
        ensure!(
            !fs::symlink_metadata(&record.path)?.file_type().is_symlink(),
            "Workspace path is a symbolic link"
        );
        if self.kind == WorkspaceKind::Git {
            let common = |path: &Path| -> Result<PathBuf> {
                Ok(PathBuf::from(git(
                    path,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                )?)
                .canonicalize()?)
            };
            ensure!(
                common(&record.path)? == common(&self.source)?,
                "Git worktree belongs to another repository"
            );
            let actual = PathBuf::from(git(&record.path, &["rev-parse", "--show-toplevel"])?);
            ensure!(
                actual.canonicalize()? == record.path.canonicalize()?,
                "Invalid Git worktree directory"
            );
            ensure!(
                git(&record.path, &["symbolic-ref", "--short", "HEAD"])? == record.name,
                "Git worktree branch does not match the database"
            );
            return Ok(());
        }
        let registered = PathBuf::from(jj(
            &self.source,
            &["workspace", "root", "--name", &record.name],
        )?);
        ensure!(
            registered.canonicalize()? == record.path.canonicalize()?,
            "Jujutsu workspace path does not match the database"
        );
        let actual = PathBuf::from(jj(&record.path, &["workspace", "root"])?);
        ensure!(
            actual.canonicalize()? == record.path.canonicalize()?,
            "Invalid workspace directory"
        );
        Ok(())
    }

    pub fn conversation(&self, ticket_id: &str, agent: &str) -> Result<Option<String>> {
        let row = self.runtime.block_on(sqlx::query("SELECT session_id FROM agent_sessions WHERE repository = ? AND ticket_id = ? AND agent = ?")
            .bind(&self.repository).bind(ticket_id).bind(agent).fetch_optional(&self.pool))?;
        row.map(|r| r.try_get("session_id"))
            .transpose()
            .map_err(Into::into)
    }

    pub fn save_conversation(&self, ticket_id: &str, agent: &str, id: &str) -> Result<()> {
        self.runtime.block_on(sqlx::query("INSERT INTO agent_sessions(repository, ticket_id, agent, session_id) VALUES (?, ?, ?, ?) ON CONFLICT(repository, ticket_id, agent) DO UPDATE SET session_id = excluded.session_id")
            .bind(&self.repository).bind(ticket_id).bind(agent).bind(id).execute(&self.pool))?;
        Ok(())
    }

    pub fn mark_closed(&self, ids: &[String]) -> Result<()> {
        for id in ids {
            self.runtime.block_on(
                sqlx::query(
                    "UPDATE workspaces SET cleanup = 1 WHERE repository = ? AND ticket_id = ?",
                )
                .bind(&self.repository)
                .bind(id)
                .execute(&self.pool),
            )?;
        }
        Ok(())
    }

    /// Caller first checks that no process still uses this workspace.
    pub fn cleanup(&self, record: &Workspace) -> Result<bool> {
        ensure!(record.cleanup, "Workspace cleanup requires a closed ticket");
        self.validate_record(record)?;
        let _lock = self.lock(record)?;
        if self.kind == WorkspaceKind::Git {
            if record.path.try_exists()? {
                self.validate(record)?;
                if !git(
                    &record.path,
                    &[
                        "status",
                        "--porcelain",
                        "--untracked-files=all",
                        "--ignored",
                    ],
                )?
                .is_empty()
                {
                    return Ok(false);
                }
                git(
                    &self.source,
                    &[
                        "worktree",
                        "remove",
                        record
                            .path
                            .to_str()
                            .context("Workspace path must be UTF-8")?,
                    ],
                )?;
            } else {
                let registered = git(&self.source, &["worktree", "list", "--porcelain", "-z"])?;
                if registered.split('\0').any(|field| {
                    field
                        .strip_prefix("worktree ")
                        .is_some_and(|path| Path::new(path) == record.path)
                }) {
                    git(
                        &self.source,
                        &[
                            "worktree",
                            "remove",
                            "--force",
                            record
                                .path
                                .to_str()
                                .context("Workspace path must be UTF-8")?,
                        ],
                    )?;
                }
            }
            // Keep the branch so committed ticket work remains reachable after cleanup.
        } else if record.path.try_exists()? {
            self.validate(record)?;
            if !jj(&record.path, &["diff", "--summary"])?.is_empty() {
                return Ok(false);
            }
            // Remove only the validated managed path; a retry can forget a missing directory.
            fs::remove_dir_all(&record.path).context("Cannot remove closed ticket workspace")?;
            jj(&self.source, &["workspace", "forget", &record.name])?;
        } else {
            let names = jj(
                &self.source,
                &["workspace", "list", "-T", "name ++ \"\\n\""],
            )?;
            if names.lines().any(|name| name == record.name) {
                jj(&self.source, &["workspace", "forget", &record.name])?;
            }
        }
        self.runtime.block_on(
            sqlx::query("DELETE FROM workspaces WHERE repository = ? AND ticket_id = ?")
                .bind(&self.repository)
                .bind(&record.ticket_id)
                .execute(&self.pool),
        )?;
        tracing::info!(ticket_id = %record.ticket_id, "Closed ticket workspace removed");
        Ok(true)
    }
}

fn prepare_database(state: &Path) -> Result<PathBuf> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state)?;
    ensure!(
        fs::symlink_metadata(state)?.is_dir(),
        "State directory must be a real directory"
    );
    fs::set_permissions(state, fs::Permissions::from_mode(0o700))?;
    let path = state.join("mentat.db");
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .context("Cannot open private state database; symlinks are not allowed")?;
    ensure!(
        file.metadata()?.is_file(),
        "State database must be a regular file"
    );
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(path)
}

fn path_component(value: &str) -> String {
    let name: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(32)
        .collect();
    if name.trim_matches('-').is_empty() {
        "workspace".into()
    } else {
        name
    }
}

/// Run a version control command with its pager and colour disabled. Both
/// backends are driven the same way; only their names and advice differ.
fn vcs(tool: &Vcs, directory: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new(tool.program)
        .current_dir(directory)
        .args(tool.globals)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context(tool.unavailable)?;
    ensure!(
        output.status.success(),
        "{} failed: {}",
        tool.label,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

struct Vcs {
    program: &'static str,
    globals: &'static [&'static str],
    label: &'static str,
    unavailable: &'static str,
}

const GIT: Vcs = Vcs {
    program: "git",
    globals: &["--no-pager", "-c", "color.ui=false"],
    label: "Git",
    unavailable: "Cannot run git; install Git and start Mentat in a Git repository",
};

const JJ: Vcs = Vcs {
    program: "jj",
    globals: &["--no-pager", "--color=never"],
    label: "Jujutsu",
    unavailable: "Cannot run jj; install Jujutsu and start Mentat in a jj repository",
};

fn git(directory: &Path, args: &[&str]) -> Result<String> {
    vcs(&GIT, directory, args)
}

fn jj(directory: &Path, args: &[&str]) -> Result<String> {
    vcs(&JJ, directory, args)
}

pub fn create_bookmark(directory: &Path, name: &str) -> Result<()> {
    jj(
        directory,
        &["bookmark", "set", "--revision", "@", "--", name],
    )?;
    jj(
        directory,
        &["bookmark", "track", "--remote=origin", "--", name],
    )
    .context("Bookmark set at @, but tracking on origin failed")?;
    Ok(())
}

pub struct Push {
    child: std::process::Child,
    output: fs::File,
}

impl Push {
    pub fn start(directory: &Path) -> Result<Self> {
        let output = tempfile::tempfile()?;
        let child = Command::new(JJ.program)
            .current_dir(directory)
            .args(JJ.globals)
            .args(["git", "push"])
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output.try_clone()?)
            .spawn()
            .context(JJ.unavailable)?;
        Ok(Self { child, output })
    }

    pub fn poll(&mut self) -> Result<Option<String>> {
        use std::io::{Read, Seek};

        let Some(status) = self.child.try_wait()? else {
            return Ok(None);
        };
        self.output.rewind()?;
        let mut output = String::new();
        self.output.read_to_string(&mut output)?;
        ensure!(status.success(), "jj git push failed:\n{}", output.trim());
        Ok(Some(format!("jj git push completed.\n{}", output.trim())))
    }
}

impl Drop for Push {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_bookmark_targets_ticket_workspace_and_tracks_origin() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        jj(
            temp.path(),
            &["git", "init", "--no-colocate", repo.to_str().unwrap()],
        )
        .unwrap();
        jj(&repo, &["bookmark", "create", "main"]).unwrap();
        jj(
            &repo,
            &[
                "git",
                "remote",
                "add",
                "origin",
                temp.path().join("remote").to_str().unwrap(),
            ],
        )
        .unwrap();
        let workspaces = Workspaces::open(
            &repo,
            &temp.path().join("state"),
            Some(&temp.path().join("checkouts")),
            WorkspaceKind::Jj,
        )
        .unwrap();
        let ticket = workspaces.ensure("one", "ENG-1").unwrap();
        let name = "ben/eng-1-ticket";
        create_bookmark(&ticket.path, name).unwrap();
        let target = jj(
            &ticket.path,
            &["log", "--no-graph", "-r", name, "-T", "commit_id"],
        )
        .unwrap();
        assert_eq!(
            target,
            jj(
                &ticket.path,
                &["log", "--no-graph", "-r", "@", "-T", "commit_id"]
            )
            .unwrap()
        );
        assert_ne!(
            target,
            jj(&repo, &["log", "--no-graph", "-r", "@", "-T", "commit_id"]).unwrap()
        );
        jj(&ticket.path, &["new"]).unwrap();
        create_bookmark(&ticket.path, name).unwrap();
        assert_eq!(
            jj(
                &ticket.path,
                &["log", "--no-graph", "-r", "@", "-T", "commit_id"]
            )
            .unwrap(),
            jj(
                &ticket.path,
                &["log", "--no-graph", "-r", name, "-T", "commit_id"]
            )
            .unwrap()
        );
        assert!(
            jj(&ticket.path, &["bookmark", "list", "--tracked"])
                .unwrap()
                .contains("@origin")
        );
    }

    #[test]
    fn workspace_roots_sit_next_to_the_repository_when_started_in_subdirectory() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        git(temp.path(), &["init", repo.to_str().unwrap()]).unwrap();
        let nested = repo.join("nested");
        fs::create_dir(&nested).unwrap();
        let state = temp.path().join("state");
        let default = crate::settings::WorkspaceSettings::default()
            .directory()
            .unwrap();
        assert_eq!(default, None);
        let workspaces =
            Workspaces::open(&nested, &state, default.as_deref(), WorkspaceKind::Git).unwrap();
        assert_eq!(
            workspaces.root.canonicalize().unwrap(),
            temp.path().join("repo-workspaces").canonicalize().unwrap()
        );
        // A configured relative root follows the repository, not the current directory.
        let workspaces = Workspaces::open(
            &nested,
            &state,
            Some(Path::new("../checkouts")),
            WorkspaceKind::Git,
        )
        .unwrap();
        assert_eq!(
            workspaces.root.canonicalize().unwrap(),
            temp.path().join("checkouts").canonicalize().unwrap()
        );
    }

    #[test]
    fn linear_database_is_available_while_the_ui_runtime_is_idle() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        jj(
            temp.path(),
            &["git", "init", "--no-colocate", repo.to_str().unwrap()],
        )
        .unwrap();
        jj(&repo, &["bookmark", "create", "main"]).unwrap();
        let checkouts = temp.path().join("checkouts");
        let workspaces = Workspaces::open(
            &repo,
            &temp.path().join("state"),
            Some(&checkouts),
            WorkspaceKind::Jj,
        )
        .unwrap();
        let (store, _, _) = workspaces.linear_state(temp.path(), None).unwrap();
        // No more block_on calls on the UI thread: a worker must still acquire
        // the sole pooled connection after the startup query returns it.
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(1), store.workspace()).await
                    .expect("Linear is waiting for the idle UI runtime to return its database connection")
                    .unwrap();
            });
        }).join().unwrap();
    }

    #[test]
    fn database_permissions_are_private_and_symlinks_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        let path = prepare_database(&state).unwrap();
        assert_eq!(
            fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        prepare_database(&state).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(&path).unwrap();
        let target = temp.path().join("other");
        fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(prepare_database(&state).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "keep");
    }

    #[test]
    fn workspace_components_are_short_and_cannot_escape_the_root() {
        assert_eq!(path_component("FOR-805"), "FOR-805");
        assert_eq!(path_component("../../bad/name"), "------bad-name");
        assert_eq!(path_component(""), "workspace");
        assert_eq!(path_component(&"x".repeat(100)).len(), 32);
    }

    #[test]
    fn colocated_backends_keep_separate_workspaces_and_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        git(temp.path(), &["init", repo.to_str().unwrap()]).unwrap();
        git(
            &repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "Initial",
            ],
        )
        .unwrap();
        jj(&repo, &["git", "init", "--colocate"]).unwrap();
        jj(&repo, &["bookmark", "set", "main", "-r", "@"]).unwrap();
        let state = temp.path().join("state");
        let checkouts = temp.path().join("checkouts");
        let jj_store =
            Workspaces::open(&repo, &state, Some(&checkouts), WorkspaceKind::Jj).unwrap();
        let git_store =
            Workspaces::open(&repo, &state, Some(&checkouts), WorkspaceKind::Git).unwrap();
        let jj_workspace = jj_store.ensure("one", "TEST-1").unwrap();
        jj_store
            .save_conversation("one", "codex", "jj-session")
            .unwrap();
        let git_workspace = git_store.ensure("one", "TEST-1").unwrap();
        assert_eq!(jj_workspace.path, checkouts.join("TEST-1"));
        assert_ne!(jj_workspace.path, git_workspace.path);
        assert_eq!(git_workspace.path.parent().unwrap(), checkouts);
        assert!(git_store.conversation("one", "codex").unwrap().is_none());
        git_store.mark_closed(&["one".into()]).unwrap();
        assert!(!jj_store.list().unwrap()[0].cleanup);
        assert!(jj_store.ensure("one", "TEST-1").is_ok());
    }

    #[test]
    fn stored_paths_are_reused_and_repositories_sharing_a_root_stay_separate() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let root = temp.path().join("checkouts");
        let repo = temp.path().join("repo");
        jj(
            temp.path(),
            &["git", "init", "--no-colocate", repo.to_str().unwrap()],
        )
        .unwrap();
        jj(&repo, &["bookmark", "create", "main"]).unwrap();
        let store = Workspaces::open(&repo, &state, Some(&root), WorkspaceKind::Jj).unwrap();
        let stored = state.join("workspaces/repo-TEST-1");
        store
            .runtime
            .block_on(
                sqlx::query(
                    "INSERT INTO workspaces(repository, ticket_id, path, name) VALUES (?, ?, ?, ?)",
                )
                .bind(&store.repository)
                .bind("one")
                .bind(stored.to_str().unwrap())
                .bind("mentat-repo-TEST-1")
                .execute(&store.pool),
            )
            .unwrap();
        let record = store.ensure("one", "TEST-1").unwrap();
        assert_eq!(record.path, stored);
        assert_eq!(store.ensure("one", "TEST-1").unwrap().path, stored);
        store.mark_closed(&["one".into()]).unwrap();
        assert!(store.cleanup(&store.list().unwrap()[0]).unwrap());
        let first = store.ensure("one", "TEST-1").unwrap();
        let other = temp.path().join("other/repo");
        fs::create_dir_all(other.parent().unwrap()).unwrap();
        jj(
            temp.path(),
            &["git", "init", "--no-colocate", other.to_str().unwrap()],
        )
        .unwrap();
        jj(&other, &["bookmark", "create", "main"]).unwrap();
        let other = Workspaces::open(&other, &state, Some(&root), WorkspaceKind::Jj).unwrap();
        let second = other.ensure("one", "TEST-1").unwrap();
        assert_eq!(first.path, root.join("TEST-1"));
        assert_ne!(first.path, second.path);
        assert_eq!(second.path.parent().unwrap(), root);
        // Names only have to be unique inside one repository.
        assert_eq!(first.name, second.name);
    }

    #[test]
    fn persists_workspaces_and_provider_sessions_then_cleans_closed_tickets() {
        for kind in [WorkspaceKind::Jj, WorkspaceKind::Git] {
            workspace_lifecycle(kind);
        }
    }

    fn workspace_lifecycle(kind: WorkspaceKind) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        match kind {
            WorkspaceKind::Jj => {
                jj(
                    temp.path(),
                    &["git", "init", "--no-colocate", repo.to_str().unwrap()],
                )
                .unwrap();
                jj(&repo, &["bookmark", "create", "main"]).unwrap();
            }
            WorkspaceKind::Git => {
                git(temp.path(), &["init", repo.to_str().unwrap()]).unwrap();
                git(
                    &repo,
                    &[
                        "-c",
                        "user.name=Test",
                        "-c",
                        "user.email=test@example.com",
                        "commit",
                        "--allow-empty",
                        "-m",
                        "Initial",
                    ],
                )
                .unwrap();
            }
        }
        let state = temp.path().join("state");
        let checkouts = temp.path().join("checkouts");
        let store = Workspaces::open(&repo, &state, Some(&checkouts), kind).unwrap();
        let first = store.ensure("one", "TEST-1").unwrap();
        assert_eq!(first.path, checkouts.join("TEST-1"));
        let second = store.ensure("two", "TEST-2").unwrap();
        assert_ne!(first.path, second.path);
        assert_eq!(store.ensure("one", "TEST-1").unwrap().path, first.path);
        store.save_conversation("one", "codex", "codex-id").unwrap();
        store
            .save_conversation("one", "claude", "claude-id")
            .unwrap();
        drop(store);
        let new_checkouts = temp.path().join("new-checkouts");
        let store = Workspaces::open(&repo, &state, Some(&new_checkouts), kind).unwrap();
        assert_eq!(store.ensure("one", "TEST-1").unwrap().path, first.path);
        let third = store.ensure("three", "TEST-3").unwrap();
        assert_eq!(third.path, new_checkouts.join("TEST-3"));
        assert_eq!(
            store.conversation("one", "codex").unwrap().as_deref(),
            Some("codex-id")
        );
        assert_eq!(
            store.conversation("one", "claude").unwrap().as_deref(),
            Some("claude-id")
        );
        assert!(store.cleanup(&first).is_err());
        store.mark_closed(&["one".into()]).unwrap();
        let first = store
            .list()
            .unwrap()
            .into_iter()
            .find(|w| w.ticket_id == "one")
            .unwrap();
        let lock = store.lock(&first).unwrap();
        assert!(store.cleanup(&first).is_err());
        drop(lock);
        fs::write(first.path.join("work"), "keep me").unwrap();
        assert!(!store.cleanup(&first).unwrap());
        fs::remove_file(first.path.join("work")).unwrap();
        let commit = if kind == WorkspaceKind::Git {
            fs::write(first.path.join("result"), "committed work").unwrap();
            git(&first.path, &["add", "result"]).unwrap();
            git(
                &first.path,
                &[
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.com",
                    "commit",
                    "-m",
                    "Ticket work",
                ],
            )
            .unwrap();
            // Ignored files must also prevent automatic cleanup.
            let exclude = repo.join(".git/info/exclude");
            fs::write(exclude, "ignored\n").unwrap();
            fs::write(first.path.join("ignored"), "keep me").unwrap();
            assert!(!store.cleanup(&first).unwrap());
            fs::remove_file(first.path.join("ignored")).unwrap();
            Some(git(&first.path, &["rev-parse", "HEAD"]).unwrap())
        } else {
            None
        };
        assert!(store.cleanup(&first).unwrap());
        if let Some(commit) = commit {
            assert_eq!(git(&repo, &["rev-parse", &first.name]).unwrap(), commit);
            let reopened = store.ensure("one", "TEST-1").unwrap();
            assert!(reopened.path.join("result").is_file());
            store.mark_closed(&["one".into()]).unwrap();
            let reopened = store
                .list()
                .unwrap()
                .into_iter()
                .find(|w| w.ticket_id == "one")
                .unwrap();
            fs::remove_dir_all(&reopened.path).unwrap();
            assert!(store.cleanup(&reopened).unwrap());
        }
        assert!(!first.path.exists());
        assert!(second.path.exists());
        assert!(store.conversation("one", "codex").unwrap().is_none());
    }
}
