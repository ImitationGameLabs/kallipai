//! The `log` bridge: `init_service_logging` must route `log` records into
//! the tracing subscriber, not only install the subscriber itself. The
//! install is process-global, so this guard lives in its own test binary.

#![cfg(feature = "logging")]

use kallipai_testkit::DevDir;

#[test]
fn init_service_logging_routes_log_records_into_the_file_channel() {
    let dir = DevDir::new("log-bridge");
    let filter = tracing_subscriber::EnvFilter::new("info");

    kallipai_common::logging::init_service_logging(&filter, "common", Some(dir.path()));

    assert_ne!(
        log::max_level(),
        log::LevelFilter::Off,
        "init_service_logging must install the log compatibility layer"
    );

    // The marker reaches the file only when the bridge forwarded it.
    log::warn!("kallipai log bridge marker");

    let text = read_active_log(&dir);
    assert!(
        text.contains("kallipai log bridge marker"),
        "the log record must reach the file channel, got {text:?}"
    );
}

/// The single rolling file the file layer writes under `dir`.
fn read_active_log(dir: &DevDir) -> String {
    let files: Vec<_> = std::fs::read_dir(dir)
        .expect("the log directory must be readable")
        .map(|entry| entry.expect("a readable directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "log"))
        .collect();
    assert_eq!(files.len(), 1, "exactly one rolling file, got {files:?}");
    std::fs::read_to_string(&files[0]).expect("the active log file must be readable")
}
