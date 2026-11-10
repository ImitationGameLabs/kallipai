//! Display shapes and the evidence rules of the spawn-refusal errors.

use super::*;

fn spawn_failed(stage: SpawnStage, path: Option<&str>) -> ShellError {
    ShellError::SpawnFailed {
        stage,
        path: path.map(PathBuf::from),
        detail: "permission denied (os error 13)".to_owned(),
    }
}

fn recorded(stage: Option<SpawnStage>, path: Option<&str>) -> Option<RecordedRefusal> {
    Some((
        stage,
        path.map(PathBuf::from),
        "mount: permission denied".to_owned(),
    ))
}

/// A directory that exists and can be entered, so the parent-side checks pass.
fn usable_cwd() -> PathBuf {
    std::env::temp_dir()
}

#[test]
fn spawn_failed_names_step_and_path() {
    let error = spawn_failed(SpawnStage::Bind, Some("/work/team/researcher"));
    assert_eq!(
        error.to_string(),
        "spawn failed at bind for /work/team/researcher: permission denied (os error 13)"
    );
}

#[test]
fn spawn_failed_without_path_leaks_no_placeholder() {
    let error = spawn_failed(SpawnStage::WorkingDirectory, None);
    assert_eq!(
        error.to_string(),
        "spawn failed at working directory: permission denied (os error 13)"
    );
    assert!(!error.to_string().contains("None"));
}

#[test]
fn a_ruleset_refusal_says_nothing_was_filtered_and_names_no_path() {
    let error = ShellError::Landlock {
        detail: "ruleset has no fd".to_owned(),
    };
    assert_eq!(
        error.to_string(),
        "landlock refused the spawn: ruleset has no fd"
    );
    assert!(!error.to_string().contains("None"));
}

#[test]
fn a_recorded_refusal_outranks_parent_side_evidence() {
    // The cwd does not exist, so the parent-side check would say
    // "working directory"; the child's own record is the better fact.
    let error = explain_spawn(
        std::io::Error::from_raw_os_error(libc::ENOENT),
        Path::new("/no/such/cwd"),
        std::ffi::OsStr::new("bash"),
        recorded(Some(SpawnStage::Bind), Some("/work/team/researcher")),
    );
    match &error {
        ShellError::SpawnFailed {
            stage,
            path,
            detail,
        } => {
            assert_eq!(*stage, SpawnStage::Bind);
            assert_eq!(path.as_deref(), Some(Path::new("/work/team/researcher")));
            assert_eq!(detail, "mount: permission denied");
        }
        other => panic!("expected a recorded step, got {other:?}"),
    }
}

#[test]
fn a_recorded_refusal_without_a_step_is_the_ruleset() {
    let error = explain_spawn(
        std::io::Error::from_raw_os_error(libc::EPERM),
        &usable_cwd(),
        std::ffi::OsStr::new("bash"),
        recorded(None, None),
    );
    assert!(
        matches!(error, ShellError::Landlock { .. }),
        "got {error:?}"
    );
}

#[test]
fn a_working_directory_that_is_a_regular_file_is_the_working_directory() {
    let scratch = tempfile::tempdir().expect("scratch");
    let file = scratch.path().join("not-a-directory");
    std::fs::write(&file, b"").expect("write file");
    // 0755 passes the search-permission probe: only `is_dir` separates this
    // from a usable working directory, and `chdir` answers ENOTDIR.
    std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("chmod");
    let error = explain_spawn(
        std::io::Error::from_raw_os_error(libc::ENOTDIR),
        &file,
        std::ffi::OsStr::new("bash"),
        None,
    );
    match &error {
        ShellError::SpawnFailed { stage, path, .. } => {
            assert_eq!(*stage, SpawnStage::WorkingDirectory);
            assert_eq!(path.as_deref(), Some(file.as_path()));
        }
        other => panic!("expected the working directory, got {other:?}"),
    }
}

#[test]
fn a_relative_program_missing_from_the_path_is_the_exec() {
    // The default shell is the relative name "bash": this is the shape the
    // production configuration produces on a PATH without it.
    let error = explain_spawn(
        std::io::Error::from_raw_os_error(libc::ENOENT),
        &usable_cwd(),
        std::ffi::OsStr::new("kallipai-no-such-shell"),
        None,
    );
    match &error {
        ShellError::SpawnFailed { stage, path, .. } => {
            assert_eq!(*stage, SpawnStage::Exec);
            assert_eq!(path.as_deref(), Some(Path::new("kallipai-no-such-shell")));
        }
        other => panic!("expected the exec step, got {other:?}"),
    }
}

#[test]
fn a_relative_program_that_may_not_be_executed_is_the_exec() {
    let error = explain_spawn(
        std::io::Error::from_raw_os_error(libc::EACCES),
        &usable_cwd(),
        std::ffi::OsStr::new("kallipai-locked-shell"),
        None,
    );
    match &error {
        ShellError::SpawnFailed { stage, .. } => assert_eq!(*stage, SpawnStage::Exec),
        other => panic!("expected the exec step, got {other:?}"),
    }
}

#[test]
fn an_absolute_program_outside_a_searchable_path_is_the_exec() {
    let scratch = tempfile::tempdir().expect("scratch");
    let shell = scratch.path().join("missing-shell");
    let error = explain_spawn(
        std::io::Error::from_raw_os_error(libc::ENOENT),
        &usable_cwd(),
        shell.as_os_str(),
        None,
    );
    match &error {
        ShellError::SpawnFailed { stage, path, .. } => {
            assert_eq!(*stage, SpawnStage::Exec);
            assert_eq!(path.as_deref(), Some(shell.as_path()));
        }
        other => panic!("expected the exec step, got {other:?}"),
    }
}

#[test]
fn an_absolute_program_that_is_a_directory_is_the_exec() {
    let scratch = tempfile::tempdir().expect("scratch");
    // A directory passes the search-permission probe; only `is_file` tells it
    // apart from a program the kernel could exec.
    let error = explain_spawn(
        std::io::Error::from_raw_os_error(libc::EACCES),
        &usable_cwd(),
        scratch.path().as_os_str(),
        None,
    );
    match &error {
        ShellError::SpawnFailed { stage, .. } => assert_eq!(*stage, SpawnStage::Exec),
        other => panic!("expected the exec step, got {other:?}"),
    }
}

#[test]
fn a_failure_without_evidence_stays_io() {
    // Resource exhaustion reaches neither the working directory nor the
    // program, and no child record exists: naming any step here would be a
    // guess.
    let error = explain_spawn(
        std::io::Error::from_raw_os_error(libc::EMFILE),
        &usable_cwd(),
        std::ffi::OsStr::new("bash"),
        None,
    );
    assert!(matches!(error, ShellError::Io(_)), "got {error:?}");
    assert!(error.to_string().starts_with("I/O error: "));
}

#[test]
fn every_stage_has_its_own_wire_code() {
    for stage in SpawnStage::WIRE {
        let code = stage.code().expect("every stage codes");
        assert_eq!(SpawnStage::from_code(code), Some(stage));
    }
    let codes: Vec<u8> = SpawnStage::WIRE
        .iter()
        .map(|stage| stage.code().expect("every stage codes"))
        .collect();
    let mut unique = codes.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), codes.len(), "codes collide: {codes:?}");
    // Codes are positions plus one, so zero is "no step" and the code past the
    // table is unknown; neither may alias a stage.
    assert_eq!(SpawnStage::from_code(0), None);
    assert_eq!(SpawnStage::from_code(codes.len() as u8 + 1), None);
}
