use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    env,
    ffi::OsString,
    fs,
    io::{ErrorKind, Write},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
};

const TEMPLATE: &str = r#"# Mentat configuration

[agent]
kind = "codex" # codex or claude

[editor]
kind = "nvim" # nvim, vim, or emacs
# executable = "/custom/path/nvim"
args = []

[keybindings]
return_to_sidebar = "ctrl-g"
pane_navigation = true

[linear]
# api_key = "lin_api_..."
# client_id = "your-public-oauth-client-id"
"#;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub agent: AgentSettings,
    pub editor: EditorSettings,
    pub keybindings: Keybindings,
    pub linear: LinearSettings,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinearSettings {
    pub api_key: Option<String>,
    pub client_id: Option<String>,
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    #[default]
    Codex,
    Claude,
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentSettings {
    pub kind: AgentKind,
}

impl AgentKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EditorKind {
    #[default]
    Nvim,
    Vim,
    Emacs,
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EditorSettings {
    pub kind: EditorKind,
    pub executable: Option<String>,
    pub args: Vec<String>,
}

impl EditorSettings {
    pub fn name(&self) -> &'static str {
        match self.kind {
            EditorKind::Nvim => "Neovim",
            EditorKind::Vim => "Vim",
            EditorKind::Emacs => "Emacs",
        }
    }

    pub fn command(&self) -> Vec<OsString> {
        let program = self.executable.as_deref().unwrap_or(match self.kind {
            EditorKind::Nvim => "nvim",
            EditorKind::Vim => "vim",
            EditorKind::Emacs => "emacs",
        });
        let mut command = vec![OsString::from(program)];
        if matches!(self.kind, EditorKind::Emacs) {
            command.push("-nw".into());
        }
        command.extend(self.args.iter().map(OsString::from));
        command
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Keybindings {
    pub return_to_sidebar: String,
    pub pane_navigation: bool,
}

impl Default for Keybindings {
    fn default() -> Self {
        Self {
            return_to_sidebar: "ctrl-g".into(),
            pane_navigation: true,
        }
    }
}

pub fn directory() -> Result<PathBuf> {
    resolve_directory(
        env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).as_deref(),
        env::var_os("HOME").map(PathBuf::from).as_deref(),
    )
}

fn resolve_directory(xdg: Option<&Path>, home: Option<&Path>) -> Result<PathBuf> {
    if let Some(xdg) = xdg.filter(|path| path.is_absolute()) {
        return Ok(xdg.join("mentat"));
    }
    let home = home
        .filter(|path| path.is_absolute())
        .context("Cannot locate the user configuration directory")?;
    Ok(home.join(".config/mentat"))
}

pub fn state_directory() -> Result<PathBuf> {
    resolve_state_directory(
        env::var_os("XDG_STATE_HOME").map(PathBuf::from).as_deref(),
        env::var_os("HOME").map(PathBuf::from).as_deref(),
    )
}

fn resolve_state_directory(xdg: Option<&Path>, home: Option<&Path>) -> Result<PathBuf> {
    if let Some(xdg) = xdg.filter(|p| p.is_absolute()) {
        return Ok(xdg.join("mentat"));
    }
    Ok(home
        .filter(|p| p.is_absolute())
        .context("Cannot locate the user state directory")?
        .join(".local/state/mentat"))
}

impl Settings {
    pub fn load(directory: &Path) -> Result<Self> {
        let path = directory.join("config.toml");
        let mut builder = fs::DirBuilder::new();
        builder
            .recursive(true)
            .mode(0o700)
            .create(directory)
            .with_context(|| format!("Cannot create {}", directory.display()))?;

        if !path.try_exists()? {
            let mut file = tempfile::NamedTempFile::new_in(directory)?;
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(TEMPLATE.as_bytes())?;
            file.as_file().sync_all()?;
            if let Err(error) = file.persist_noclobber(&path)
                && error.error.kind() != ErrorKind::AlreadyExists
            {
                return Err(error.error)
                    .with_context(|| format!("Cannot create {}", path.display()));
            }
        }

        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_file() && metadata.permissions().mode() & 0o077 == 0,
            "{} must be a private regular file (mode 600)",
            path.display()
        );
        let source =
            fs::read_to_string(&path).with_context(|| format!("Cannot read {}", path.display()))?;
        // TOML diagnostics include source lines, which can contain credentials.
        let settings: Self = toml::from_str(&source).map_err(|_| {
            anyhow::anyhow!("Invalid TOML or unknown settings in {}", path.display())
        })?;
        for (name, value) in [
            ("api_key", &settings.linear.api_key),
            ("client_id", &settings.linear.client_id),
        ] {
            if value.as_ref().is_some_and(|value| value.trim().is_empty()) {
                bail!("linear.{name} must not be empty in {}", path.display());
            }
        }
        if settings
            .editor
            .executable
            .as_ref()
            .is_some_and(|p| p.trim().is_empty() || p.contains('\0'))
            || settings.editor.args.iter().any(|arg| arg.contains('\0'))
        {
            bail!(
                "Invalid editor executable or arguments in {}",
                path.display()
            );
        }
        crate::config::parse_escape(&settings.keybindings.return_to_sidebar)?;
        // Expose the provider choice in older files without changing user settings.
        let table: toml::Table = toml::from_str(&source)
            .map_err(|_| anyhow::anyhow!("Invalid configuration in {}", path.display()))?;
        if !table.contains_key("agent") {
            let mut file = tempfile::NamedTempFile::new_in(directory)?;
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
            write!(
                file,
                "{source}\n[agent]\nkind = \"codex\" # codex or claude\n"
            )?;
            file.as_file().sync_all()?;
            ensure!(
                fs::read_to_string(&path)? == source,
                "Configuration changed during startup; restart Mentat"
            );
            file.persist(&path)
                .with_context(|| format!("Cannot update {}", path.display()))?;
        }
        Ok(settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_paths_follow_xdg() {
        let home = Some(Path::new("/home/user"));
        assert_eq!(
            resolve_state_directory(Some(Path::new("/state")), None).unwrap(),
            Path::new("/state/mentat")
        );
        for invalid in [None, Some(Path::new("")), Some(Path::new("relative"))] {
            assert_eq!(
                resolve_state_directory(invalid, home).unwrap(),
                Path::new("/home/user/.local/state/mentat")
            );
        }
    }

    #[test]
    fn editor_presets_and_overrides() {
        for (kind, expected) in [
            ("nvim", vec!["nvim"]),
            ("vim", vec!["vim"]),
            ("emacs", vec!["emacs", "-nw"]),
        ] {
            let settings: Settings = toml::from_str(&format!("[editor]\nkind = '{kind}'")).unwrap();
            assert_eq!(
                settings.editor.command(),
                expected.into_iter().map(OsString::from).collect::<Vec<_>>()
            );
        }
        let settings: Settings = toml::from_str(
            "[editor]\nkind = 'emacs'\nexecutable = '/custom path/emacs'\nargs = ['--quick']",
        )
        .unwrap();
        assert_eq!(
            settings.editor.command(),
            ["/custom path/emacs", "-nw", "--quick"].map(OsString::from)
        );
        assert!(toml::from_str::<Settings>("[editor]\nkind = 'unknown'").is_err());
        assert!(toml::from_str::<Settings>("[editor]\narg = []").is_err());
    }

    #[test]
    fn config_paths_follow_xdg() {
        let home = Some(Path::new("/home/user"));
        assert_eq!(
            resolve_directory(Some(Path::new("/config")), None).unwrap(),
            Path::new("/config/mentat")
        );
        for invalid in [None, Some(Path::new("")), Some(Path::new("relative"))] {
            assert_eq!(
                resolve_directory(invalid, home).unwrap(),
                Path::new("/home/user/.config/mentat")
            );
        }
        assert!(resolve_directory(None, None).is_err());
    }

    #[test]
    fn creates_private_template_and_preserves_user_settings() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("mentat");
        let generated = Settings::load(&directory).unwrap();
        assert!(generated.linear.api_key.is_none());
        assert_eq!(generated.editor.command(), [OsString::from("nvim")]);
        assert!(generated.keybindings.pane_navigation);
        assert_eq!(generated.keybindings.return_to_sidebar, "ctrl-g");
        let path = directory.join("config.toml");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let source = "[linear]\napi_key = 'test-secret'\nclient_id = 'app'\n";
        fs::write(&path, source).unwrap();
        let settings = Settings::load(&directory).unwrap();
        assert_eq!(settings.linear.api_key.as_deref(), Some("test-secret"));
        assert_eq!(settings.linear.client_id.as_deref(), Some("app"));
        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.starts_with(source));
        assert!(updated.contains("[agent]\nkind = \"codex\""));
        Settings::load(&directory).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), updated);
        fs::write(
            &path,
            updated.replace("kind = \"codex\"", "kind = \"claude\""),
        )
        .unwrap();
        assert!(Settings::load(&directory).unwrap().agent.kind == AgentKind::Claude);
        fs::write(&path, "[linear]\napi_key = 'test-secret").unwrap();
        let error = Settings::load(&directory).err().unwrap().to_string();
        assert!(!error.contains("test-secret"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Settings::load(&directory).is_err());
    }
}
