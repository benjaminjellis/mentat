use std::{env, ffi::OsString, path::PathBuf};

use anyhow::{Result, bail};

pub struct Cli {
    pub agent: Option<crate::settings::AgentKind>,
    pub editor: bool,
    pub command: Vec<OsString>,
    pub cwd: PathBuf,
    pub escape: Option<char>,
    pub protect_child: bool,
    pub agent_mode: bool,
    pub resume: Option<String>,
    pub new_conversation: bool,
}

impl Cli {
    pub fn parse() -> Result<Option<Self>> {
        let mut config = Self {
            agent: None,
            editor: false,
            command: vec!["codex".into(), "--no-daemon".into()],
            cwd: env::current_dir()?,
            escape: None,
            protect_child: false,
            agent_mode: true,
            resume: None,
            new_conversation: false,
        };
        let mut args = env::args_os().skip(1);

        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--shell") => {
                    config.editor = false;
                    config.agent_mode = false;
                    config.command = vec![env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into())];
                    config.protect_child = true;
                }
                Some("--codex" | "--claude") => {
                    config.agent = Some(if arg == "--claude" {
                        crate::settings::AgentKind::Claude
                    } else {
                        crate::settings::AgentKind::Codex
                    });
                    config.editor = false;
                    config.agent_mode = true;
                    config.command = vec!["codex".into(), "--no-daemon".into()];
                    config.protect_child = false;
                }
                Some("--editor") => {
                    config.agent_mode = false;
                    config.editor = true;
                    config.command.clear();
                    config.protect_child = true;
                }
                Some("--cwd") => {
                    config.cwd = args
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--cwd needs a path"))?
                        .into()
                }
                Some("--resume") => {
                    config.resume = Some(
                        args.next()
                            .and_then(|id| id.into_string().ok())
                            .ok_or_else(|| {
                                anyhow::anyhow!("--resume needs an agent conversation ID")
                            })?,
                    );
                }
                Some("--new-conversation") => config.new_conversation = true,
                Some("--escape") => {
                    let value = args.next().unwrap_or_default();
                    let value = value.to_str().unwrap_or_default();
                    config.escape = Some(crate::config::parse_escape(value)?);
                }
                Some("--") => {
                    config.editor = false;
                    config.agent_mode = false;
                    config.command = args.collect();
                    if config.command.is_empty() {
                        bail!("Supply a command after --");
                    }
                    // Custom programs may include an editor. Require normal exit.
                    config.protect_child = true;
                    break;
                }
                Some("--help" | "-h") => {
                    println!(
                        "Mentat: live terminal views\n\nUsage: mentat [--codex | --claude | --shell | --editor] [--cwd PATH] [--escape ctrl-i] [--resume ID | --new-conversation] [-- COMMAND ARGS...]\n\nDefault: resume the selected ticket conversation.\nEnter: open agent. a: open agent, paste ticket if new. e: configured editor. t: shell. o: open issue in Linear. Ctrl-I: sidebar. l: choose Linear workspace. ?: help.\nQuit offers a force option when an editor or shell remains open."
                    );
                    return Ok(None);
                }
                _ => bail!("Unknown option: {}", arg.to_string_lossy()),
            }
        }

        if config.resume.is_some() && config.new_conversation {
            bail!("Use either --resume or --new-conversation");
        }
        if !config.agent_mode && (config.resume.is_some() || config.new_conversation) {
            bail!("Conversation options require an agent mode");
        }
        config.cwd = config.cwd.canonicalize()?;
        if !config.cwd.is_dir() {
            bail!("The working directory must be a directory");
        }
        Ok(Some(config))
    }
}
