//! The prune rules, driven through the public interface: real files under a
//! temporary root, with modification times pinned to exact instants so the age
//! boundary is tested on both sides rather than approximated.
use kallipai_telemetry_prune::{
    MAX_AGE, MAX_AGE_HOURS, MAX_BYTES, Policy, UNIT_PARENTS, Unit, apply, default_root,
    existing_root, plan, render, render_applied, scan,
};
use kallipai_testkit::DevDir;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

fn hours(n: u64) -> Duration {
    Duration::from_secs(n * 60 * 60)
}

/// Pin a path's mtime exactly; `std::fs::FileTimes` needs no extra dependency.
fn set_mtime(path: &Path, at: SystemTime) {
    let file = fs::File::open(path).expect("open for set_times");
    file.set_times(fs::FileTimes::new().set_modified(at))
        .expect("set mtime");
}

/// A telemetry root under the test guard, with both unit parents created.
fn root(dev: &DevDir) -> PathBuf {
    let root = dev.path().join("telemetry");
    for parent in UNIT_PARENTS {
        fs::create_dir_all(root.join(parent)).expect("create unit parent");
    }
    fs::create_dir_all(root.join("grafana")).expect("create grafana dir");
    root
}

/// Write `bytes` bytes at `<root>/<relative>`, then stamp the file and every
/// directory above it with `at` (creating a file bumps its parent's mtime).
fn put(root: &Path, relative: &str, bytes: usize, at: SystemTime) -> PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("parent")).expect("create parents");
    fs::write(&path, vec![b'x'; bytes]).expect("write payload");
    set_mtime(&path, at);
    let mut dir = path.parent();
    while let Some(current) = dir {
        if current == root {
            set_mtime(current, at);
            break;
        }
        set_mtime(current, at);
        dir = current.parent();
    }
    path
}

/// File names of the units a rule picked, in the order it picked them.
fn names(units: &[Unit]) -> Vec<String> {
    units
        .iter()
        .map(|unit| {
            unit.path
                .file_name()
                .expect("a unit names a file")
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// The same, spelled relative to the root: what a report prints.
fn relative(root: &Path, units: &[Unit]) -> Vec<String> {
    units
        .iter()
        .map(|unit| {
            unit.path
                .strip_prefix(root)
                .expect("under the root")
                .display()
                .to_string()
        })
        .collect()
}

#[test]
fn the_root_prefers_xdg_data_home() {
    let resolved = default_root(Some("/xdg"), Some("/home/u")).expect("resolve");
    assert_eq!(resolved, PathBuf::from("/xdg/kallipai/telemetry"));
}

#[test]
fn the_root_falls_back_to_home() {
    let resolved = default_root(None, Some("/home/u")).expect("resolve");
    assert_eq!(
        resolved,
        PathBuf::from("/home/u/.local/share/kallipai/telemetry")
    );
    // An empty value is the same as unset: direnv and systemd both leave
    // XDG_DATA_HOME as an empty string.
    let resolved = default_root(Some(""), Some("/home/u")).expect("resolve");
    assert_eq!(
        resolved,
        PathBuf::from("/home/u/.local/share/kallipai/telemetry")
    );
}

#[test]
fn the_root_refuses_an_unset_or_relative_environment() {
    assert!(default_root(None, None).is_err());
    assert!(default_root(Some(""), Some("")).is_err());
    assert!(default_root(Some("relative/xdg"), None).is_err());
    assert!(default_root(None, Some("relative/home")).is_err());
}

#[test]
fn the_budgets_are_the_documented_ones_and_env_can_move_only_them() {
    let default = Policy::default();
    assert_eq!(default.max_bytes, MAX_BYTES);
    assert_eq!(default.max_age, MAX_AGE);
    assert_eq!(MAX_AGE, hours(MAX_AGE_HOURS));

    let moved = Policy::from_env(|key| match key {
        "KALLIPAI_TELEMETRY_MAX_BYTES" => Some("1024".into()),
        "KALLIPAI_TELEMETRY_MAX_AGE_HOURS" => Some("1".into()),
        _ => None,
    })
    .expect("parse");
    assert_eq!(moved.max_bytes, 1024);
    assert_eq!(moved.max_age, hours(1));

    assert!(
        Policy::from_env(|key| (key == "KALLIPAI_TELEMETRY_MAX_BYTES").then(|| "lots".into()))
            .is_err()
    );
}

#[test]
fn the_scan_counts_grafana_in_the_total_but_never_as_a_unit() {
    let dev = DevDir::new("prune-scan");
    let root = root(&dev);
    let now = SystemTime::now();
    put(&root, "tempo/blocks/block-a/meta.json", 100, now - hours(1));
    put(&root, "tempo/wal/seg-1", 50, now - hours(1));
    put(&root, "grafana/grafana.db", 400, now - hours(1));

    let scanned = scan(&root).expect("scan");
    assert_eq!(scanned.units.len(), 2, "only tempo children are units");
    assert_eq!(
        scanned.total_bytes, 550,
        "the total counts grafana's bytes too"
    );
    let unit_names = relative(&root, &scanned.units);
    assert!(unit_names.contains(&"tempo/blocks/block-a".to_string()));
    assert!(unit_names.contains(&"tempo/wal/seg-1".to_string()));
    assert!(scanned.refusals.is_empty());
}

#[test]
fn the_scan_orders_units_oldest_first() {
    let dev = DevDir::new("prune-order");
    let root = root(&dev);
    let now = SystemTime::now();
    put(&root, "tempo/blocks/young", 10, now - hours(2));
    put(&root, "tempo/blocks/old", 10, now - hours(40));
    put(&root, "tempo/blocks/middle", 10, now - hours(20));

    let scanned = scan(&root).expect("scan");
    assert_eq!(names(&scanned.units), ["old", "middle", "young"]);
}

#[test]
fn the_scan_refuses_a_symlinked_unit_and_leaves_its_target_alone() {
    let dev = DevDir::new("prune-symlink");
    let root = root(&dev);
    let now = SystemTime::now();
    put(&root, "tempo/blocks/block-a/meta.json", 10, now - hours(90));
    let outside = dev.path().join("outside");
    fs::create_dir_all(&outside).expect("outside dir");
    fs::write(outside.join("precious"), b"keep me").expect("payload");
    std::os::unix::fs::symlink(&outside, root.join("tempo/blocks/linked")).expect("symlink");

    let scanned = scan(&root).expect("scan");
    assert_eq!(scanned.units.len(), 1, "the symlink is not a unit");
    assert_eq!(scanned.refusals.len(), 1);
    // The reason string is for the operator, not a contract: what this test
    // pins is the behaviour above -- one unit, and a target nobody touches.

    let planned = plan(&scanned, now, &Policy::default());
    apply(&scanned, &planned).expect("apply");
    assert!(
        outside.join("precious").exists(),
        "a symlink target is never deleted"
    );
}

#[test]
fn the_age_rule_deletes_past_the_boundary_and_keeps_inside_it() {
    let dev = DevDir::new("prune-age");
    let root = root(&dev);
    let now = SystemTime::now();
    put(
        &root,
        "tempo/blocks/just-over",
        10,
        now - MAX_AGE - Duration::from_secs(60),
    );
    put(
        &root,
        "tempo/blocks/just-under",
        10,
        now - MAX_AGE + Duration::from_secs(60),
    );

    let scanned = scan(&root).expect("scan");
    let planned = plan(&scanned, now, &Policy::default());
    assert_eq!(names(&planned.deletions), ["just-over"]);
    assert_eq!(planned.kept_units, 1);
}

#[test]
fn the_size_rule_deletes_the_oldest_units_until_the_total_fits() {
    let dev = DevDir::new("prune-size");
    let root = root(&dev);
    let now = SystemTime::now();
    // Young enough that only the size rule can fire.
    put(&root, "tempo/blocks/a", 100, now - hours(1));
    put(&root, "tempo/blocks/b", 100, now - hours(2));
    put(&root, "tempo/blocks/c", 100, now - hours(3));
    put(&root, "grafana/grafana.db", 400, now - hours(1));

    let policy = Policy {
        max_bytes: 500,
        max_age: MAX_AGE,
    };
    let scanned = scan(&root).expect("scan");
    assert_eq!(scanned.total_bytes, 700);
    let planned = plan(&scanned, now, &policy);
    // Total 700 over the 500 budget: the two oldest units (c, b) go, which
    // leaves 700 - 200 = 500 -- exactly the budget, so the walk stops.
    assert_eq!(names(&planned.deletions), ["c", "b"]);
    assert_eq!(planned.total_after, 500);
    assert_eq!(planned.freed_bytes, 200);
}

#[test]
fn the_two_rules_never_delete_the_same_unit_twice() {
    let dev = DevDir::new("prune-both");
    let root = root(&dev);
    let now = SystemTime::now();
    put(&root, "tempo/blocks/old", 100, now - hours(100));
    put(&root, "tempo/blocks/new", 100, now - hours(1));

    let policy = Policy {
        max_bytes: 150,
        max_age: MAX_AGE,
    };
    let scanned = scan(&root).expect("scan");
    let planned = plan(&scanned, now, &policy);
    // The old unit is over the age bound; deleting it also brings 200 down to
    // 100, inside the size bound -- so the young unit survives.
    assert_eq!(planned.deletions.len(), 1);
    assert_eq!(
        planned.deletions[0].path.file_name().unwrap(),
        std::ffi::OsStr::new("old")
    );
    assert_eq!(planned.kept_units, 1);
}

#[test]
fn a_run_without_apply_leaves_the_tree_untouched() {
    let dev = DevDir::new("prune-dry");
    let root = root(&dev);
    let now = SystemTime::now();
    let victim = put(&root, "tempo/blocks/stale/meta.json", 10, now - hours(200));

    let scanned = scan(&root).expect("scan");
    let planned = plan(&scanned, now, &Policy::default());
    let report = render(&scanned, &planned, &Policy::default(), now, true);
    assert!(report.contains("dry run: nothing deleted"));
    assert!(report.contains("tempo/blocks/stale"));
    assert!(victim.exists(), "a dry run deletes nothing");
}

#[test]
fn apply_deletes_exactly_the_plan_and_keeps_the_parents() {
    let dev = DevDir::new("prune-apply");
    let root = root(&dev);
    let now = SystemTime::now();
    let stale = put(&root, "tempo/blocks/stale/meta.json", 10, now - hours(200));
    let fresh = put(&root, "tempo/blocks/fresh/meta.json", 10, now - hours(1));
    let wal = put(&root, "tempo/wal/00000001", 10, now - hours(200));
    let grafana = put(&root, "grafana/grafana.db", 10, now - hours(200));

    let scanned = scan(&root).expect("scan");
    let planned = plan(&scanned, now, &Policy::default());
    let applied = apply(&scanned, &planned).expect("apply");

    assert_eq!(applied.deleted, planned.deletions.len());
    assert!(applied.skipped.is_empty());
    assert!(!stale.exists());
    assert!(!wal.exists());
    assert!(fresh.exists(), "a young unit survives");
    assert!(grafana.exists(), "grafana is counted, never pruned");
    assert!(root.join("tempo/blocks").exists(), "unit parents survive");
    assert!(root.join("tempo/wal").exists());
}

#[test]
fn apply_skips_a_unit_that_became_a_symlink_after_the_scan() {
    let dev = DevDir::new("prune-race");
    let root = root(&dev);
    let now = SystemTime::now();
    let unit = root.join("tempo/blocks/swap");
    fs::create_dir_all(&unit).expect("unit dir");
    fs::write(unit.join("meta.json"), b"payload").expect("payload");
    set_mtime(&unit, now - hours(200));

    let scanned = scan(&root).expect("scan");
    let planned = plan(&scanned, now, &Policy::default());
    assert_eq!(planned.deletions.len(), 1);

    // Swap the unit for a symlink between scan and apply.
    let outside = dev.path().join("elsewhere");
    fs::create_dir_all(&outside).expect("outside");
    fs::remove_dir_all(&unit).expect("remove unit");
    std::os::unix::fs::symlink(&outside, &unit).expect("symlink");

    let applied = apply(&scanned, &planned).expect("apply");
    assert_eq!(applied.deleted, 0, "a symlink is skipped, not followed");
    assert_eq!(applied.skipped.len(), 1);
    assert!(outside.exists(), "and its target is untouched");
}

#[test]
fn the_report_states_the_budgets_the_totals_and_the_refusals() {
    let dev = DevDir::new("prune-report");
    let root = root(&dev);
    let now = SystemTime::now();
    put(
        &root,
        "tempo/blocks/stale/meta.json",
        2 * 1024 * 1024,
        now - hours(200),
    );
    std::os::unix::fs::symlink(dev.path(), root.join("tempo/blocks/link")).expect("symlink");

    let policy = Policy::default();
    let scanned = scan(&root).expect("scan");
    let planned = plan(&scanned, now, &policy);
    let report = render(&scanned, &planned, &policy, now, false);

    assert!(report.contains("budget 5.0 GiB"), "report: {report}");
    assert!(report.contains("max age 72h"), "report: {report}");
    assert!(
        report.contains("delete tempo/blocks/stale"),
        "report: {report}"
    );
    assert!(report.contains("refused"), "report: {report}");
    assert!(
        !report.contains("dry run"),
        "an apply run says so by omission"
    );
}

#[test]
fn apply_skips_a_unit_that_vanished_after_the_scan() {
    let dev = DevDir::new("prune-vanished");
    let root = root(&dev);
    let now = SystemTime::now();
    let retired = put(
        &root,
        "tempo/blocks/retired/meta.json",
        10,
        now - hours(200),
    );
    let kept = put(&root, "tempo/wal/00000001", 10, now - hours(200));

    let scanned = scan(&root).expect("scan");
    let planned = plan(&scanned, now, &Policy::default());
    assert_eq!(planned.deletions.len(), 2);

    // Tempo retires one block on its own between the scan and the delete.
    let retired_unit = retired.parent().expect("unit dir");
    fs::remove_dir_all(retired_unit).expect("retire one unit");

    let applied = apply(&scanned, &planned).expect("a retired unit is not a failure");
    assert_eq!(
        applied.deleted, 1,
        "the surviving plan entry is still deleted"
    );
    assert_eq!(applied.skipped.len(), 1);
    assert!(!kept.exists());
    assert!(!retired.exists());

    let report = render_applied(&scanned, &applied, 2);
    assert!(
        report.starts_with("deleted 1 of 2 planned units, skipped 1"),
        "report: {report}"
    );
    assert!(
        report.contains("skipped tempo/blocks/retired"),
        "a skipped unit is named, relative to the root: {report}"
    );
}

#[test]
fn an_absent_root_is_a_no_op() {
    let dev = DevDir::new("prune-absent");
    let root = dev.path().join("never-written");

    assert_eq!(existing_root(&root).expect("look the root up"), None);
}

#[test]
fn a_root_link_that_leads_nowhere_is_an_error() {
    let dev = DevDir::new("prune-root-dangling");
    let link = dev.path().join("telemetry");
    std::os::unix::fs::symlink(dev.path().join("gone"), &link).expect("symlink the root");

    // The stack writes through a link, so a target that is gone is a broken
    // setup, not a machine the stack has never run on.
    let error = existing_root(&link).expect_err("a link to nothing is not a no-op");
    assert!(
        error.to_string().contains("leads nowhere"),
        "error: {error}"
    );
}

#[test]
fn a_root_that_is_not_a_directory_is_an_error() {
    let dev = DevDir::new("prune-not-a-dir");
    let root = dev.path().join("telemetry");
    fs::write(&root, b"a file, not a directory").expect("write a file at the root path");

    let error = existing_root(&root).expect_err("a file is not a root");
    assert!(
        error.to_string().contains("is not a directory"),
        "error: {error}"
    );
}

#[test]
fn the_binary_reports_an_absent_root_as_a_no_op() {
    let dev = DevDir::new("prune-bin-noop");
    let xdg = dev.path().join("xdg");
    fs::create_dir_all(&xdg).expect("create xdg dir");

    let output = Command::new(env!("CARGO_BIN_EXE_kallipai-telemetry-prune"))
        .env_clear()
        .env("XDG_DATA_HOME", &xdg)
        .output()
        .expect("run the pruner");

    assert!(output.status.success(), "status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("does not exist; nothing to prune"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains(&xdg.display().to_string()),
        "the line names the root: {stdout}"
    );
}

#[test]
fn the_scan_refuses_units_that_resolve_outside_the_root() {
    let dev = DevDir::new("prune-escape");
    let root = root(&dev);
    let now = SystemTime::now();
    // Replace a unit parent with a symlink to a directory outside the root.
    // `read_dir` follows it, and the entries under it are ordinary directories
    // whose own metadata says nothing about the escape: only the canonical
    // check can tell they are outside, which is why this branch exists.
    let outside = dev.path().join("elsewhere");
    fs::create_dir_all(outside.join("segment")).expect("outside segment");
    fs::write(outside.join("segment/payload"), vec![b'x'; 10]).expect("payload");
    set_mtime(&outside.join("segment/payload"), now - hours(200));
    fs::remove_dir_all(root.join("tempo/wal")).expect("drop the real wal dir");
    std::os::unix::fs::symlink(&outside, root.join("tempo/wal")).expect("symlink");

    let scanned = scan(&root).expect("scan");
    assert!(scanned.units.is_empty(), "an escaped child is never a unit");
    assert_eq!(scanned.refusals.len(), 1);

    let planned = plan(&scanned, now, &Policy::default());
    assert!(planned.deletions.is_empty());
    let applied = apply(&scanned, &planned).expect("apply");
    assert_eq!(applied.deleted, 0);
    assert!(
        outside.join("segment/payload").exists(),
        "nothing outside the root is touched"
    );
}

#[test]
fn a_root_that_is_a_symlink_to_a_directory_is_scanned_through() {
    let dev = DevDir::new("prune-root-link");
    let real = root(&dev);
    let now = SystemTime::now();
    put(&real, "tempo/blocks/stale/meta.json", 10, now - hours(200));
    let link = dev.path().join("telemetry-link");
    std::os::unix::fs::symlink(&real, &link).expect("symlink the root");

    // Docker follows such a link when it binds the data directory, so a run
    // that refused to look through it would never prune what the stack writes.
    let resolved = existing_root(&link)
        .expect("a symlink to a directory is a root")
        .expect("the root exists");
    assert_eq!(resolved, link);

    let scanned = scan(&link).expect("scan through the link");
    assert_eq!(
        scanned.units.len(),
        1,
        "the units behind the link are found"
    );
    assert!(
        scanned.total_bytes >= 10,
        "and their bytes are counted: {}",
        scanned.total_bytes
    );

    let planned = plan(&scanned, now, &Policy::default());
    let applied = apply(&scanned, &planned).expect("apply");
    assert_eq!(applied.deleted, 1);
    assert!(!real.join("tempo/blocks/stale").exists());
}

#[test]
fn apply_skips_a_unit_that_changed_shape_after_the_scan() {
    let dev = DevDir::new("prune-shape");
    let root = root(&dev);
    let now = SystemTime::now();
    let unit = put(&root, "tempo/blocks/swap/meta.json", 10, now - hours(200));
    let unit = unit.parent().expect("unit dir").to_path_buf();

    let scanned = scan(&root).expect("scan");
    let planned = plan(&scanned, now, &Policy::default());
    assert_eq!(planned.deletions.len(), 1);

    // The occupant replaces the whole block directory with a file of the same
    // name between the scan and the delete.
    fs::remove_dir_all(&unit).expect("remove the directory");
    fs::write(&unit, b"a file now").expect("a file takes its place");

    let applied = apply(&scanned, &planned).expect("apply");
    assert_eq!(
        applied.deleted, 0,
        "a different shape is not the planned unit"
    );
    assert_eq!(applied.skipped.len(), 1);
    assert!(unit.exists(), "and it is left for the next run to look at");
}

#[test]
fn an_unreachable_budget_stands_the_size_rule_down() {
    let dev = DevDir::new("prune-unreachable");
    let root = root(&dev);
    let now = SystemTime::now();
    // Grafana's state is counted and never pruned: on its own it is over the
    // budget, so no amount of deleting reaches it.
    put(&root, "grafana/grafana.db", 4000, now - hours(1));
    put(&root, "tempo/blocks/young", 100, now - hours(1));
    put(&root, "tempo/blocks/older", 100, now - hours(20));
    // Over the age horizon, so the age rule wants it whatever the byte budget
    // says. A young-only fixture cannot tell "the size rule stood down" from
    // "an empty plan stands down".
    put(&root, "tempo/wal/stale", 100, now - MAX_AGE - hours(1));

    let policy = Policy {
        max_bytes: 1000,
        max_age: MAX_AGE,
    };
    let scanned = scan(&root).expect("scan");
    let planned = plan(&scanned, now, &policy);

    assert!(planned.budget_unreachable);
    assert_eq!(planned.stranded_bytes, 4000);
    assert_eq!(
        planned.deletions.len(),
        1,
        "only the age rule's unit goes; the size rule stood down"
    );

    let report = render(&scanned, &planned, &policy, now, true);
    assert!(
        report.contains("budget unreachable: 3.9 KiB cannot be pruned"),
        "report: {report}"
    );
}

#[test]
fn a_zero_budget_is_refused_instead_of_deleting_everything() {
    for key in [
        "KALLIPAI_TELEMETRY_MAX_BYTES",
        "KALLIPAI_TELEMETRY_MAX_AGE_HOURS",
    ] {
        let moved = Policy::from_env(|name| (name == key).then(|| "0".into()));
        assert!(moved.is_err(), "{key}=0 must not be accepted");
    }
}

#[test]
fn a_unit_from_the_future_is_kept() {
    let dev = DevDir::new("prune-future");
    let root = root(&dev);
    let now = SystemTime::now();
    // Clock skew or a restored archive: the unit reads as younger than now.
    // For a delete tool that is the right direction to guess in.
    put(&root, "tempo/blocks/skewed", 10, now + hours(10));

    let scanned = scan(&root).expect("scan");
    let planned = plan(&scanned, now, &Policy::default());
    assert!(
        planned.deletions.is_empty(),
        "a future mtime is never aged out"
    );
}
