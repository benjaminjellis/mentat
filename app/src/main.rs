mod agent_session;
mod app;
mod cli;
mod codex_session;
mod config;
mod linear_state;
mod settings;
mod ticket;
mod workspace;

fn main() -> anyhow::Result<()> {
    let Some(cli) = cli::Cli::parse()? else {
        return Ok(());
    };
    let settings = settings::Settings::load(&settings::directory()?)?;
    let config = config::Config::resolve(cli, settings)?;
    app::run(config)
}
