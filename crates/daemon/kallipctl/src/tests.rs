//! Unit tests for the parsing shape and the string helpers of this CLI.

use crate::print::error_prefix;
use kallipai_daemon_common::wire::ErrorCode;

use crate::print::adopt_line;
use crate::token::{read_operator_token_file, strip_trailing_newline, write_operator_token_file};
use kallipai_common::authtoken::OPERATOR_TOKEN_FILE;
use kallipai_daemon_common::wire::InstanceState;

#[test]
fn trailing_newline_stripped_lf_crlf_and_none() {
    assert_eq!(strip_trailing_newline("sk-x\n".into()), "sk-x");
    assert_eq!(strip_trailing_newline("sk-x\r\n".into()), "sk-x");
    assert_eq!(strip_trailing_newline("sk-x".into()), "sk-x");
    // Interior whitespace is preserved verbatim (the tool never
    // rewrites the operator's value).
    assert_eq!(strip_trailing_newline("sk-x  \n".into()), "sk-x  ");
}

#[test]
fn token_file_write_read_roundtrip_is_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let credentials = dir.path().join("credentials");
    write_operator_token_file(&credentials, "sk-operator-static").unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(credentials.join(OPERATOR_TOKEN_FILE))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    let secret = read_operator_token_file(&credentials.join(OPERATOR_TOKEN_FILE)).unwrap();
    assert_eq!(secret, "sk-operator-static");
}

#[test]
fn missing_and_valueless_token_files_get_operator_facing_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent.env");
    let err = read_operator_token_file(&path).unwrap_err().to_string();
    assert!(err.contains("no operator token file"), "{err}");
    assert!(err.contains("pinned"), "{err}");
    std::fs::write(&path, "OTHER=x\n").unwrap();
    let err = read_operator_token_file(&path).unwrap_err().to_string();
    assert!(err.contains("carries no KALLIPAI_OPERATOR_TOKEN"), "{err}");
}

#[test]
fn adopt_line_renders_the_registration_fact() {
    assert_eq!(
        adopt_line("team-a", InstanceState::Running),
        "adopted team-a (running)"
    );
    assert_eq!(
        adopt_line("team-b", InstanceState::Stopped),
        "adopted team-b (stopped)"
    );
}

#[test]
fn error_prefix_is_the_scripting_vocabulary() {
    assert_eq!(error_prefix(ErrorCode::NotFound), "no such instance");
    assert_eq!(error_prefix(ErrorCode::Denied), "not authorized");
    assert_eq!(
        error_prefix(ErrorCode::WorkspaceOverlap),
        "instance path overlap"
    );
}

mod service_logs_tests {
    use crate::cli::Cli;
    use crate::logs::{LogLevel, keep_level};
    use clap::Parser;

    #[test]
    fn keep_level_filters_on_the_level_word() {
        let lines = vec![
            "2026-09-27T10:00:00Z  INFO ready".to_string(),
            "2026-09-27T10:00:01Z ERROR boom".to_string(),
            "2026-09-27T10:00:02Z  WARN careful".to_string(),
        ];
        assert_eq!(keep_level(&lines, None).len(), 3);
        let errors = keep_level(&lines, Some(LogLevel::Error));
        assert_eq!(errors, vec!["2026-09-27T10:00:01Z ERROR boom".to_string()]);
        assert_eq!(keep_level(&lines, Some(LogLevel::Debug)).len(), 0);
    }

    #[test]
    fn logs_flag_sources_are_exclusive_and_bound_to_a_source() {
        // --file and --level without any source stay rejected: the
        // flags lock to their arm instead of falling through to the
        // listing shape.
        assert!(
            Cli::try_parse_from(["kallipctl", "logs", "--file", "x", "--level", "error",]).is_err()
        );
        // --file belongs to the instance arm.
        assert!(Cli::try_parse_from(["kallipctl", "logs", "myinstance", "--file", "x",]).is_ok());
        // --level belongs to the service arm.
        assert!(
            Cli::try_parse_from([
                "kallipctl",
                "logs",
                "--service",
                "archeion",
                "--level",
                "error",
            ])
            .is_ok()
        );
    }
}
