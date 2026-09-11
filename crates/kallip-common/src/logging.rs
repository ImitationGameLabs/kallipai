//! Opt-in rolling file logging for the polis services.
//!
//! The services run under stdout capture (compose/journald), so stdout stays
//! the always-on channel. A service opts into a daily-rolling file via its
//! `KALLIP_<SERVICE>_LOG_DIR` variable: set to a directory, events are
//! double-written (file + stdout); unset or empty keeps the historical
//! stdout-only behavior, so container and development forms are untouched.
//! A directory that cannot be created, or an appender that fails to build,
//! degrades to stdout-only with a one-line notice, mirroring the tagma
//! precedent. Directory ownership and permissions stay a deployment concern
//! (systemd `LogsDirectory` or volume configuration); this module only
//! creates the directory if missing.

use std::path::{Path, PathBuf};

use tracing_subscriber::prelude::*;

/// Interpret the raw `KALLIP_<SERVICE>_LOG_DIR` value: unset or empty means
/// no file logging (a blank value is how a unit file or compose entry
/// expresses "keep the default", matching how tagma reads blank variables);
/// any other value is the service's own log directory, used verbatim.
pub fn parse_log_dir(raw: Option<String>) -> Option<PathBuf> {
    match raw {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => None,
    }
}

/// Build the rolling file layer for `dir`: daily rotation, seven files kept,
/// `<service>.log.` prefixed names. Any failure to create the directory or
/// the appender degrades to `None` with a one-line stderr notice.
pub fn build_file_layer<S>(
    dir: &Path,
    service: &str,
    filter: &tracing_subscriber::EnvFilter,
) -> Option<impl tracing_subscriber::Layer<S>>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("kallip: creating the {service} log dir failed, keeping stdout-only: {e}");
        return None;
    }
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(service)
        .filename_suffix("log")
        .max_log_files(7)
        .build(dir)
        .map_err(|e| {
            eprintln!("kallip: {service} file log init failed, keeping stdout-only: {e}");
            e
        })
        .ok()?;
    Some(
        tracing_subscriber::fmt::layer()
            .with_writer(std::sync::Mutex::new(appender))
            .with_ansi(false)
            .with_filter(filter.clone()),
    )
}

/// Compose the service stack: a stdout layer always (the journald capture
/// channel) plus the rolling file layer when `dir` resolves, on top of the
/// shared registry. The stdout writer is a parameter so tests can capture
/// that arm; production passes `std::io::stdout`.
fn service_layers<W>(
    filter: &tracing_subscriber::EnvFilter,
    service: &str,
    dir: Option<&Path>,
    stdout: W,
) -> impl tracing::Subscriber
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + 'static,
{
    // Each arm states its ANSI choice explicitly instead of leaning on the
    // fmt default: stdout keeps color (journald and terminals tolerate
    // it), the file arm drops it so escapes never pollute archived logs.
    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_writer(stdout)
        .with_ansi(true)
        .with_filter(filter.clone());
    let file_layer = dir.and_then(|dir| build_file_layer(dir, service, filter));
    tracing_subscriber::registry()
        .with(stdout_layer)
        .with(file_layer)
}

/// Install the process-wide subscriber. Dual writing is the point -- the
/// file is an added channel, not a replacement -- so the panic hook keeps
/// its default behavior and panics keep reaching stderr, and with it
/// journald.
/// Installing over an existing global subscriber silently does nothing (the
/// set_global_default error is swallowed); the services install exactly once
/// at startup, so this only matters for embedders.
pub fn init_service_logging(
    filter: &tracing_subscriber::EnvFilter,
    service: &str,
    dir: Option<&Path>,
) {
    tracing::subscriber::set_global_default(service_layers(filter, service, dir, std::io::stdout))
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_filter() -> tracing_subscriber::EnvFilter {
        tracing_subscriber::EnvFilter::new("info")
    }

    /// A guard under the managed /tmp/kallipai-dev root: the TempDir
    /// deletes the tree on drop, so nothing outlives the run (Deref/
    /// AsRef keep call sites reading as plain paths).
    struct DevDir(tempfile::TempDir);

    impl std::ops::Deref for DevDir {
        type Target = std::path::Path;

        fn deref(&self) -> &Self::Target {
            self.0.path()
        }
    }

    impl AsRef<std::path::Path> for DevDir {
        fn as_ref(&self) -> &std::path::Path {
            self.0.path()
        }
    }

    fn temp_dir(label: &str) -> DevDir {
        let root = std::env::temp_dir().join("kallipai-dev");
        std::fs::create_dir_all(&root).expect("create /tmp/kallipai-dev");
        DevDir(
            tempfile::Builder::new()
                .prefix(&format!("logging-{label}-"))
                .tempdir_in(root)
                .expect("create test tempdir"),
        )
    }

    #[test]
    fn parse_log_dir_treats_unset_as_no_file_logging() {
        assert_eq!(parse_log_dir(None), None);
    }

    #[test]
    fn parse_log_dir_treats_empty_as_no_file_logging() {
        assert_eq!(parse_log_dir(Some(String::new())), None);
    }

    #[test]
    fn parse_log_dir_keeps_the_value_verbatim() {
        assert_eq!(
            parse_log_dir(Some("/var/log/kallipai/archeion".into())),
            Some(PathBuf::from("/var/log/kallipai/archeion"))
        );
    }

    #[test]
    fn file_layer_writes_events_into_the_directory() {
        let dir = temp_dir("writes");
        let layer = build_file_layer(&dir, "archeion", &test_filter())
            .expect("a creatable dir builds the file layer");
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            tracing::info!("kallip logging marker event")
        });
        let contents: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap_or_default())
            .collect();
        assert!(
            contents
                .iter()
                .any(|text| text.contains("kallip logging marker event")),
            "the marker event must land in a rolling file, got {contents:?}"
        );
    }

    #[test]
    fn file_layer_degrades_when_the_dir_cannot_be_created() {
        let dir = temp_dir("blocked");
        let blocker = dir.join("occupant");
        std::fs::write(&blocker, b"").unwrap();
        assert!(
            build_file_layer::<tracing_subscriber::Registry>(&blocker, "archeion", &test_filter())
                .is_none()
        );
    }

    #[derive(Clone)]
    struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
        type Writer = MutexGuardWriter<'a>;

        fn make_writer(&'a self) -> Self::Writer {
            MutexGuardWriter(self.0.lock().unwrap())
        }
    }

    struct MutexGuardWriter<'a>(std::sync::MutexGuard<'a, Vec<u8>>);

    impl std::io::Write for MutexGuardWriter<'_> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.0.flush()
        }
    }

    #[test]
    fn service_layers_dual_write_reaches_both_file_and_stdout() {
        let dir = temp_dir("dual");
        let captured = SharedBuf(Default::default());
        let subscriber = service_layers(&test_filter(), "archeion", Some(&dir), captured.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("kallip dual write marker");
        });
        let file_text: String = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap_or_default())
            .collect();
        assert!(
            file_text.contains("kallip dual write marker"),
            "the file arm must carry the event: {file_text:?}"
        );
        assert!(
            !file_text.contains(0x1b as char),
            "the file arm must be ANSI-free, got {file_text:?}"
        );
        let stdout_bytes = captured.0.lock().unwrap().clone();
        let stdout_text = String::from_utf8_lossy(&stdout_bytes).into_owned();
        assert!(
            stdout_text.contains("kallip dual write marker"),
            "the stdout arm must carry the event: {stdout_text:?}"
        );
        assert!(
            stdout_text.contains(0x1b as char),
            "the stdout arm keeps ANSI coloring"
        );
    }
}
