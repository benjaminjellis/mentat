use crate::{codex_session, config::Config, settings::AgentKind};
use anyhow::Result;

pub fn prepare(config: &mut Config) -> Result<()> {
    if !config.agent_mode {
        return Ok(());
    }
    if config.agent == AgentKind::Codex {
        return codex_session::prepare(config);
    }
    let (id, resume) = match &config.resume {
        Some(id) if !config.new_conversation => (id.clone(), true),
        _ => (new_id()?, false),
    };
    config.command = vec![
        "claude".into(),
        if resume { "--resume" } else { "--session-id" }.into(),
        id.clone().into(),
    ];
    config.resume = Some(id);
    Ok(())
}

fn new_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("Cannot obtain a session ID"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = format!("{:032x}", u128::from_be_bytes(bytes));
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cli::Cli, settings::Settings};

    #[test]
    fn claude_uses_a_stable_id_for_resume() {
        let mut config = Config::resolve(
            Cli {
                agent: Some(AgentKind::Claude),
                editor: false,
                command: vec![],
                cwd: "/tmp".into(),
                escape: None,
                protect_child: false,
                agent_mode: true,
                resume: None,
                new_conversation: false,
            },
            Settings::default(),
        )
        .unwrap();
        prepare(&mut config).unwrap();
        let id = config.resume.clone().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(config.command[1], "--session-id");
        prepare(&mut config).unwrap();
        assert_eq!(config.command[1], "--resume");
        assert_eq!(config.resume.as_deref(), Some(id.as_str()));
        config.new_conversation = true;
        prepare(&mut config).unwrap();
        assert_ne!(config.resume.as_deref(), Some(id.as_str()));
    }
}
