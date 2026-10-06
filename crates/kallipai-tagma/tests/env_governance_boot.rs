//! Boot-time env governance at the real-binary level: a retired relay
//! variable fails the boot loudly (the same treatment a misspelled name
//! gets) instead of being silently ignored. The scrub's agent-side effect
//! (an empty `printenv` inside an agent bash) is asserted by the sandbox
//! normal scenario, which boots the tagma with the operator token in its
//! environment; this file covers the boot-fail shape alone.

use std::path::PathBuf;
use std::process::Command;

fn tagma_bin() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_kallipai-tagma")
        .map(PathBuf::from)
        .expect("cargo injects CARGO_BIN_EXE_kallipai-tagma for same-package tests")
}

fn boot_with(extra_env: &[(&str, &str)]) -> std::process::Output {
    let home = tempfile::tempdir().unwrap();
    Command::new(tagma_bin())
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        // Instance roots resolve before the governance check in `main`;
        // a valid slug gets the boot past them and into the check.
        .env("KALLIPAI_TAGMA_SLUG", "envgov-boot-test")
        .envs(extra_env.iter().copied())
        .output()
        .expect("spawn kallipai-tagma")
}

#[test]
fn a_retired_relay_variable_fails_the_boot_naming_it() {
    let out = boot_with(&[("KALLIPAI_TAGMA_RELAY_ARCHEION_URL", "https://old.example")]);
    assert!(
        !out.status.success(),
        "a retired variable is a configuration error and must fail the boot"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("KALLIPAI_TAGMA_RELAY_ARCHEION_URL"),
        "the retired name must appear in the error: {stderr}"
    );
    assert!(
        stderr.contains("KALLIPAI_POLIS_URL"),
        "the remediation must point at the origin: {stderr}"
    );
}

#[test]
fn both_retired_relay_variables_are_named_together() {
    let out = boot_with(&[
        ("KALLIPAI_TAGMA_RELAY_ARCHEION_URL", "https://old.example"),
        ("KALLIPAI_TAGMA_RELAY_LESCHE_URL", "https://old.example"),
    ]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("KALLIPAI_TAGMA_RELAY_ARCHEION_URL")
            && stderr.contains("KALLIPAI_TAGMA_RELAY_LESCHE_URL"),
        "every retired name must appear: {stderr}"
    );
}
