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

mod shape_tests {
    use crate::cli::Cli;
    use clap::Parser;
    use clap::error::ErrorKind;

    #[test]
    fn logs_is_instance_only() {
        // The verb names one instance; a bare call names nothing and is
        // rejected.
        let err = Cli::try_parse_from(["kallipctl", "logs"])
            .err()
            .expect("the bare verb names no instance");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument, "{err}");

        let err = Cli::try_parse_from(["kallipctl", "logs", "--service", "archeion"])
            .err()
            .expect("--service is not in the command's vocabulary");
        assert_eq!(err.kind(), ErrorKind::UnknownArgument, "{err}");

        assert!(Cli::try_parse_from(["kallipctl", "logs", "myinstance", "--file", "x"]).is_ok());
    }

    #[test]
    fn blobs_rewrite_accepts_instance_stores_only() {
        // Every store belongs to an instance record: --slug is mandatory
        // and `files` is not one of the targets.
        let err = Cli::try_parse_from(["kallipctl", "blobs", "rewrite", "--target", "tasks"])
            .err()
            .expect("the slug is not optional");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument, "{err}");

        let err = Cli::try_parse_from(["kallipctl", "blobs", "rewrite", "--target", "files", "a"])
            .err()
            .expect("files is not a target");
        assert_eq!(err.kind(), ErrorKind::InvalidValue, "{err}");
    }

    #[test]
    fn a_minted_code_is_named_in_the_failure_note() {
        let minted = crate::archeion::MintedCode {
            code: "KLP-123".to_owned(),
            id: "tgm_42".to_owned(),
            expires_at: "2026-11-02T00:30:00Z".to_owned(),
        };
        let err = crate::pending_note(Some(&minted), "spawn failed".to_owned());
        let text = format!("{err:#}");
        assert!(text.starts_with("spawn failed"), "{text}");
        assert!(text.contains("tgm_42"), "{text}");
        assert!(text.contains("2026-11-02T00:30:00Z"), "{text}");
        assert!(text.contains("revoke"), "{text}");
        // The code is a credential until it is redeemed.
        assert!(!text.contains("KLP-123"), "{text}");
    }

    #[test]
    fn a_failure_without_a_mint_keeps_its_message() {
        let err = crate::pending_note(None, "cannot reach archeion".to_owned());
        assert_eq!(format!("{err:#}"), "cannot reach archeion");
    }

    #[test]
    fn a_relative_workspace_resolves_against_the_given_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        let resolved = crate::absolute_workspace_in("nested", dir.path()).expect("resolves");
        assert_eq!(
            resolved,
            dir.path().join("nested").to_string_lossy().into_owned()
        );
    }

    #[test]
    fn a_workspace_that_is_not_a_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let err = crate::absolute_workspace_in("missing", dir.path()).expect_err("refused");
        let text = format!("{err:#}");
        assert!(text.contains("is not an existing directory"), "{text}");
        assert!(text.contains(&dir.path().display().to_string()), "{text}");
    }
}
