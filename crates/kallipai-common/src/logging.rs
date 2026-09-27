//! Opt-in rolling file logging for the polis services.
//!
//! A service opts into a daily-rolling file via its
//! `KALLIPAI_<SERVICE>_LOG_DIR` variable: set to a directory, the file is
//! the only event channel -- stdout carries a single signpost line (the
//! service name, the active log file, and the level threshold) and then
//! goes quiet. Unset or empty keeps the stdout-only behavior, so the
//! container and development forms are untouched. A directory that cannot
//! be created, or an appender that fails to build, is a fast failure: one
//! stderr line and a non-zero exit, because a file mode that quietly lost
//! its events would be indistinguishable from a healthy one. Panics keep
//! their default stderr path. Directory ownership and permissions stay a
//! deployment concern (systemd `LogsDirectory` or volume configuration);
//! this module only creates the directory if missing.

use std::path::{Path, PathBuf};

use tracing_subscriber::prelude::*;

/// Interpret the raw `KALLIPAI_<SERVICE>_LOG_DIR` value: unset or empty means
/// no file logging (a blank value is how a unit file or compose entry
/// expresses "keep the default", matching how tagma reads blank variables);
/// any other value is the service's own log directory, used verbatim.
pub fn parse_log_dir(raw: Option<String>) -> Option<PathBuf> {
    match raw {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => None,
    }
}

/// The current daily file for `service` inside `dir`, in the
/// `<service>.<date>.log` shape the rolling appender writes. The date is
/// UTC, matching the appender's rotation boundary.
fn active_file(dir: &Path, service: &str) -> PathBuf {
    dir.join(format!("{service}.{}.log", utc_today()))
}

/// Today's UTC date as `Y-m-d`, derived from the system clock without a
/// date dependency.
fn utc_today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The civil date for days since the Unix epoch (civil-from-days):
/// pure, so the month-end and leap-day shapes are unit-testable
/// without a clock.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

/// Build the rolling file layer for `dir`: daily rotation, seven files kept,
/// `<service>.<date>.log` names. Any failure to create the directory or the
/// appender surfaces as an error for the caller to fail fast on.
fn try_build_file_layer<S>(
    dir: &Path,
    service: &str,
    filter: &tracing_subscriber::EnvFilter,
) -> std::io::Result<impl tracing_subscriber::Layer<S>>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    std::fs::create_dir_all(dir)?;
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(service)
        .filename_suffix("log")
        .max_log_files(7)
        .build(dir)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(tracing_subscriber::fmt::layer()
        .with_writer(std::sync::Mutex::new(appender))
        .with_ansi(false)
        .with_filter(filter.clone()))
}

/// Build the rolling file layer, or fail fast: one stderr line naming the
/// service and the cause, then a non-zero exit. File mode has no fallback
/// channel; degrading to stdout would silently drop the events the
/// operator asked to persist.
fn build_file_layer<S>(
    dir: &Path,
    service: &str,
    filter: &tracing_subscriber::EnvFilter,
) -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    const LOG_INIT_FAILURE: i32 = 1;
    match try_build_file_layer(dir, service, filter) {
        Ok(layer) => layer,
        Err(e) => {
            eprintln!("kallipai: {service} file log init failed ({e}); refusing to start");
            std::process::exit(LOG_INIT_FAILURE);
        }
    }
}

/// Compose the service stack on top of the shared registry. File mode (a
/// resolved `dir`) makes the rolling file the only event layer: the stdout
/// writer then carries a single signpost line naming the service, the
/// active log file, and the level threshold. Stdout mode keeps the plain
/// formatted layer. The stdout writer is a parameter so tests can capture
/// it; production passes `std::io::stdout`.
fn service_layers<W>(
    filter: &tracing_subscriber::EnvFilter,
    service: &str,
    dir: Option<&Path>,
    stdout: W,
) -> impl tracing::Subscriber
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    let registry = tracing_subscriber::registry();
    match dir {
        None => {
            // The stdout arm keeps ANSI: it is the only channel and
            // terminals tolerate color, stated explicitly instead of
            // leaning on the fmt default.
            let stdout_layer = tracing_subscriber::fmt::layer()
                .with_writer(stdout)
                .with_ansi(true)
                .with_filter(filter.clone())
                .boxed();
            registry.with(stdout_layer)
        }
        Some(dir) => {
            let file_layer = build_file_layer(dir, service, filter).boxed();
            // One signpost line on the way in: where the events now go.
            // Plain text, and best-effort -- a broken stdout pipe must not
            // kill a service whose file channel is healthy.
            let mut out = stdout.make_writer();
            use std::io::Write as _;
            let _ = writeln!(
                out,
                "kallipai {service}: file logging active at {} (level threshold: {})",
                active_file(dir, service).display(),
                filter
            );
            registry.with(file_layer)
        }
    }
}

/// Install the process-wide subscriber. With a resolved `dir` the rolling
/// file is the only event channel and stdout carries the one signpost
/// line; without one, events go to stdout. The panic hook keeps its
/// default behavior, so panics reach stderr either way.
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

    use kallipai_testkit::DevDir;

    fn temp_dir(label: &str) -> DevDir {
        DevDir::new(&format!("logging-{label}"))
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
    fn civil_from_days_handles_month_ends_and_leap_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(30), (1970, 1, 31));
        assert_eq!(civil_from_days(58), (1970, 2, 28));
        assert_eq!(civil_from_days(789), (1972, 2, 29));
        assert_eq!(civil_from_days(1_095), (1972, 12, 31));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn utc_today_renders_a_zero_padded_iso_date() {
        let today = utc_today();
        let parts: Vec<&str> = today.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].len(), 4);
        assert_eq!(parts[1].len(), 2);
        assert_eq!(parts[2].len(), 2);
        assert!(parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())));
    }
    #[test]
    fn file_layer_writes_events_into_the_active_file() {
        let dir = temp_dir("writes");
        let layer = try_build_file_layer(&dir, "archeion", &test_filter())
            .expect("a creatable dir builds the file layer");
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            tracing::info!("kallipai logging marker event")
        });
        let path = active_file(&dir, "archeion");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("the active file {path:?} must exist: {e}"));
        assert!(
            text.contains("kallipai logging marker event"),
            "the marker event must land in the active file, got {text:?}"
        );
    }

    #[test]
    fn file_layer_fails_when_the_dir_cannot_be_created() {
        let dir = temp_dir("blocked");
        let blocker = dir.join("occupant");
        std::fs::write(&blocker, b"").unwrap();
        assert!(
            try_build_file_layer::<tracing_subscriber::Registry>(
                &blocker,
                "archeion",
                &test_filter()
            )
            .is_err()
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
    fn service_layers_file_mode_signposts_stdout_and_quiets_it() {
        let dir = temp_dir("file-mode");
        let captured = SharedBuf(Default::default());
        let subscriber = service_layers(&test_filter(), "archeion", Some(&dir), captured.clone());
        let stdout_before =
            String::from_utf8_lossy(&captured.0.lock().unwrap().clone()).into_owned();
        assert!(
            stdout_before.contains("kallipai archeion: file logging active at"),
            "the signpost line must reach stdout: {stdout_before:?}"
        );
        assert!(
            stdout_before.contains(active_file(&dir, "archeion").to_string_lossy().as_ref()),
            "the signpost must name the active file: {stdout_before:?}"
        );
        assert!(
            stdout_before.contains("level threshold: info"),
            "the signpost must carry the level threshold: {stdout_before:?}"
        );
        assert!(
            !stdout_before.contains(0x1b as char),
            "the signpost is plain text, got {stdout_before:?}"
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("kallipai file mode marker");
        });
        let stdout_after =
            String::from_utf8_lossy(&captured.0.lock().unwrap().clone()).into_owned();
        assert_eq!(
            stdout_before, stdout_after,
            "stdout must stay quiet after the signpost"
        );
        let file_text = std::fs::read_to_string(active_file(&dir, "archeion"))
            .unwrap_or_else(|e| panic!("the active file must exist: {e}"));
        assert!(
            file_text.contains("kallipai file mode marker"),
            "the file must carry the event: {file_text:?}"
        );
        assert!(
            !file_text.contains(0x1b as char),
            "the file must be ANSI-free, got {file_text:?}"
        );
    }

    #[test]
    fn service_layers_stdout_mode_keeps_events_on_stdout() {
        let captured = SharedBuf(Default::default());
        let subscriber = service_layers(&test_filter(), "archeion", None, captured.clone());
        assert_eq!(
            captured.0.lock().unwrap().len(),
            0,
            "stdout mode emits no signpost"
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("kallipai stdout mode marker");
        });
        let stdout_text = String::from_utf8_lossy(&captured.0.lock().unwrap().clone()).into_owned();
        assert!(
            stdout_text.contains("kallipai stdout mode marker"),
            "stdout stays the event channel: {stdout_text:?}"
        );
        assert!(
            stdout_text.contains(0x1b as char),
            "stdout keeps ANSI coloring"
        );
    }
}
