use anyhow::{Context, Result, ensure};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tracing::level_filters::LevelFilter;
use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{
    filter::{FilterFn, filter_fn},
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

const RETAINED_FILES: usize = 7;

pub fn directory() -> Result<PathBuf> {
    Ok(crate::settings::state_directory()?.join("logs"))
}

fn appender(directory: &Path) -> Result<RollingFileAppender> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .context("Cannot create the Mentat log directory")?;
    ensure!(
        fs::symlink_metadata(directory)?.is_dir(),
        "Log directory must be a real directory"
    );
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("mentat")
        .filename_suffix("jsonl")
        .max_log_files(RETAINED_FILES)
        .build(directory)
        .context("Cannot open the Mentat log file")
}

fn level(value: Option<&str>) -> Result<LevelFilter> {
    value
        .unwrap_or("info")
        .parse()
        .context("MENTAT_LOG must be off, error, warn, info, debug, or trace")
}

fn filter(level: LevelFilter) -> FilterFn<impl Fn(&tracing::Metadata<'_>) -> bool> {
    // Match complete crate names, not prefixes such as the dependency lineark_sdk.
    filter_fn(move |metadata| {
        let target = metadata.target();
        (target == "mentat"
            || target.starts_with("mentat::")
            || target == "linear"
            || target.starts_with("linear::"))
            && level >= *metadata.level()
    })
    .with_max_level_hint(level)
}

pub fn init() -> Result<WorkerGuard> {
    let level = level(std::env::var("MENTAT_LOG").ok().as_deref())?;
    let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .buffered_lines_limit(8192)
        .thread_name("mentat-log-writer")
        .finish(appender(&directory()?)?);
    tracing_subscriber::registry()
        .with(filter(level))
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(false)
                .with_thread_ids(true)
                .with_writer(writer),
        )
        .try_init()
        .context("Cannot initialize Mentat logging")?;
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Panic payloads can contain credentials or remote data; log only location.
        if let Some(location) = info.location() {
            tracing::error!(
                file = location.file(),
                line = location.line(),
                "Application panicked"
            );
        } else {
            tracing::error!("Application panicked");
        }
        previous(info);
    }));
    Ok(guard)
}

/// Error text may contain remote responses, commands, tokens, or ticket content.
/// Record useful typed diagnostics without formatting the error or its chain.
pub fn failure(operation: &'static str, error: &anyhow::Error) {
    let io = error.downcast_ref::<std::io::Error>();
    let database = error.downcast_ref::<sqlx::Error>();
    let database_kind = database.map(|error| match error {
        sqlx::Error::PoolTimedOut => "pool_timeout",
        sqlx::Error::PoolClosed => "pool_closed",
        sqlx::Error::Database(_) => "database",
        sqlx::Error::Io(_) => "io",
        _ => "other",
    });
    tracing::error!(operation, io_kind = ?io.map(std::io::Error::kind),
        os_error = ?io.and_then(std::io::Error::raw_os_error), database_kind,
        "Operation failed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn log_levels_are_explicit_and_do_not_accept_dependency_directives() {
        assert_eq!(level(None).unwrap(), LevelFilter::INFO);
        assert_eq!(level(Some("debug")).unwrap(), LevelFilter::DEBUG);
        assert_eq!(level(Some("off")).unwrap(), LevelFilter::OFF);
        assert!(level(Some("reqwest=trace")).is_err());
    }

    #[test]
    fn rotation_cleans_old_logs_and_keeps_unrelated_files() {
        let temp = tempfile::tempdir().unwrap();
        for day in 1..=12 {
            fs::write(
                temp.path().join(format!("mentat.2000-01-{day:02}.jsonl")),
                "old",
            )
            .unwrap();
        }
        fs::write(temp.path().join("keep.txt"), "keep").unwrap();
        let mut writer = appender(temp.path()).unwrap();
        writeln!(writer, "new").unwrap();
        let logs: Vec<_> = fs::read_dir(temp.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        assert!(logs.len() <= RETAINED_FILES);
        assert!(
            logs.iter()
                .any(|path| fs::read_to_string(path).unwrap().contains("new"))
        );
        // Seeded files can have identical filesystem creation timestamps;
        // retention must prune the set without relying on their tie order.
        assert!(
            logs.iter()
                .filter(|path| path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("mentat.2000-"))
                .count()
                < 12
        );
        assert!(temp.path().join("keep.txt").exists());
        assert_eq!(
            fs::metadata(temp.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn logs_flush_as_json_and_exclude_dependency_and_error_payloads() {
        let temp = tempfile::tempdir().unwrap();
        let (writer, guard) = tracing_appender::non_blocking(appender(temp.path()).unwrap());
        let subscriber = tracing_subscriber::registry()
            .with(filter(LevelFilter::DEBUG))
            .with(tracing_subscriber::fmt::layer().json().with_writer(writer));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(operation = "test", "Application event");
            tracing::debug!("Debug event");
            tracing::error!(target: "reqwest", "secret-http-token");
            tracing::error!(target: "lineark_sdk", "secret-sdk-response");
            failure("test", &anyhow::anyhow!("secret-error-content"));
        });
        drop(guard);
        let path = fs::read_dir(temp.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let text = fs::read_to_string(path).unwrap();
        assert!(text.contains("Application event"));
        assert!(text.contains("Debug event"));
        assert!(!text.contains("secret"));
        for line in text.lines() {
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        }
    }
}
