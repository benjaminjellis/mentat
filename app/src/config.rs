use std::{env, ffi::OsString, path::PathBuf};

use anyhow::{Context, Result, bail};
use crossterm::event::{KeyEvent, KeyModifiers};

use crate::{
    cli::Cli,
    settings::{EditorSettings, Settings},
};

#[derive(Clone)]
pub struct Config {
    pub agent: crate::settings::AgentKind,
    pub linear: Option<::linear::Config>,
    pub editor: EditorSettings,
    pub pane_navigation: bool,
    pub command: Vec<OsString>,
    pub cwd: PathBuf,
    pub escape: char,
    pub protect_child: bool,
    pub agent_mode: bool,
    pub resume: Option<String>,
    pub new_conversation: bool,
}

impl Config {
    pub fn resolve(cli: Cli, settings: Settings) -> Result<Self> {
        let client_id = env::var("LINEAR_CLIENT_ID")
            .ok()
            .or(settings.linear.client_id)
            .unwrap_or_default();
        let config_directory = crate::settings::directory()?;
        let linear = if settings.linear.api_key.is_some() || !client_id.trim().is_empty() {
            Some(::linear::Config {
                client_id,
                api_key: settings.linear.api_key,
                directory: config_directory.clone(),
            })
        } else {
            None
        };
        let escape = cli
            .escape
            .unwrap_or(parse_escape(&settings.keybindings.return_to_sidebar)?);
        if settings.keybindings.pane_navigation && matches!(escape, 'h' | 'j' | 'k' | 'l') {
            bail!(
                "The return shortcut conflicts with pane navigation; disable pane_navigation or choose another shortcut"
            );
        }
        Ok(Self {
            agent: cli.agent.unwrap_or(settings.agent.kind),
            linear,
            command: if cli.editor {
                settings.editor.command()
            } else {
                cli.command
            },
            editor: settings.editor,
            pane_navigation: settings.keybindings.pane_navigation,
            cwd: cli.cwd,
            escape,
            protect_child: cli.protect_child,
            agent_mode: cli.agent_mode,
            resume: cli.resume,
            new_conversation: cli.new_conversation,
        })
    }

    pub fn is_escape(&self, key: KeyEvent) -> bool {
        key.modifiers == KeyModifiers::CONTROL
            && ::terminal::input::encode(key, false) == [self.escape as u8 & 0x1f]
    }
}

pub fn parse_escape(value: &str) -> Result<char> {
    let key = value
        .strip_prefix("ctrl-")
        .context("Use ctrl-] or ctrl-<letter> for the return shortcut")?;
    let mut chars = key.chars();
    let c = chars.next().unwrap_or_default();
    if chars.next().is_some()
        || !(c.is_ascii_lowercase() || matches!(c, ']' | '\\' | '^' | '_'))
        || matches!(c, 'i' | 'm')
    {
        bail!("Use ctrl-] or a control letter other than i or m for the return shortcut");
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(escape: Option<char>) -> Cli {
        Cli {
            editor: true,
            agent: None,
            command: vec![],
            cwd: PathBuf::from("/tmp"),
            escape,
            protect_child: true,
            agent_mode: false,
            resume: None,
            new_conversation: false,
        }
    }

    #[test]
    fn resolves_editor_and_shortcut_precedence() {
        let source = "[editor]\nkind = 'emacs'\n[keybindings]\nreturn_to_sidebar = 'ctrl-]'\npane_navigation = false";
        let config = Config::resolve(cli(None), toml::from_str(source).unwrap()).unwrap();
        assert_eq!(config.command, ["emacs", "-nw"].map(OsString::from));
        assert!(!config.pane_navigation);
        assert!(config.protect_child);
        assert_eq!(config.escape, ']');
        let config = Config::resolve(cli(Some('x')), toml::from_str(source).unwrap()).unwrap();
        assert_eq!(config.escape, 'x');
        assert!(Config::resolve(cli(Some('h')), Settings::default()).is_err());
        for invalid in ["ctrl-i", "ctrl-m", "ctrl-", "alt-g", "ctrl-gg"] {
            assert!(parse_escape(invalid).is_err());
        }
    }
}
