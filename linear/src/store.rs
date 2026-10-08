use crate::{Config, Snapshot, oauth::Tokens};
use anyhow::{Context, Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{fs, io::Write, os::unix::fs::PermissionsExt};

pub(crate) fn load_tokens(config: &Config) -> Result<Tokens> {
    let path = config.directory.join("linear-oauth.json");
    let metadata =
        fs::symlink_metadata(&path).context("Linear is not connected; press l to sign in")?;
    ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o077 == 0,
        "Linear credential file must be a private regular file (mode 600)"
    );
    let tokens: Tokens = read(config, "linear-oauth.json")?;
    ensure!(
        tokens.client_id == config.client_id,
        "Linear application changed; press l to reconnect"
    );
    Ok(tokens)
}

pub(crate) fn save_tokens(config: &Config, tokens: &Tokens) -> Result<()> {
    write(config, "linear-oauth.json", tokens)
}

pub(crate) fn load_cache(config: &Config) -> Result<Snapshot> {
    let (client_id, snapshot): (String, Snapshot) = read(config, "linear-cache.json")?;
    ensure!(
        client_id == cache_identity(config),
        "Cache belongs to another Linear application"
    );
    Ok(snapshot)
}

pub(crate) fn save_cache(config: &Config, snapshot: &Snapshot) -> Result<()> {
    write(
        config,
        "linear-cache.json",
        &(cache_identity(config), snapshot),
    )
}

fn cache_identity(config: &Config) -> String {
    match &config.api_key {
        Some(key) => format!("api-key:{:x}", Sha256::digest(key.as_bytes())),
        None => config.client_id.clone(),
    }
}

fn read<T: DeserializeOwned>(config: &Config, name: &str) -> Result<T> {
    serde_json::from_slice(&fs::read(config.directory.join(name))?)
        .context("Cannot read Linear local data")
}

fn write(config: &Config, name: &str, value: &impl Serialize) -> Result<()> {
    fs::create_dir_all(&config.directory)?;
    fs::set_permissions(&config.directory, fs::Permissions::from_mode(0o700))?;
    let mut temp = tempfile::NamedTempFile::new_in(&config.directory)?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    serde_json::to_writer(&mut temp, value)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist(config.directory.join(name))
        .context("Cannot save Linear local data")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_cache_does_not_expose_or_mix_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config {
            client_id: String::new(),
            api_key: Some("first-secret".into()),
            directory: temp.path().into(),
        };
        save_cache(
            &config,
            &Snapshot {
                viewer: "User".into(),
                issues: vec![],
            },
        )
        .unwrap();
        assert_eq!(load_cache(&config).unwrap().viewer, "User");
        assert!(
            !fs::read_to_string(temp.path().join("linear-cache.json"))
                .unwrap()
                .contains("first-secret")
        );
        config.api_key = Some("second-secret".into());
        assert!(load_cache(&config).is_err());
        config.api_key = None;
        assert!(load_cache(&config).is_err());
    }

    #[test]
    fn credentials_are_private_and_bound_to_the_client() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config {
            client_id: "app".into(),
            api_key: None,
            directory: temp.path().join("mentat"),
        };
        let tokens = Tokens {
            client_id: "app".into(),
            access_token: "test".into(),
            refresh_token: "test".into(),
            expires_at: 1,
        };
        save_tokens(&config, &tokens).unwrap();
        assert_eq!(
            fs::metadata(config.directory.join("linear-oauth.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(load_tokens(&config).unwrap().client_id, "app");
        config.client_id = "another-app".into();
        assert!(load_tokens(&config).is_err());
    }
}
