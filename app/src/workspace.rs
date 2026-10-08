use std::{
    fs,
    os::unix::{ffi::OsStrExt, fs::DirBuilderExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use tokio::runtime::Runtime;

pub struct Workspaces {
    pub source: PathBuf,
    root: PathBuf,
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

impl Workspaces {
    pub fn open(source: &Path, state: &Path) -> Result<Self> {
        let source = PathBuf::from(jj(source, &["workspace", "root"])?).canonicalize()?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(state)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let options = SqliteConnectOptions::new()
            .filename(state.join("mentat.db"))
            .create_if_missing(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = runtime.block_on(
            SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(options),
        )?;
        runtime.block_on(sqlx::migrate!("./migrations").run(&pool))?;
        let repository_hash = format!("{:x}", Sha256::digest(source.as_os_str().as_bytes()));
        let root = state.join("workspaces").join(repository_hash);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)?;
        Ok(Self {
            source,
            root,
            pool,
            runtime,
        })
    }

    pub fn list(&self) -> Result<Vec<Workspace>> {
        let rows = self.runtime.block_on(
            sqlx::query(
                "SELECT ticket_id, path, name, cleanup FROM workspaces WHERE repository = ?",
            )
            .bind(self.source.to_string_lossy().as_ref())
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

    pub fn ensure(&self, ticket_id: &str) -> Result<Workspace> {
        let hash = format!("{:x}", Sha256::digest(ticket_id.as_bytes()));
        let path = self.root.join(&hash);
        let name = format!("mentat-{}", &hash[..20]);
        self.runtime.block_on(sqlx::query("INSERT INTO workspaces(repository, ticket_id, path, name) VALUES (?, ?, ?, ?) ON CONFLICT(repository, ticket_id) DO NOTHING")
            .bind(self.source.to_string_lossy().as_ref()).bind(ticket_id).bind(path.to_string_lossy().as_ref()).bind(&name).execute(&self.pool))?;
        let record = self
            .list()?
            .into_iter()
            .find(|w| w.ticket_id == ticket_id)
            .context("Missing workspace record")?;
        ensure!(
            !record.cleanup,
            "This ticket is closed; its workspace awaits cleanup"
        );
        if !record.path.try_exists()? {
            let destination = record
                .path
                .to_str()
                .context("Workspace path must be UTF-8")?;
            jj(
                &self.source,
                &[
                    "workspace",
                    "add",
                    "--name",
                    &record.name,
                    "-r",
                    "@",
                    destination,
                ],
            )?;
        }
        self.validate(&record)?;
        Ok(record)
    }

    pub fn lock(&self, record: &Workspace) -> Result<fs::File> {
        let path = self.root.join(format!("{}.lock", record.name));
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.try_lock()
            .context("Another Mentat session uses this ticket workspace")?;
        Ok(file)
    }

    fn validate(&self, record: &Workspace) -> Result<()> {
        ensure!(
            record.path.parent() == Some(self.root.as_path()),
            "Workspace path is outside the managed directory"
        );
        ensure!(
            !fs::symlink_metadata(&record.path)?.file_type().is_symlink(),
            "Workspace path is a symbolic link"
        );
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
            .bind(self.source.to_string_lossy().as_ref()).bind(ticket_id).bind(agent).fetch_optional(&self.pool))?;
        row.map(|r| r.try_get("session_id"))
            .transpose()
            .map_err(Into::into)
    }

    pub fn save_conversation(&self, ticket_id: &str, agent: &str, id: &str) -> Result<()> {
        self.runtime.block_on(sqlx::query("INSERT INTO agent_sessions(repository, ticket_id, agent, session_id) VALUES (?, ?, ?, ?) ON CONFLICT(repository, ticket_id, agent) DO UPDATE SET session_id = excluded.session_id")
            .bind(self.source.to_string_lossy().as_ref()).bind(ticket_id).bind(agent).bind(id).execute(&self.pool))?;
        Ok(())
    }

    pub fn select(&self, ticket_id: &str) -> Result<()> {
        self.runtime.block_on(sqlx::query("INSERT INTO app_state(repository, selected_ticket) VALUES (?, ?) ON CONFLICT(repository) DO UPDATE SET selected_ticket = excluded.selected_ticket")
            .bind(self.source.to_string_lossy().as_ref()).bind(ticket_id).execute(&self.pool))?;
        Ok(())
    }

    pub fn selected(&self) -> Result<Option<String>> {
        let row = self.runtime.block_on(
            sqlx::query("SELECT selected_ticket FROM app_state WHERE repository = ?")
                .bind(self.source.to_string_lossy().as_ref())
                .fetch_optional(&self.pool),
        )?;
        row.map(|r| r.try_get("selected_ticket"))
            .transpose()
            .map_err(Into::into)
    }

    pub fn mark_closed(&self, ids: &[String]) -> Result<()> {
        for id in ids {
            self.runtime.block_on(
                sqlx::query(
                    "UPDATE workspaces SET cleanup = 1 WHERE repository = ? AND ticket_id = ?",
                )
                .bind(self.source.to_string_lossy().as_ref())
                .bind(id)
                .execute(&self.pool),
            )?;
        }
        Ok(())
    }

    /// Caller first checks that no process still uses this workspace.
    pub fn cleanup(&self, record: &Workspace) -> Result<bool> {
        ensure!(record.cleanup, "Workspace cleanup requires a closed ticket");
        let _lock = self.lock(record)?;
        if record.path.try_exists()? {
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
                .bind(self.source.to_string_lossy().as_ref())
                .bind(&record.ticket_id)
                .execute(&self.pool),
        )?;
        Ok(true)
    }
}

fn jj(directory: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("jj")
        .current_dir(directory)
        .args(["--no-pager", "--color=never"])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("Cannot run jj; install Jujutsu and start Mentat in a jj repository")?;
    ensure!(
        output.status.success(),
        "Jujutsu failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_workspace_selection_and_provider_sessions_then_cleans_closed_tickets() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        jj(
            temp.path(),
            &["git", "init", "--no-colocate", repo.to_str().unwrap()],
        )
        .unwrap();
        let state = temp.path().join("state");
        let store = Workspaces::open(&repo, &state).unwrap();
        let first = store.ensure("one").unwrap();
        let second = store.ensure("two").unwrap();
        assert_ne!(first.path, second.path);
        assert_eq!(store.ensure("one").unwrap().path, first.path);
        store.save_conversation("one", "codex", "codex-id").unwrap();
        store
            .save_conversation("one", "claude", "claude-id")
            .unwrap();
        store.select("one").unwrap();
        drop(store);
        let store = Workspaces::open(&repo, &state).unwrap();
        assert_eq!(store.selected().unwrap().as_deref(), Some("one"));
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
        assert!(store.cleanup(&first).unwrap());
        assert!(!first.path.exists());
        assert!(second.path.exists());
        assert!(store.conversation("one", "codex").unwrap().is_none());
    }
}
