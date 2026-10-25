//! Retention for the local observability stack's data directory.
//!
//! The stack (`nix/obs-stack.nix`) writes tempo's blocks, its wal and
//! grafana's state under one root on the host, and nothing else bounds them.
//! This crate drops the oldest units that are safe to lose whole -- a tempo
//! block, a wal segment -- until the root fits both an age and a size budget.
//!
//! Two rules shape the design:
//!
//! - Grafana's directory counts toward the size total but is never a deletion
//!   unit: it holds configuration and plugins, so an age threshold would wipe
//!   the UI's state instead of old telemetry.
//! - Nothing outside the root is reachable: a unit that is a symlink is
//!   refused rather than followed, and a unit whose canonical path leaves the
//!   root is refused as well. The root itself is fixed by the caller (the same
//!   string the stack mounts), never taken from a flag.
//!
//! The two budgets are mirrored in `nix/obs-stack.nix` (`maxBytes`,
//! `maxAgeHours`), and `nix/checks/obs-stack.nix` reads this file back to hold
//! the pair together: a change to one copy without the other fails that check.
//!
//! Tempo retires a block only once compaction has run, and its
//! `block_retention` does not cover the wal at all. The age rule here is what
//! makes the horizon hold either way: it does not wait for compaction and it
//! reaches the wal segments, which is why the same 72h is written twice.
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Total budget across the whole telemetry root.
pub const MAX_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Age budget: traces older than this go before the size budget is reached.
pub const MAX_AGE_HOURS: u64 = 72;
pub const MAX_AGE: Duration = Duration::from_secs(MAX_AGE_HOURS * 60 * 60);

/// Directories whose immediate children are deletion units, relative to the
/// telemetry root. Grafana is deliberately absent: it is counted, not pruned.
///
/// Each child has to be one block directory or one wal segment, which is what
/// the layout under these two paths holds today. If a layout ever nests a
/// level deeper -- a tenant directory per child, say -- the age of a child
/// stops being the age of its oldest data: writing a file inside a directory
/// does not move that directory's mtime, so the age would read as "last time
/// this directory changed" and the size rule could then take fresh data. A
/// layout change therefore has to change this walk to descend to the leaves,
/// not only this constant.
pub const UNIT_PARENTS: [&str; 2] = ["tempo/blocks", "tempo/wal"];

/// The same root the stack writes: `$XDG_DATA_HOME/kallipai/telemetry`, or
/// `$HOME/.local/share/kallipai/telemetry`. No flag and no env var overrides
/// it -- cleaning and writing have to agree on one path.
///
/// The stack guards harder than this: it also rejects a colon and `/`, because
/// those break docker's bind syntax. Prune needs only an absolute path, so a
/// base the stack rejects for a colon reaches this function as a root that does
/// not exist, which a run reports as a no-op; a relative base fails the check
/// below instead.
pub fn default_root(xdg_data_home: Option<&str>, home: Option<&str>) -> Result<PathBuf> {
    let (base, suffix) = match non_empty(xdg_data_home) {
        Some(path) => (path, "kallipai/telemetry"),
        None => (
            non_empty(home).context(
                "neither XDG_DATA_HOME nor HOME is set; cannot place the telemetry data",
            )?,
            ".local/share/kallipai/telemetry",
        ),
    };
    let base = Path::new(base);
    if !base.is_absolute() {
        bail!(
            "the telemetry root's base must be an absolute path (got '{}')",
            base.display()
        );
    }
    Ok(base.join(suffix))
}

/// The root as a run sees it: `None` when nothing has written there yet.
///
/// A machine that never started the stack has no telemetry directory; a tidy
/// job on it is a no-op, not an alarm. A root that does exist but is not a
/// directory, or cannot be looked at, is still an error.
///
/// A root that is a symlink to a directory counts as a directory: docker
/// follows that link when it binds the data directory, so the stack writes
/// there, and a run that refused to look through the link would leave the data
/// unpruned for as long as the link stands.
/// A link whose target is gone is not "nothing here yet": the path is
/// configured and the target disappeared, so a run reports it instead.
pub fn existing_root(root: &Path) -> Result<Option<PathBuf>> {
    match fs::metadata(root) {
        Ok(metadata) if metadata.is_dir() => Ok(Some(root.to_path_buf())),
        Ok(_) => bail!("the telemetry root {} is not a directory", root.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::symlink_metadata(root) {
                // The link is here, its target is not: the path is configured,
                // so a no-op would hide a setup that points at nothing.
                Ok(_) => bail!(
                    "the telemetry root {} is a link that leads nowhere",
                    root.display()
                ),
                Err(inner) if inner.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(inner) => Err(inner).with_context(|| {
                    format!("the telemetry root {} is unreadable", root.display())
                }),
            }
        }
        Err(error) => Err(error)
            .with_context(|| format!("the telemetry root {} is unreadable", root.display())),
    }
}

/// Ages in the budget line stay in hours: 72h reads as a budget, 3d as a
/// duration someone would have to convert back.
fn hours_of(age: Duration) -> String {
    format!("{}h", age.as_secs() / 3600)
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.is_empty())
}

/// Budgets, so a test can drive the rules without writing 5 GiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub max_bytes: u64,
    pub max_age: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_bytes: MAX_BYTES,
            max_age: MAX_AGE,
        }
    }
}

impl Policy {
    /// The two numbers are the only part an operator may move; the root is not.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let mut policy = Self::default();
        if let Some(raw) = get("KALLIPAI_TELEMETRY_MAX_BYTES") {
            policy.max_bytes = non_empty(Some(&raw))
                .context("KALLIPAI_TELEMETRY_MAX_BYTES is empty")?
                .parse()
                .with_context(|| {
                    format!("KALLIPAI_TELEMETRY_MAX_BYTES is not a byte count: '{raw}'")
                })?;
            if policy.max_bytes == 0 {
                bail!(
                    "KALLIPAI_TELEMETRY_MAX_BYTES is zero: a zero budget would delete every unit"
                );
            }
        }
        if let Some(raw) = get("KALLIPAI_TELEMETRY_MAX_AGE_HOURS") {
            let hours: u64 = non_empty(Some(&raw))
                .context("KALLIPAI_TELEMETRY_MAX_AGE_HOURS is empty")?
                .parse()
                .with_context(|| {
                    format!("KALLIPAI_TELEMETRY_MAX_AGE_HOURS is not a number of hours: '{raw}'")
                })?;
            if hours == 0 {
                bail!(
                    "KALLIPAI_TELEMETRY_MAX_AGE_HOURS is zero: a zero age would delete every unit"
                );
            }
            policy.max_age = Duration::from_secs(hours.saturating_mul(60 * 60));
        }
        Ok(policy)
    }
}

/// One deletion candidate: an immediate child of a unit parent.
#[derive(Debug, Clone)]
pub struct Unit {
    pub path: PathBuf,
    pub bytes: u64,
    pub modified: SystemTime,
    /// The shape the scan saw. `apply` compares the path against it: a child
    /// the occupant replaced with a different kind of entry is not the unit
    /// that was planned, so it is left alone rather than deleted by guesswork.
    pub is_dir: bool,
}

/// A unit the scan refused to treat as a candidate, and why.
#[derive(Debug, Clone)]
pub struct Refusal {
    pub path: PathBuf,
    pub reason: &'static str,
}

#[derive(Debug)]
pub struct Scan {
    pub root: PathBuf,
    /// Everything under the root, grafana included.
    pub total_bytes: u64,
    /// Oldest first.
    pub units: Vec<Unit>,
    pub refusals: Vec<Refusal>,
}

/// What a run would delete, and what the root looks like afterwards.
#[derive(Debug)]
pub struct Plan {
    pub deletions: Vec<Unit>,
    pub freed_bytes: u64,
    pub total_after: u64,
    pub kept_units: usize,
    /// Bytes that stay because no rule may touch them: grafana's state and
    /// every unit the scan refused.
    pub stranded_bytes: u64,
    /// True when `stranded_bytes` alone is past the size budget. The size rule
    /// then stands down -- deleting every unit could not reach the budget, and
    /// taking all the traces for nothing is the one outcome this tool must not
    /// choose -- so the age rule decides alone.
    pub budget_unreachable: bool,
}

/// What a run deleted, and every planned unit it left alone on the way.
#[derive(Debug, Default)]
pub struct Applied {
    pub deleted: usize,
    /// The same reasons a scan refuses a unit, plus the two shapes a live stack
    /// produces between the scan and the delete.
    pub skipped: Vec<Refusal>,
}

impl Applied {
    fn skip(&mut self, path: &Path, reason: &'static str) {
        self.skipped.push(Refusal {
            path: path.to_path_buf(),
            reason,
        });
    }
}

pub fn scan(root: &Path) -> Result<Scan> {
    let root = root.to_path_buf();
    // Follow a symlinked root, the way docker follows it when it binds the data
    // directory (see `existing_root`).
    let meta = fs::metadata(&root)
        .with_context(|| format!("the telemetry root {} is unreadable", root.display()))?;
    if !meta.is_dir() {
        bail!("the telemetry root {} is not a directory", root.display());
    }
    let canonical_root =
        fs::canonicalize(&root).with_context(|| format!("cannot resolve {}", root.display()))?;

    let mut units = Vec::new();
    let mut refusals = Vec::new();
    for parent in UNIT_PARENTS {
        let dir = root.join(parent);
        // A service that never ran leaves no directory; that is not an error.
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", dir.display()));
            }
        };
        for entry in entries {
            let path = entry
                .with_context(|| format!("cannot read an entry of {}", dir.display()))?
                .path();
            // The stack keeps running while this walks: tempo compacts and
            // retires blocks on its own. A path that is gone by now is the
            // service cleaning up, not a fault in this run.
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("cannot read {}", path.display()));
                }
            };
            if metadata.file_type().is_symlink() {
                refusals.push(Refusal {
                    path,
                    reason: "a symlink is never a deletion unit and is not followed",
                });
                continue;
            }
            let canonical = match fs::canonicalize(&path) {
                Ok(canonical) => canonical,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("cannot resolve {}", path.display()));
                }
            };
            if !canonical.starts_with(&canonical_root) {
                refusals.push(Refusal {
                    path,
                    reason: "resolves outside the telemetry root",
                });
                continue;
            }
            // No mtime would read as 1970 and make a young unit look oldest:
            // for a delete tool that is the wrong way to guess, so refuse.
            let modified = match metadata.modified() {
                Ok(modified) => modified,
                Err(_) => {
                    refusals.push(Refusal {
                        path,
                        reason: "no modification time; ages cannot be trusted",
                    });
                    continue;
                }
            };
            let bytes = match size_of(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("cannot size {}", path.display()));
                }
            };
            units.push(Unit {
                bytes,
                modified,
                is_dir: metadata.is_dir(),
                path,
            });
        }
    }
    units.sort_by_key(|unit| unit.modified);

    Ok(Scan {
        // The root itself may be a symlink and `size_of` never follows one, so
        // the total walks the resolved path. Unit paths stay as the caller
        // spelled them, which is what `render` strips.
        total_bytes: size_of(&canonical_root)
            .with_context(|| format!("cannot size {}", root.display()))?,
        root,
        units,
        refusals,
    })
}

/// Recursive size, never following a symlink (its own metadata is tiny and its
/// target may live anywhere).
fn size_of(path: &Path) -> std::io::Result<u64> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        // The stack keeps running while this walks: a block or a wal segment
        // can be retired between the read of a directory and the size of its
        // entries. A path that is gone by now is the service cleaning up, not a
        // fault in this run, so it counts as nothing.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        return Ok(0);
    }
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut total = 0;
    for entry in entries {
        total += size_of(&entry?.path())?;
    }
    Ok(total)
}

/// Oldest first, and a unit goes while the root is over either budget. Walking
/// in age order keeps the two rules from fighting: the size rule stops as soon
/// as the total fits, and the age rule never revisits what the size rule kept.
pub fn plan(scan: &Scan, now: SystemTime, policy: &Policy) -> Plan {
    let mut remaining = scan.total_bytes;
    let mut deletions = Vec::new();
    let mut freed_bytes = 0;
    let prunable: u64 = scan.units.iter().map(|unit| unit.bytes).sum();
    let stranded_bytes = scan.total_bytes.saturating_sub(prunable);
    let budget_unreachable = stranded_bytes > policy.max_bytes;
    for unit in &scan.units {
        let over_size = !budget_unreachable && remaining > policy.max_bytes;
        // The list is sorted by age, so `over_age` never turns back on once it
        // is false. That monotone pair is what makes the `break` below safe: a
        // different sort key would silently skip units that should go.
        // An mtime in the future -- clock skew, a restored archive -- makes
        // this age zero, so the unit reads as young and is kept. For a delete
        // tool, guessing wrong towards "keep" is the only acceptable side.
        let over_age = now
            .duration_since(unit.modified)
            .unwrap_or_default()
            .gt(&policy.max_age);
        if !over_size && !over_age {
            break;
        }
        remaining = remaining.saturating_sub(unit.bytes);
        freed_bytes += unit.bytes;
        deletions.push(unit.clone());
    }
    Plan {
        kept_units: scan.units.len() - deletions.len(),
        total_after: remaining,
        freed_bytes,
        stranded_bytes,
        budget_unreachable,
        deletions,
    }
}

/// Delete exactly the planned units, re-checking the guard on the way. A path
/// that became a symlink, left the root, was retired by the stack, or changed
/// shape since the scan is skipped and reported, never followed; the run stops
/// only on an error it cannot attribute to the occupant.
pub fn apply(scan: &Scan, plan: &Plan) -> Result<Applied> {
    let canonical_root = fs::canonicalize(&scan.root)
        .with_context(|| format!("cannot resolve {}", scan.root.display()))?;
    let mut applied = Applied::default();
    for unit in &plan.deletions {
        // Half the list may already be gone when this runs against a live
        // stack: tempo compacts and retires blocks while the plan is executed.
        let metadata = match fs::symlink_metadata(&unit.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                applied.skip(&unit.path, "was gone before it could be deleted");
                continue;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", unit.path.display()));
            }
        };
        if metadata.file_type().is_symlink() {
            applied.skip(&unit.path, "became a symlink, which is never followed");
            continue;
        }
        let canonical = match fs::canonicalize(&unit.path) {
            Ok(canonical) => canonical,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                applied.skip(&unit.path, "was gone before it could be deleted");
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot resolve {}", unit.path.display()));
            }
        };
        if !canonical.starts_with(&canonical_root) || canonical == canonical_root {
            applied.skip(&unit.path, "resolved outside the telemetry root");
            continue;
        }
        if metadata.is_dir() != unit.is_dir {
            applied.skip(&unit.path, "changed shape since the scan");
            continue;
        }
        let was_dir = metadata.is_dir();
        let removed = if was_dir {
            fs::remove_dir_all(&unit.path)
        } else {
            fs::remove_file(&unit.path)
        };
        match removed {
            Ok(()) => applied.deleted += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                applied.skip(&unit.path, "was gone before it could be deleted");
            }
            Err(error) => {
                // A directory that turned into a file, or the reverse, between
                // the read and the delete fails with a type error. That is the
                // occupant replacing its own data -- the same event the NotFound
                // arm absorbs -- so it is skipped the same way rather than
                // aborting a run that may already have deleted half the plan.
                let now_is_dir = fs::symlink_metadata(&unit.path)
                    .map(|metadata| metadata.is_dir())
                    .ok();
                if now_is_dir != Some(was_dir) {
                    applied.skip(&unit.path, "changed shape while it was being deleted");
                    continue;
                }
                return Err(error).with_context(|| {
                    format!(
                        "cannot delete {} ({} of {} planned units were deleted)",
                        unit.path.display(),
                        applied.deleted,
                        plan.deletions.len()
                    )
                });
            }
        }
    }
    Ok(applied)
}

/// The dry run's face: what would go, what it frees, and where the root lands.
pub fn render(scan: &Scan, plan: &Plan, policy: &Policy, now: SystemTime, dry_run: bool) -> String {
    let mut out = String::new();
    out.push_str(&format!("telemetry root {}\n", scan.root.display()));
    out.push_str(&format!(
        "  total {}, budget {}, max age {}\n",
        human(scan.total_bytes),
        human(policy.max_bytes),
        hours_of(policy.max_age),
    ));
    out.push_str(&format!(
        "  units {} (keep {}), refused {}\n",
        scan.units.len(),
        plan.kept_units,
        scan.refusals.len(),
    ));
    for unit in &plan.deletions {
        out.push_str(&format!(
            "  delete {} ({}, {} old)\n",
            strip(&scan.root, &unit.path).display(),
            human(unit.bytes),
            human_age(now.duration_since(unit.modified).unwrap_or_default()),
        ));
    }
    for refusal in &scan.refusals {
        out.push_str(&format!(
            "  refused {} ({})\n",
            strip(&scan.root, &refusal.path).display(),
            refusal.reason
        ));
    }
    out.push_str(&format!(
        "  frees {}, total after {}\n",
        human(plan.freed_bytes),
        human(plan.total_after),
    ));
    if plan.budget_unreachable {
        out.push_str(&format!(
            "  budget unreachable: {} cannot be pruned (grafana state, refused units)\n",
            human(plan.stranded_bytes),
        ));
    }
    if dry_run {
        out.push_str("dry run: nothing deleted (pass --apply to delete)\n");
    }
    out
}

/// The apply run's face: what it deleted, and every planned unit it left alone.
pub fn render_applied(scan: &Scan, applied: &Applied, planned: usize) -> String {
    let mut out = format!(
        "deleted {} of {} planned units, skipped {}\n",
        applied.deleted,
        planned,
        applied.skipped.len()
    );
    for skipped in &applied.skipped {
        out.push_str(&format!(
            "  skipped {} ({})\n",
            strip(&scan.root, &skipped.path).display(),
            skipped.reason
        ));
    }
    out
}

/// Report paths relative to the root when they sit under it: one screen, one
/// coordinate system.
fn strip<'a>(root: &Path, path: &'a Path) -> &'a Path {
    path.strip_prefix(root).unwrap_or(path)
}

fn human(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
    for (label, scale) in UNITS {
        if bytes >= scale {
            return format!("{:.1} {label}", bytes as f64 / scale as f64);
        }
    }
    format!("{bytes} B")
}

fn human_age(age: Duration) -> String {
    let hours = age.as_secs() / 3600;
    if hours >= 48 {
        format!("{}d", hours / 24)
    } else {
        format!("{hours}h")
    }
}
