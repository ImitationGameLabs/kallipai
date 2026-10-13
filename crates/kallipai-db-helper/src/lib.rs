//! Shared database connection options for kallipai services.

use std::time::Duration;

use sea_orm::ConnectOptions;

/// Build SeaORM connect options with the workspace SQL logging tiers.
///
/// Successful statements log at DEBUG, statements slower than `sql_slow` log
/// at WARN. Failure logging is the caller's responsibility: this helper
/// only fixes the statement tiers so every service reports SQL timing
/// the same way.
pub fn connect_options(url: &str, sql_slow: Duration) -> ConnectOptions {
    let mut options = ConnectOptions::new(url.to_owned());
    options
        .sqlx_logging(true)
        .sqlx_logging_level(log::LevelFilter::Debug)
        .sqlx_slow_statements_logging_settings(log::LevelFilter::Warn, sql_slow);
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logging_tiers_follow_the_slow_threshold() {
        for threshold in [
            Duration::from_millis(50),
            Duration::from_millis(250),
            Duration::from_secs(1),
            Duration::from_secs(5),
        ] {
            let options = connect_options("sqlite::memory:", threshold);
            assert!(options.get_sqlx_logging());
            assert_eq!(options.get_sqlx_logging_level(), log::LevelFilter::Debug);
            assert_eq!(
                options.get_sqlx_slow_statements_logging_settings(),
                (log::LevelFilter::Warn, threshold)
            );
        }
    }
}
