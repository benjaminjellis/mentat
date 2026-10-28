mod agent_session;
mod app;
mod browser;
mod cli;
mod clipboard;
mod codex_session;
mod config;
mod host;
mod issue_form;
mod linear_state;
mod logging;
mod session;
mod settings;
mod ticket;
mod workspace;

fn main() -> anyhow::Result<()> {
    let _logs = logging::init()?;
    let span = tracing::info_span!(
        "run",
        pid = std::process::id(),
        version = env!("CARGO_PKG_VERSION")
    );
    let _entered = span.enter();
    tracing::info!("Mentat starting");
    let result = run();
    if let Err(error) = &result {
        logging::failure("application", error);
    }
    tracing::info!(success = result.is_ok(), "Mentat stopped");
    result
}

fn run() -> anyhow::Result<()> {
    let Some(cli) = cli::Cli::parse()? else {
        return Ok(());
    };
    let settings = settings::Settings::load(&settings::directory()?)?;
    let config = config::Config::resolve(cli, settings)?;
    app::run(config)
}
