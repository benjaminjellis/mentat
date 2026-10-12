use crate::{Config, Snapshot, Workspace, oauth::Tokens};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
    repository: Option<String>,
}

impl Store {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            repository: None,
        }
    }

    /// Repository is the canonical source path, shared by Git and jj backends.
    pub fn for_repository(pool: SqlitePool, repository: String) -> Self {
        Self {
            pool,
            repository: Some(repository),
        }
    }

    pub async fn workspace(&self) -> Result<Option<Workspace>> {
        let Some(repository) = &self.repository else {
            return Ok(None);
        };
        let row = sqlx::query(
            "SELECT workspace_id, workspace_name FROM repository_linear WHERE repository = ?",
        )
        .bind(repository)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(Workspace {
                id: row.try_get("workspace_id")?,
                name: row.try_get("workspace_name")?,
            })
        })
        .transpose()
    }

    pub async fn require_connection(&self, connect: bool) -> Result<()> {
        ensure!(
            self.repository.is_none() || connect || self.workspace().await?.is_some(),
            "No Linear workspace mapped to this repository; press l to connect"
        );
        Ok(())
    }

    /// Bind once on explicit connection. Refresh and reconnect must match that workspace.
    pub async fn bind_workspace(&self, workspace: &Workspace, connect: bool) -> Result<()> {
        let Some(repository) = &self.repository else {
            return Ok(());
        };
        ensure!(
            !workspace.id.is_empty() && !workspace.name.is_empty(),
            "Linear returned an invalid workspace"
        );
        if connect {
            sqlx::query("INSERT INTO repository_linear(repository, workspace_id, workspace_name) VALUES (?, ?, ?) ON CONFLICT(repository) DO NOTHING")
                .bind(repository).bind(&workspace.id).bind(&workspace.name).execute(&self.pool).await?;
        }
        let saved = self
            .workspace()
            .await?
            .context("No Linear workspace mapped to this repository; press l to connect")?;
        ensure!(
            saved.id == workspace.id,
            "This repository is mapped to Linear workspace '{}', but these credentials belong to '{}'; connect with the mapped workspace",
            saved.name,
            workspace.name
        );
        sqlx::query("UPDATE repository_linear SET workspace_name = ? WHERE repository = ? AND workspace_id = ?")
            .bind(&workspace.name).bind(repository).bind(&workspace.id).execute(&self.pool).await?;
        Ok(())
    }

    async fn cache_key(&self, config: &Config) -> Result<Option<String>> {
        match &self.repository {
            None => Ok(Some(cache_identity(config))),
            Some(repository) => Ok(self.workspace().await?.map(|workspace| {
                let identity =
                    serde_json::to_vec(&(repository, workspace.id, cache_identity(config)))
                        .expect("string tuple");
                format!("repository:{:x}", Sha256::digest(identity))
            })),
        }
    }

    pub async fn cached(&self, config: &Config) -> Result<Option<Snapshot>> {
        let Some(identity) = self.cache_key(config).await? else {
            return Ok(None);
        };
        let value: Option<String> =
            sqlx::query_scalar("SELECT snapshot FROM linear_cache WHERE identity = ?")
                .bind(identity)
                .fetch_optional(&self.pool)
                .await?;
        value
            .map(|value| {
                serde_json::from_str(&value)
                    .map_err(|_| anyhow::anyhow!("Cannot read cached Linear issues"))
            })
            .transpose()
    }

    pub async fn save_cache(&self, config: &Config, snapshot: &Snapshot) -> Result<()> {
        let identity = self
            .cache_key(config)
            .await?
            .context("Connect a Linear workspace before caching issues")?;
        sqlx::query("INSERT INTO linear_cache(identity, snapshot) VALUES (?, ?) ON CONFLICT(identity) DO UPDATE SET snapshot = excluded.snapshot")
            .bind(identity).bind(serde_json::to_string(snapshot)?).execute(&self.pool).await?;
        Ok(())
    }

    pub(crate) async fn load_tokens(&self, config: &Config) -> Result<Tokens> {
        let row = if let Some(repository) = &self.repository {
            sqlx::query("SELECT access_token, refresh_token, expires_at FROM repository_linear_oauth WHERE repository = ? AND client_id = ?")
                .bind(repository).bind(&config.client_id).fetch_optional(&self.pool).await?
        } else {
            sqlx::query("SELECT access_token, refresh_token, expires_at FROM linear_oauth WHERE client_id = ?")
                .bind(&config.client_id).fetch_optional(&self.pool).await?
        }.context("Linear is not connected; press l to sign in")?;
        Ok(Tokens {
            client_id: config.client_id.clone(),
            access_token: row.try_get("access_token")?,
            refresh_token: row.try_get("refresh_token")?,
            expires_at: u64::try_from(row.try_get::<i64, _>("expires_at")?)?,
        })
    }

    pub(crate) async fn save_tokens(&self, config: &Config, tokens: &Tokens) -> Result<()> {
        ensure!(
            config.client_id == tokens.client_id,
            "Linear application changed; press l to reconnect"
        );
        if let Some(repository) = &self.repository {
            sqlx::query("INSERT INTO repository_linear_oauth(repository, client_id, access_token, refresh_token, expires_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT(repository, client_id) DO UPDATE SET access_token = excluded.access_token, refresh_token = excluded.refresh_token, expires_at = excluded.expires_at")
                .bind(repository).bind(&tokens.client_id).bind(&tokens.access_token).bind(&tokens.refresh_token)
                .bind(i64::try_from(tokens.expires_at)?).execute(&self.pool).await?;
        } else {
            sqlx::query("INSERT INTO linear_oauth(client_id, access_token, refresh_token, expires_at) VALUES (?, ?, ?, ?) ON CONFLICT(client_id) DO UPDATE SET access_token = excluded.access_token, refresh_token = excluded.refresh_token, expires_at = excluded.expires_at")
            .bind(&tokens.client_id).bind(&tokens.access_token).bind(&tokens.refresh_token)
            .bind(i64::try_from(tokens.expires_at)?).execute(&self.pool).await?;
        }
        Ok(())
    }

    /// Import each legacy file once. Existing database rows always take precedence.
    pub async fn import_legacy(&self, directory: &Path) -> Result<()> {
        for (name, credentials) in [("linear-cache.json", false), ("linear-oauth.json", true)] {
            let path = directory.join(name);
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error).context("Cannot inspect legacy Linear data"),
            };
            ensure!(
                metadata.is_file(),
                "Legacy Linear data must be a regular file: {name}"
            );
            ensure!(
                !credentials || metadata.permissions().mode() & 0o077 == 0,
                "Linear credential file must be private (mode 600)"
            );
            let bytes = fs::read(&path).context("Cannot read legacy Linear data")?;
            let mut transaction = self.pool.begin().await?;
            if credentials {
                let tokens: Tokens = serde_json::from_slice(&bytes).map_err(|_| {
                    anyhow::anyhow!(
                        "Invalid linear-oauth.json; repair it or remove it and sign in again"
                    )
                })?;
                sqlx::query("INSERT INTO linear_oauth(client_id, access_token, refresh_token, expires_at) VALUES (?, ?, ?, ?) ON CONFLICT(client_id) DO NOTHING")
                    .bind(tokens.client_id).bind(tokens.access_token).bind(tokens.refresh_token)
                    .bind(i64::try_from(tokens.expires_at)?).execute(&mut *transaction).await?;
            } else {
                let (identity, snapshot): (String, Snapshot) = serde_json::from_slice(&bytes)
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "Invalid linear-cache.json; repair it or remove it and refresh Linear"
                        )
                    })?;
                sqlx::query("INSERT INTO linear_cache(identity, snapshot) VALUES (?, ?) ON CONFLICT(identity) DO NOTHING")
                    .bind(identity).bind(serde_json::to_string(&snapshot)?).execute(&mut *transaction).await?;
            }
            transaction.commit().await?;
            ensure!(
                fs::read(&path)? == bytes,
                "Legacy Linear data changed during import; close older Mentat instances and restart"
            );
            fs::remove_file(&path).context(
                "Linear data imported, but its legacy file could not be removed; restart to retry",
            )?;
        }
        Ok(())
    }
}

fn cache_identity(config: &Config) -> String {
    match &config.api_key {
        Some(key) => format!("api-key:{:x}", Sha256::digest(key.as_bytes())),
        None => config.client_id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> Store {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("../app/migrations")
            .run(&pool)
            .await
            .unwrap();
        Store::new(pool)
    }

    fn config() -> Config {
        Config {
            client_id: "app".into(),
            api_key: None,
        }
    }

    fn snapshot(viewer: &str) -> Snapshot {
        Snapshot {
            viewer: viewer.into(),
            issues: vec![],
        }
    }

    fn tokens(value: &str) -> Tokens {
        Tokens {
            client_id: "app".into(),
            access_token: value.into(),
            refresh_token: value.into(),
            expires_at: 123,
        }
    }

    fn legacy(directory: &Path) {
        fs::write(
            directory.join("linear-cache.json"),
            serde_json::to_vec(&("app", snapshot("old"))).unwrap(),
        )
        .unwrap();
        let path = directory.join("linear-oauth.json");
        fs::write(&path, serde_json::to_vec(&tokens("old-token")).unwrap()).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[tokio::test]
    async fn repository_bindings_isolate_cache_and_tokens_and_reject_workspace_changes() {
        let base = store().await;
        base.save_cache(&config(), &snapshot("legacy"))
            .await
            .unwrap();
        base.save_tokens(&config(), &tokens("legacy-secret"))
            .await
            .unwrap();
        let exchange = Store::for_repository(base.pool.clone(), "/forge/exchange".into());
        let mentat = Store::for_repository(base.pool.clone(), "/code/mentat".into());
        assert!(exchange.cached(&config()).await.unwrap().is_none());
        assert!(exchange.load_tokens(&config()).await.is_err());
        assert!(exchange.require_connection(false).await.is_err());
        let forge = Workspace {
            id: "forge-id".into(),
            name: "Forge".into(),
        };
        let personal = Workspace {
            id: "personal-id".into(),
            name: "Personal".into(),
        };
        assert!(exchange.bind_workspace(&forge, false).await.is_err());
        exchange.bind_workspace(&forge, true).await.unwrap();
        exchange.require_connection(false).await.unwrap();
        exchange
            .save_tokens(&config(), &tokens("forge-secret"))
            .await
            .unwrap();
        exchange
            .save_cache(&config(), &snapshot("forge issues"))
            .await
            .unwrap();
        assert!(mentat.cached(&config()).await.unwrap().is_none());
        assert!(mentat.load_tokens(&config()).await.is_err());
        mentat.bind_workspace(&personal, true).await.unwrap();
        mentat
            .save_tokens(&config(), &tokens("personal-secret"))
            .await
            .unwrap();
        mentat
            .save_cache(&config(), &snapshot("personal issues"))
            .await
            .unwrap();
        for connect in [false, true] {
            assert!(exchange.bind_workspace(&personal, connect).await.is_err());
        }
        assert_eq!(exchange.workspace().await.unwrap(), Some(forge.clone()));
        assert_eq!(
            exchange.load_tokens(&config()).await.unwrap().access_token,
            "forge-secret"
        );
        assert_eq!(
            mentat.load_tokens(&config()).await.unwrap().access_token,
            "personal-secret"
        );
        assert_eq!(
            exchange.cached(&config()).await.unwrap().unwrap().viewer,
            "forge issues"
        );
        assert_eq!(
            mentat.cached(&config()).await.unwrap().unwrap().viewer,
            "personal issues"
        );
        // Workspace IDs survive renames; the displayed name is refreshed.
        let renamed = Workspace {
            name: "Forge renamed".into(),
            ..forge
        };
        exchange.bind_workspace(&renamed, false).await.unwrap();
        assert_eq!(exchange.workspace().await.unwrap(), Some(renamed));
        assert!(exchange.cached(&config()).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn repository_mapping_survives_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(temp.path().join("mentat.db"))
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(options.clone()).await.unwrap();
        sqlx::migrate!("../app/migrations")
            .run(&pool)
            .await
            .unwrap();
        let store = Store::for_repository(pool.clone(), "/forge/exchange".into());
        let workspace = Workspace {
            id: "forge-id".into(),
            name: "Forge".into(),
        };
        store.bind_workspace(&workspace, true).await.unwrap();
        store
            .save_cache(&config(), &snapshot("saved"))
            .await
            .unwrap();
        store
            .save_tokens(&config(), &tokens("saved-secret"))
            .await
            .unwrap();
        pool.close().await;
        let pool = SqlitePool::connect_with(options).await.unwrap();
        let store = Store::for_repository(pool.clone(), "/forge/exchange".into());
        assert_eq!(store.workspace().await.unwrap(), Some(workspace));
        assert_eq!(
            store.cached(&config()).await.unwrap().unwrap().viewer,
            "saved"
        );
        assert_eq!(
            store.load_tokens(&config()).await.unwrap().access_token,
            "saved-secret"
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn isolates_cache_identities_without_storing_api_keys() {
        let store = store().await;
        let mut config = config();
        config.api_key = Some("first-secret".into());
        store.save_cache(&config, &snapshot("User")).await.unwrap();
        assert_eq!(store.cached(&config).await.unwrap().unwrap().viewer, "User");
        let identity: String = sqlx::query_scalar("SELECT identity FROM linear_cache")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert!(!identity.contains("first-secret"));
        config.api_key = Some("second-secret".into());
        assert!(store.cached(&config).await.unwrap().is_none());
        config.api_key = None;
        assert!(store.cached(&config).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn imports_both_files_and_retries_without_overwriting_newer_rows() {
        let store = store().await;
        let temp = tempfile::tempdir().unwrap();
        legacy(temp.path());
        store.import_legacy(temp.path()).await.unwrap();
        assert!(!temp.path().join("linear-cache.json").exists());
        assert!(!temp.path().join("linear-oauth.json").exists());
        assert_eq!(
            store.cached(&config()).await.unwrap().unwrap().viewer,
            "old"
        );
        assert_eq!(
            store.load_tokens(&config()).await.unwrap().access_token,
            "old-token"
        );
        store.save_cache(&config(), &snapshot("new")).await.unwrap();
        store
            .save_tokens(&config(), &tokens("new-token"))
            .await
            .unwrap();
        // Simulate a committed import whose source-file removal was interrupted.
        legacy(temp.path());
        store.import_legacy(temp.path()).await.unwrap();
        store.import_legacy(temp.path()).await.unwrap();
        assert_eq!(
            store.cached(&config()).await.unwrap().unwrap().viewer,
            "new"
        );
        assert_eq!(
            store.load_tokens(&config()).await.unwrap().access_token,
            "new-token"
        );
        let other = Config {
            client_id: "another-app".into(),
            api_key: None,
        };
        assert!(store.load_tokens(&other).await.is_err());
        assert!(store.save_tokens(&other, &tokens("wrong")).await.is_err());
        assert!(store.cached(&other).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn failed_import_retains_file_and_errors_do_not_expose_secrets() {
        let store = store().await;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("linear-oauth.json");
        fs::write(
            &path,
            br#"{"access_token":"secret-token","expires_at":"secret-value"}"#,
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let error = store.import_legacy(temp.path()).await.unwrap_err();
        assert!(!format!("{error:#}").contains("secret"));
        assert!(path.exists());
        fs::write(&path, serde_json::to_vec(&tokens("secret-token")).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.import_legacy(temp.path()).await.is_err());
        assert!(path.exists());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        sqlx::query("DROP TABLE linear_oauth")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store.import_legacy(temp.path()).await.is_err());
        assert!(path.exists());
    }

    #[tokio::test]
    async fn snapshots_and_tokens_survive_database_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(temp.path().join("mentat.db"))
            .create_if_missing(true);
        let pool = SqlitePool::connect_with(options.clone()).await.unwrap();
        sqlx::migrate!("../app/migrations")
            .run(&pool)
            .await
            .unwrap();
        let store = Store::new(pool.clone());
        store
            .save_cache(&config(), &snapshot("offline"))
            .await
            .unwrap();
        store
            .save_tokens(&config(), &tokens("saved-token"))
            .await
            .unwrap();
        pool.close().await;
        let pool = SqlitePool::connect_with(options).await.unwrap();
        let store = Store::new(pool.clone());
        assert_eq!(
            store.cached(&config()).await.unwrap().unwrap().viewer,
            "offline"
        );
        assert_eq!(
            store.load_tokens(&config()).await.unwrap().refresh_token,
            "saved-token"
        );
        pool.close().await;
    }
}
