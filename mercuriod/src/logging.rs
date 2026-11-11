//! The daemon's logs, whose level `setLogLevel` changes while it runs.

use std::sync::Mutex;

use mercurio_server::admin::{AdminError, ErrorCode};
use tracing_subscriber::{fmt, prelude::*, reload, EnvFilter, Registry};

use crate::config::LoggingConfig;

/// The level in force: the directives `[logging] level` takes, applied to
/// every log output.
pub struct LogLevel {
    handle: reload::Handle<EnvFilter, Registry>,
    /// As last given, without the directive `connection_messages` adds
    directives: Mutex<String>,
    connection_messages: bool,
}

impl LogLevel {
    /// The level `directives` sets, `info` when they are invalid, and the
    /// layer that filters by it.
    pub fn new(
        directives: &str,
        connection_messages: bool,
    ) -> (LogLevel, reload::Layer<EnvFilter, Registry>) {
        let (directives, filter) = match filter(directives, connection_messages) {
            Ok(filter) => (directives.to_string(), filter),
            Err(_) => (
                "info".to_string(),
                filter("info", connection_messages).expect("info is a valid level"),
            ),
        };
        let (layer, handle) = reload::Layer::new(filter);
        let level = LogLevel {
            handle,
            directives: Mutex::new(directives),
            connection_messages,
        };
        (level, layer)
    }

    pub fn get(&self) -> String {
        self.directives.lock().unwrap().clone()
    }

    /// Filters by `directives` from now until the broker restarts.
    pub fn set(&self, directives: &str) -> Result<(), AdminError> {
        let filter =
            filter(directives, self.connection_messages).map_err(AdminError::invalid_argument)?;
        // Held while swapping, so `get` never names a level not in force
        let mut current = self.directives.lock().unwrap();
        self.handle
            .reload(filter)
            .map_err(|e| AdminError::new(ErrorCode::Unavailable, e.to_string()))?;
        *current = directives.to_string();
        Ok(())
    }
}

/// The filter for `directives`. Without `connection_messages`, connection
/// messages stay off unless the directives name their target.
fn filter(directives: &str, connection_messages: bool) -> Result<EnvFilter, String> {
    if directives.trim().is_empty() {
        return Err("the level is empty".to_string());
    }
    let mut filter =
        EnvFilter::try_new(directives).map_err(|e| format!("invalid level `{directives}`: {e}"))?;
    if !connection_messages && !directives.contains(mercurio_server::CONNECTIONS) {
        filter = filter.add_directive(
            format!("{}=off", mercurio_server::CONNECTIONS)
                .parse()
                .expect("valid directive"),
        );
    }
    Ok(filter)
}

/// Sends logs where `config` says, at `debug` with `verbose`.
pub fn init(config: &LoggingConfig, verbose: bool) -> Result<LogLevel, String> {
    let directives = if verbose { "debug" } else { &config.level };
    let (level, filter) = LogLevel::new(directives, config.connection_messages);
    let registry = tracing_subscriber::registry().with(filter);
    #[cfg(unix)]
    let syslog = match config.syslog {
        true => Some(
            crate::syslog::Syslog::connect("/dev/log")
                .map_err(|e| format!("Failed to connect to syslog at /dev/log: {}", e))?,
        ),
        false => None,
    };
    #[cfg(not(unix))]
    let syslog: Option<std::io::Sink> = match config.syslog {
        true => return Err("syslog is only supported on Unix".to_string()),
        false => None,
    };
    match (syslog, &config.file) {
        (Some(syslog), _) => registry
            .with(
                fmt::layer()
                    .with_ansi(false)
                    .without_time()
                    .with_writer(syslog),
            )
            .init(),
        (None, Some(path)) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| format!("Failed to open log file {}: {}", path, e))?;
            registry
                .with(fmt::layer().with_ansi(false).with_writer(Mutex::new(file)))
                .init();
        }
        (None, None) => registry.with(fmt::layer()).init(),
    }
    Ok(level)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use mercurio_server::admin::ErrorCode;
    use tracing_subscriber::{fmt, prelude::*};

    use super::LogLevel;

    /// Collects what is logged.
    #[derive(Clone, Default)]
    struct Logs(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Logs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Logs {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    /// Runs `body` with logs filtered by a new `LogLevel` and collected.
    fn logged(directives: &str, connection_messages: bool, body: impl FnOnce(&LogLevel)) -> String {
        let logs = Logs::default();
        let writer = logs.clone();
        let (level, filter) = LogLevel::new(directives, connection_messages);
        let subscriber = tracing_subscriber::registry().with(filter).with(
            fmt::layer()
                .with_ansi(false)
                .with_writer(move || writer.clone()),
        );
        tracing::subscriber::with_default(subscriber, || body(&level));
        logs.text()
    }

    #[test]
    fn test_set_applies_new_directives_and_refuses_bad_ones() {
        let logs = logged("info", true, |level| {
            tracing::debug!("alpha");
            level.set("info,mercuriod=debug").unwrap();
            tracing::debug!("bravo");
            for bad in ["", "  ", "info,mercuriod=loud"] {
                let error = level.set(bad).unwrap_err();
                assert_eq!(error.code, ErrorCode::InvalidArgument, "{bad:?}");
            }
            assert_eq!(level.get(), "info,mercuriod=debug");
            tracing::debug!("charlie");
        });
        assert!(!logs.contains("alpha"), "{logs}");
        assert!(logs.contains("bravo") && logs.contains("charlie"), "{logs}");
    }

    #[test]
    fn test_connection_messages_stay_off_unless_named() {
        let logs = logged("info", false, |level| {
            tracing::info!(target: "mercurio::connections", "alpha");
            level.set("debug").unwrap();
            tracing::info!(target: "mercurio::connections", "bravo");
            level.set("info,mercurio::connections=info").unwrap();
            tracing::info!(target: "mercurio::connections", "charlie");
        });
        assert!(!logs.contains("alpha") && !logs.contains("bravo"), "{logs}");
        assert!(logs.contains("charlie"), "{logs}");
    }

    #[test]
    fn test_an_invalid_configured_level_falls_back_to_info() {
        let (level, _filter) = LogLevel::new("info,x=loud", true);
        assert_eq!(level.get(), "info");
    }
}
