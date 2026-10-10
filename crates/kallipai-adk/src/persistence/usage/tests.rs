use std::fs;
use std::path::Path;

use kallipai_common::context::CumulativeUsage;

use super::read_cumulative_usage;

fn write_manifest(dir: &Path, usage: &str) {
    let doc = format!(
        "{{\"version\":1,\"conversation_turn_ids\":[],\"cumulative_usage\":{usage},\
         \"next_turn_id\":0,\"retry_log\":[]}}"
    );
    fs::write(dir.join("manifest.json"), doc).unwrap();
}

#[test]
fn reads_the_counters_from_a_manifest() {
    let dir = tempfile::tempdir().unwrap();
    write_manifest(
        dir.path(),
        "{\"prompt_tokens\":10,\"completion_tokens\":5,\
         \"cache_read_tokens\":2,\"cache_write_tokens\":1}",
    );
    assert_eq!(
        read_cumulative_usage(dir.path()),
        Some(CumulativeUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            cache_read_tokens: 2,
            cache_write_tokens: 1,
        })
    );
}

#[test]
fn a_missing_manifest_or_directory_is_none() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(read_cumulative_usage(dir.path()), None);
    assert_eq!(read_cumulative_usage(&dir.path().join("absent")), None);
}

#[test]
fn a_damaged_manifest_falls_back_to_its_backup() {
    let dir = tempfile::tempdir().unwrap();
    write_manifest(
        dir.path(),
        "{\"prompt_tokens\":7,\"completion_tokens\":0,\
         \"cache_read_tokens\":0,\"cache_write_tokens\":0}",
    );
    fs::copy(
        dir.path().join("manifest.json"),
        dir.path().join("manifest.json.bak"),
    )
    .unwrap();
    fs::write(dir.path().join("manifest.json"), "{wreck").unwrap();
    assert_eq!(
        read_cumulative_usage(dir.path()).map(|u| u.prompt_tokens),
        Some(7)
    );
}

#[test]
fn damage_in_both_copies_is_none() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("manifest.json"), "{wreck").unwrap();
    fs::write(dir.path().join("manifest.json.bak"), "{wreck").unwrap();
    assert_eq!(read_cumulative_usage(dir.path()), None);
}
