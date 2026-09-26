//! The `kallip team` family: declarative team management over the tagma's
//! team domain. `status` renders the three-way comparison, `converge`
//! plans/preflights/executes through the tagma and rewrites the lock
//! archive from the response, `lock-rebuild` reconstructs the archive
//! from reality (live registry + inactive area) when it is lost or
//! suspected stale.
//!
//! The lock (`tagma.lock`) is a CLI-side archive co-located with the
//! declaration — the tagma never holds or locates it. It is written
//! atomically (temp + rename) only after converge lands, and only for
//! the mapping the tagma produced.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};
use kallipai_client::TagmaClient;
use kallipai_common::declaration::parse_declaration;
use kallipai_common::protocol::{
    RoleDisposition, TeamAction, TeamConvergeOutcome, TeamConvergeRequest, TeamStatusQuery,
};
use serde::{Deserialize, Serialize};

use crate::args::team::{TeamCommand, TeamConvergeArgs, TeamLockRebuildArgs, TeamStatusArgs};

/// One record of `tagma.lock`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LockRole {
    name: String,
    id: String,
    converged_at: String,
}

/// The `tagma.lock` document: role records; the lock is a set compared
/// by role name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LockFile {
    #[serde(default, rename = "role")]
    roles: Vec<LockRole>,
}

impl LockFile {
    fn fold_to_pairs(&self) -> String {
        self.roles
            .iter()
            .map(|r| format!("{}:{}", r.name, r.id))
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Resolve the declaration path and the lock path for a command. The
/// lock defaults to `tagma.lock` beside the declaration — the two files
/// are one team archive. The declaration travels absolute: the tagma
/// reads it in its own process and resolves no caller-relative paths.
/// The lock stays a CLI-side path exactly as given (the tagma never
/// sees it).
fn resolve_paths(file: &Option<String>, lock: &Option<PathBuf>) -> Result<(String, PathBuf)> {
    let declaration = file.clone().unwrap_or_else(|| "tagma.toml".to_string());
    let absolute = std::path::absolute(&declaration)
        .map_err(|e| anyhow!("resolve {declaration}: {e}"))?
        .display()
        .to_string();
    let lock_path = lock.clone().unwrap_or_else(|| {
        Path::new(&absolute)
            .parent()
            .unwrap_or(Path::new("/"))
            .join("tagma.lock")
    });
    Ok((absolute, lock_path))
}

/// Read the lock archive. `Ok(None)` = the file does not exist, which
/// is legal (a first converge has nothing to restore from). A read or
/// parse failure is refused, never folded into an empty archive: an
/// empty mapping makes converge re-spawn every member as fresh agents,
/// so a corrupted archive must be rebuilt or fixed by hand first.
fn read_lock(path: &Path) -> Result<Option<LockFile>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!(
                "cannot read the lock archive {}; run `kallip team lock-rebuild` — converging with a lost mapping would re-spawn every member as fresh agents",
                path.display()
            )))
        }
    };
    // An existing but blank archive (zero bytes or whitespace) is
    // refused exactly like a corrupt one: an archive that once held
    // records must not silently read as "no members". A legal
    // document with zero records is caught after the parse below.
    // Clearing the team is an operator decision: delete the file and
    // the next converge is a legitimate first converge.
    if raw.trim().is_empty() {
        return Err(anyhow!(
            "the lock archive {} is empty; run `kallip team lock-rebuild` — converging with a lost mapping would re-spawn every member as fresh agents",
            path.display()
        ));
    }
    let lock: LockFile = toml::from_str(&raw).context(format!(
        "the lock archive {} is invalid; run `kallip team lock-rebuild` — converging with a lost mapping would re-spawn every member as fresh agents",
        path.display()
    ))?;
    // A legal document recording no members — only comments, an
    // empty `role` array, or a key serde folds into the default —
    // would read as "no members" downstream, so refuse it like a blank
    // file.
    if lock.roles.is_empty() {
        return Err(anyhow!(
            "the lock archive {} holds no role records; run `kallip team lock-rebuild` — converging with a lost mapping would re-spawn every member as fresh agents",
            path.display()
        ));
    }
    Ok(Some(lock))
}

/// Distinct temp name per call: two writers of the same archive never
/// race on one temp file; the counter makes this clock-independent.
fn lock_tmp_path(path: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    path.with_extension(format!("lock.tmp.{}.{}", std::process::id(), seq))
}

fn write_lock_atomic(path: &Path, lock: &LockFile) -> Result<()> {
    let body = toml::to_string_pretty(lock).context("serialize lock archive")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    // temp + rename: a crash leaves either the old file or the new one.
    let tmp = lock_tmp_path(path);
    std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

/// Short display form of an id: first segment is enough to eyeball a row.
fn short(id: &str) -> String {
    id.split('-').next().unwrap_or(id).to_string()
}

pub async fn run_team(client: &TagmaClient, cmd: &TeamCommand) -> Result<()> {
    match cmd {
        TeamCommand::Status(args) => run_status(client, args).await,
        TeamCommand::Converge(args) => run_converge(client, args).await,
        TeamCommand::LockRebuild(args) => run_lock_rebuild(client, args).await,
    }
}

async fn run_status(client: &TagmaClient, args: &TeamStatusArgs) -> Result<()> {
    let (declaration, lock_path) = resolve_paths(&args.common.file, &args.common.lock)?;
    // A corrupt archive must not silently render as "no lock": say so,
    // then show the table without it (the face is read-only).
    let lock = match read_lock(&lock_path) {
        Ok(Some(lock)) => lock,
        Ok(None) => LockFile::default(),
        Err(e) => {
            eprintln!("warning: {e:#}");
            eprintln!("warning: rendering without the lock archive; the LOCK column is unreliable");
            LockFile::default()
        }
    };
    let query = TeamStatusQuery {
        file: declaration.clone(),
        lock: (!lock.roles.is_empty()).then_some(lock.fold_to_pairs()),
    };
    let status = client.team_status(&query).await?;
    if args.common.json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }
    println!("declaration: {declaration}");
    println!(
        "{:<18} {:<10} {:<16} {:<10} VERDICT",
        "ROLE", "DECLARED", "LOCK", "LIVE"
    );
    for row in &status.roles {
        let declared = match &row.declaration {
            Some(d) if d.unmanaged => "unmanaged".to_string(),
            Some(_) => "yes".to_string(),
            None => "-".to_string(),
        };
        let lock_cell = match &row.lock_id {
            Some(id) => {
                let mut cell = short(id.as_ref());
                if row.lock_drift {
                    cell.push_str(" (drift)");
                } else if row.lock_inactive {
                    cell.push_str(" (parked)");
                }
                cell
            }
            None => "-".to_string(),
        };
        let live_cell = if row.live.is_empty() {
            "-".to_string()
        } else {
            row.live
                .iter()
                .map(|id| short(id.as_ref()))
                .collect::<Vec<_>>()
                .join(",")
        };
        let mut verdict = verdict_word(row.disposition).to_string();
        if row.root_conflict {
            verdict.push_str(" (reserved: root is boot-built)");
        }
        println!(
            "{:<18} {:<10} {:<16} {:<10} {}",
            row.role, declared, lock_cell, live_cell, verdict,
        );
    }
    Ok(())
}

fn verdict_word(d: RoleDisposition) -> &'static str {
    match d {
        RoleDisposition::Active => "in sync",
        RoleDisposition::Adopt => "adopt",
        RoleDisposition::Restore => "restore",
        RoleDisposition::Spawn => "spawn",
        RoleDisposition::Deactivate => "deactivate",
        RoleDisposition::Exempt => "exempt",
        RoleDisposition::Duplicate => "DUPLICATE",
        RoleDisposition::Retain => "retain",
    }
}

async fn run_converge(client: &TagmaClient, args: &TeamConvergeArgs) -> Result<()> {
    let (declaration, lock_path) = resolve_paths(&args.common.file, &args.common.lock)?;
    // A corrupt archive refuses the run (never reads as empty): the
    // operator decides — rebuild or fix — instead of losing every
    // identity binding to a silent re-spawn.
    let lock = read_lock(&lock_path)?.unwrap_or_default();

    // --drain: converge refuses while deactivation targets are busy; the
    // CLI polls until the refusal clears (Ctrl-C aborts the wait, which
    // is this loop and nothing on the tagma).
    let mut attempt = 0u64;
    loop {
        let req = TeamConvergeRequest {
            file: declaration.clone(),
            lock: (!lock.roles.is_empty()).then_some(lock.fold_to_pairs()),
            dry_run: args.dry_run,
            force: args.force,
        };
        let (outcome, resp) = client.team_converge(&req).await?;
        let busy: Vec<&kallipai_common::protocol::TeamRejection> = resp
            .rejections
            .iter()
            .filter(|r| r.kind == kallipai_common::protocol::TeamRejectionKind::Busy)
            .collect();
        let only_busy = !busy.is_empty() && busy.len() == resp.rejections.len();
        if args.drain && outcome == TeamConvergeOutcome::Rejected && only_busy {
            attempt += 1;
            if attempt == 1 {
                for b in &busy {
                    println!("draining: {}", b.message);
                }
                println!("draining — waiting for idle (Ctrl-C to abort)...");
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            continue;
        }
        render_converge(args, &outcome, &resp, &lock_path, &declaration)?;
        return Ok(());
    }
}

fn action_word(a: TeamAction) -> &'static str {
    match a {
        TeamAction::Spawn => "spawn",
        TeamAction::Restore => "restore",
        TeamAction::Adopt => "adopt",
        TeamAction::Deactivate => "deactivate",
        TeamAction::AlignMetadata => "align",
        TeamAction::Retain => "retain",
    }
}

/// Results counted per verb for the summary line.
fn count_results(
    results: &[kallipai_common::protocol::TeamActionResult],
) -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::new();
    for key in [
        "spawned",
        "restored",
        "adopted",
        "deactivated",
        "aligned",
        "retained",
    ] {
        counts.insert(key, 0usize);
    }
    for r in results {
        // A failed row still carries its action; count only rows that
        // landed so a fail-fast summary cannot inflate the successes.
        if r.outcome != kallipai_common::protocol::TeamRowOutcome::Applied {
            continue;
        }
        let key = match r.action {
            TeamAction::Spawn => "spawned",
            TeamAction::Restore => "restored",
            TeamAction::Adopt => "adopted",
            TeamAction::Deactivate => "deactivated",
            TeamAction::AlignMetadata => "aligned",
            TeamAction::Retain => "retained",
        };
        *counts.entry(key).or_insert(0usize) += 1;
    }
    counts
}

/// Inactive agents referenced by neither the fresh lock nor the
/// declaration: name them loudly at lock-write time, so invisibility
/// never masquerades as convergence. Best-effort — a missing
/// inactive-agent set (an older daemon, or the daemon's own scan
/// failing) prints a one-line skip warning and never fails the run.
fn warn_orphaned_inactive_agents(
    declaration: &str,
    mapping: &[kallipai_common::protocol::TeamLockEntry],
    inactive_agents: Option<&[kallipai_common::protocol::TeamInactiveAgent]>,
) {
    // Converge just read and parsed this same file; a failure here is
    // still warned loudly instead of passing silently.
    let declared: std::collections::HashSet<String> = match std::fs::read_to_string(declaration) {
        Ok(raw) => match parse_declaration(&raw) {
            Ok(d) => d.roles.into_iter().map(|r| r.name).collect(),
            Err(e) => {
                eprintln!("warning: cannot parse the declaration for the orphan check: {e:#}");
                return;
            }
        },
        Err(e) => {
            eprintln!("warning: cannot read the declaration for the orphan check: {e}");
            return;
        }
    };
    let inactive = match inactive_agents {
        Some(entries) => entries,
        None => {
            eprintln!(
                "warning: the daemon did not report the inactive-agent set; skipping the orphan check"
            );
            return;
        }
    };
    for line in orphan_warning_lines(&declared, mapping, inactive) {
        eprintln!("{line}");
    }
}

/// The three orphan classifications over fixed slices, extracted so the
/// branch behaviors are pin-testable without a daemon. Each line is a
/// complete warning, ready to print.
fn orphan_warning_lines(
    declared: &std::collections::HashSet<String>,
    mapping: &[kallipai_common::protocol::TeamLockEntry],
    inactive: &[kallipai_common::protocol::TeamInactiveAgent],
) -> Vec<String> {
    let mut lines = Vec::new();
    for entry in inactive {
        if mapping
            .iter()
            .any(|e| e.id.to_string() == entry.id.to_string())
        {
            continue;
        }
        if let Some(bound) = mapping.iter().find(|e| e.role == entry.role) {
            lines.push(format!(
                "warning: parked agent {} (role {:?}) is superseded by {} in the new lock — it stays parked, untouched",
                entry.id, entry.role, bound.id
            ));
        } else if declared.contains(&entry.role) {
            lines.push(format!(
                "warning: parked agent {} (role {:?}) is declared but absent from the new lock — it stays parked, untouched",
                entry.id, entry.role
            ));
        } else {
            lines.push(format!(
                "warning: parked agent {} (role {:?}) is referenced by neither the new lock nor the declaration — it stays parked, untouched",
                entry.id, entry.role
            ));
        }
    }
    lines
}
fn write_lock_from(
    resp: &kallipai_common::protocol::TeamConvergeResponse,
    lock_path: &Path,
) -> Result<()> {
    let lock = LockFile {
        roles: resp
            .lock
            .iter()
            .map(|e| LockRole {
                name: e.role.clone(),
                id: e.id.to_string(),
                converged_at: e.converged_at.clone(),
            })
            .collect(),
    };
    write_lock_atomic(lock_path, &lock)
}

fn render_converge(
    args: &TeamConvergeArgs,
    outcome: &TeamConvergeOutcome,
    resp: &kallipai_common::protocol::TeamConvergeResponse,
    lock_path: &Path,
    declaration: &str,
) -> Result<()> {
    if args.common.json {
        println!("{}", serde_json::to_string_pretty(resp)?);
    } else {
        println!("plan ({}):", resp.plan.len());
        for row in &resp.plan {
            let target = row
                .agent_id
                .as_ref()
                .map(|id| short(id.as_ref()))
                .unwrap_or_else(|| "new".to_string());
            println!(
                "  {:<14} {:<12} {}",
                row.role,
                action_word(row.action),
                target
            );
            for note in &row.notes {
                println!("                 note: {note}");
            }
        }
        for r in &resp.rejections {
            println!("REJECTED: {}", r.message);
        }
        for r in &resp.results {
            let marker = match r.outcome {
                kallipai_common::protocol::TeamRowOutcome::Applied => "ok",
                kallipai_common::protocol::TeamRowOutcome::Failed => "FAILED",
            };
            println!(
                "  [{marker}] {:<14} {}: {}",
                r.role,
                action_word(r.action),
                r.detail
            );
            for note in &r.notes {
                println!("                 note: {note}");
            }
        }
        let counts = count_results(&resp.results);
        println!(
            "spawned {} / restored {} / adopted {} / deactivated {} / aligned {} / retained {}",
            counts["spawned"],
            counts["restored"],
            counts["adopted"],
            counts["deactivated"],
            counts["aligned"],
            counts["retained"],
        );
    }
    match outcome {
        TeamConvergeOutcome::Planned => {
            println!(
                "dry run: nothing was applied; the lock mapping is the plan minus planned spawns."
            );
            Ok(())
        }
        TeamConvergeOutcome::Applied => {
            if resp.lock.is_empty() {
                if lock_path.exists() {
                    eprintln!(
                        "warning: converge left no surviving records; no lock written; the existing lock file at {} is left untouched and may be stale",
                        lock_path.display()
                    );
                } else {
                    eprintln!("warning: converge left no surviving records; no lock written");
                }
            } else {
                write_lock_from(resp, lock_path)?;
                warn_orphaned_inactive_agents(
                    declaration,
                    &resp.lock,
                    resp.inactive_agents.as_deref(),
                );
                println!("lock written: {}", lock_path.display());
            }
            Ok(())
        }
        TeamConvergeOutcome::Aborted => {
            if resp.lock.is_empty() {
                anyhow::bail!(
                    "converge aborted before anything landed; no lock written; re-run to converge"
                )
            }
            write_lock_from(resp, lock_path)?;
            warn_orphaned_inactive_agents(declaration, &resp.lock, resp.inactive_agents.as_deref());
            anyhow::bail!(
                "converge aborted mid-execution; the written lock keeps the surviving records and the rows that landed — re-run to converge the rest"
            )
        }
        TeamConvergeOutcome::Rejected => anyhow::bail!(
            "converge rejected ({} finding(s)); nothing was applied",
            resp.rejections.len()
        ),
    }
}

/// `team lock-rebuild`: reconstruct the archive from reality. Live
/// members come from the registry; inactive agents come from the daemon's
/// listing, where each entry carries its role. Every
/// recovered parked body is listed — a rebuild that silently absorbed a
/// parked body would hide a member, the invisibility rebuild must prevent.
async fn run_lock_rebuild(client: &TagmaClient, args: &TeamLockRebuildArgs) -> Result<()> {
    let live = client.list_agents(None).await?;
    let inactive = client
        .list_inactive_agents()
        .await
        .context(
            "the daemon does not expose the inactive-agent listing; upgrade the daemon to use lock-rebuild",
        )?
        .inactive_agents;

    let now = kallipai_common::timefmt::format_utc(kallipai_common::timefmt::now_epoch());
    let mut roles: Vec<LockRole> = Vec::new();
    for a in &live {
        // The root carries no lock-managed role; every other live member
        // rebinds from its registry role.
        if a.created_by.is_none() || a.role.is_empty() {
            continue;
        }
        roles.push(LockRole {
            name: a.role.clone(),
            id: a.id.to_string(),
            converged_at: now.clone(),
        });
    }
    let mut recovered = Vec::new();
    for entry in &inactive {
        if entry.role.is_empty() {
            continue;
        }
        recovered.push(format!("{} (role {:?})", entry.id, entry.role));
        roles.push(LockRole {
            name: entry.role.clone(),
            id: entry.id.to_string(),
            converged_at: now.clone(),
        });
    }
    // One record per role: duplicates mean multiple bodies claim one role
    // — a human must look, the rebuild does not guess.
    let mut by_role: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for r in &roles {
        by_role
            .entry(r.name.clone())
            .or_default()
            .push(r.id.clone());
    }
    let conflicts: Vec<(&String, &Vec<String>)> =
        by_role.iter().filter(|(_, ids)| ids.len() > 1).collect();

    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "roles": roles.iter().map(|r| serde_json::json!({
                    "name": r.name, "id": r.id, "converged_at": r.converged_at,
                })).collect::<Vec<_>>(),
                "recovered_inactive": recovered,
                "conflicts": conflicts.iter().map(|(role, ids)| serde_json::json!({
                    "role": role, "ids": ids,
                })).collect::<Vec<_>>(),
            })
        );
    } else {
        println!("live members: {}", live.len());
        for r in &recovered {
            println!("  recovered from the inactive area: {r}");
        }
        for (role, ids) in &conflicts {
            println!(
                "CONFLICT: role {role:?} is claimed by {} bodies ({ids:?})",
                ids.len()
            );
        }
    }
    if !conflicts.is_empty() {
        anyhow::bail!("lock rebuild found conflicting bindings; nothing was written");
    }
    let out_path = args
        .dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("tagma.lock"));
    let lock = LockFile { roles };
    write_lock_atomic(&out_path, &lock)?;
    if !args.json {
        println!("lock rebuilt: {}", out_path.display());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_tmp_paths_differ_across_calls() {
        let base = Path::new("/tmp/kallip-team-tests/tagma.lock");
        assert_ne!(lock_tmp_path(base), lock_tmp_path(base));
    }

    #[test]
    fn read_lock_classifies_missing_corrupt_and_valid() {
        let dir = write_scratch_dir("read-lock");
        // Missing: a legal empty archive (a first converge).
        assert!(read_lock(&dir.join("absent.lock")).unwrap().is_none());
        // Corrupt: refused with the rebuild guidance, never empty.
        let bad = dir.join("bad.lock");
        // Empty (zero bytes): refused exactly like corrupt — an archive
        // that went silently empty would re-spawn every member.
        let empty = dir.join("empty.lock");
        std::fs::write(&empty, "").unwrap();
        let err = read_lock(&empty).unwrap_err().to_string();
        assert!(err.contains("is empty"));
        assert!(err.contains("lock-rebuild"));
        std::fs::write(&bad, "not valid toml at all").unwrap();
        let err = read_lock(&bad).unwrap_err().to_string();
        assert!(err.contains("lock-rebuild"));
        assert!(err.contains("re-spawn every member"));

        // A directory path trips the io-error branch: it must name the
        // archive and the rebuild command, never read as "no members".
        let dirpath = dir.join("as-dir.lock");
        std::fs::create_dir(&dirpath).unwrap();
        let err = read_lock(&dirpath).unwrap_err().to_string();
        assert!(err.contains("cannot read the lock archive"), "{err}");
        assert!(err.contains("lock-rebuild"), "{err}");
        // Valid: parsed.
        let good = dir.join("good.lock");
        std::fs::write(
            &good,
            "[[role]]\nname = \"dev\"\nid = \"11111111-1111-4111-8111-111111111111\"\nconverged_at = \"2026-01-01T00:00:00+00:00\"\n",
        )
        .unwrap();
        let lock = read_lock(&good).unwrap().expect("valid lock parses");
        assert_eq!(lock.roles.len(), 1);
    }

    #[test]
    fn read_lock_refuses_zero_record_documents() {
        let dir = write_scratch_dir("read-lock-zero");

        // Three legal TOML documents that record no members: a pure
        // comment, an explicit empty array, and a key serde folds
        // into the default. Each refuses, never reads as "no members".
        let cases = [
            ("comment.lock", "# only a comment\n"),
            ("empty-array.lock", "role = []\n"),
            ("mistyped.lock", "rols = []\n"),
        ];
        for (name, body) in cases {
            let path = dir.join(name);
            std::fs::write(&path, body).unwrap();
            let err = read_lock(&path).unwrap_err().to_string();
            assert!(err.contains("holds no role records"), "{name}: {err}");
            assert!(err.contains("lock-rebuild"));
        }
    }

    fn archive(role: &str, id: &str) -> LockFile {
        LockFile {
            roles: vec![LockRole {
                name: role.to_string(),
                id: id.to_string(),
                converged_at: "2026-01-01T00:00:00+00:00".to_string(),
            }],
        }
    }

    use kallipai_testkit::DevDir;

    fn write_scratch_dir(name: &str) -> DevDir {
        DevDir::new(&format!("lock-write-{name}"))
    }

    #[test]
    fn fresh_write_publishes_the_archive_whole() {
        let dir = write_scratch_dir("fresh");
        let path = dir.join("team.lock");
        assert!(read_lock(&path).unwrap().is_none());
        write_lock_atomic(
            &path,
            &archive("dev", "11111111-1111-4111-8111-111111111111"),
        )
        .unwrap();
        let lock = read_lock(&path).unwrap().expect("published archive parses");
        assert_eq!(lock.roles.len(), 1);
        assert_eq!(lock.roles[0].name, "dev");
    }

    #[test]
    fn overwrite_publishes_the_new_archive_whole() {
        let dir = write_scratch_dir("overwrite");
        let path = dir.join("team.lock");
        write_lock_atomic(
            &path,
            &archive("dev", "11111111-1111-4111-8111-111111111111"),
        )
        .unwrap();
        write_lock_atomic(
            &path,
            &archive("ops", "22222222-2222-4222-8222-222222222222"),
        )
        .unwrap();
        let lock = read_lock(&path).unwrap().expect("published archive parses");
        assert_eq!(lock.roles.len(), 1);
        assert_eq!(lock.roles[0].name, "ops");
    }

    #[test]
    fn orphan_temp_from_a_crash_does_not_disturb_the_publish() {
        let dir = write_scratch_dir("orphan");
        let path = dir.join("team.lock");
        write_lock_atomic(
            &path,
            &archive("dev", "11111111-1111-4111-8111-111111111111"),
        )
        .unwrap();
        let orphan = lock_tmp_path(&path);
        std::fs::write(&orphan, "half-written garbage from a crashed writer").unwrap();
        write_lock_atomic(
            &path,
            &archive("ops", "22222222-2222-4222-8222-222222222222"),
        )
        .unwrap();
        let lock = read_lock(&path).unwrap().expect("published archive parses");
        assert_eq!(lock.roles.len(), 1);
        assert_eq!(lock.roles[0].name, "ops");
    }

    #[test]
    fn racing_writers_publish_one_whole_archive() {
        let dir = write_scratch_dir("race");
        let path = dir.join("team.lock");
        let p1 = path.clone();
        let p2 = path.clone();
        let h1 = std::thread::spawn(move || {
            write_lock_atomic(&p1, &archive("dev", "11111111-1111-4111-8111-111111111111"))
        });
        let h2 = std::thread::spawn(move || {
            write_lock_atomic(&p2, &archive("ops", "22222222-2222-4222-8222-222222222222"))
        });
        h1.join().unwrap().unwrap();
        h2.join().unwrap().unwrap();
        let lock = read_lock(&path).unwrap().expect("published archive parses");
        assert_eq!(lock.roles.len(), 1);
        assert!(lock.roles[0].name == "dev" || lock.roles[0].name == "ops");
    }

    #[test]
    fn lock_temp_names_are_unique_across_many_calls() {
        let base = std::env::temp_dir().join("kallip-team-tests/tagma.lock");
        // Pure string shaping, no IO: the archive writers must never
        // collide on one temp name, clock skew included.
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..100 {
            assert!(seen.insert(lock_tmp_path(&base)));
        }
    }

    /// The declaration travels absolute (the tagma resolves no
    /// caller-relative paths) and the lock default sits beside it —
    /// whatever directory the command runs from.
    #[test]
    fn declaration_absolutizes_and_lock_defaults_beside_it() {
        let cwd = std::env::current_dir().unwrap();
        let (declaration, lock) = resolve_paths(&Some("team/tagma.toml".into()), &None).unwrap();
        assert_eq!(Path::new(&declaration), &cwd.join("team/tagma.toml"));
        assert_eq!(lock, cwd.join("team/tagma.lock"));

        let (declaration, lock) =
            resolve_paths(&Some("/etc/kallip/tagma.toml".into()), &None).unwrap();
        assert_eq!(declaration, "/etc/kallip/tagma.toml");
        assert_eq!(lock, Path::new("/etc/kallip/tagma.lock"));
    }

    /// An explicit --lock passes through untouched: the lock is a
    /// CLI-side archive, the tagma never sees its path.
    #[test]
    fn an_explicit_lock_stays_as_given() {
        let (declaration, lock) = resolve_paths(
            &Some("/etc/kallip/tagma.toml".into()),
            &Some(PathBuf::from("relative.lock")),
        )
        .unwrap();
        assert_eq!(declaration, "/etc/kallip/tagma.toml");
        assert_eq!(lock, PathBuf::from("relative.lock"));
    }

    fn converge_args() -> TeamConvergeArgs {
        TeamConvergeArgs {
            common: crate::args::team::TeamCommonArgs {
                file: None,
                lock: None,
                json: false,
            },
            dry_run: false,
            drain: false,
            force: false,
        }
    }

    fn converge_resp(
        outcome: TeamConvergeOutcome,
        lock: Vec<kallipai_common::protocol::TeamLockEntry>,
    ) -> kallipai_common::protocol::TeamConvergeResponse {
        kallipai_common::protocol::TeamConvergeResponse {
            declaration_path: "tagma.toml".to_string(),
            dry_run: false,
            outcome,
            plan: Vec::new(),
            results: Vec::new(),
            rejections: Vec::new(),
            lock,
            inactive_agents: None,
        }
    }

    fn lock_entry(role: &str) -> kallipai_common::protocol::TeamLockEntry {
        kallipai_common::protocol::TeamLockEntry {
            role: role.to_string(),
            id: kallipai_common::agentid::AgentId::random(),
            converged_at: "2026-01-01T00:00:00+00:00".to_string(),
        }
    }

    fn action_result(
        outcome: kallipai_common::protocol::TeamRowOutcome,
    ) -> kallipai_common::protocol::TeamActionResult {
        kallipai_common::protocol::TeamActionResult {
            role: "dev".to_string(),
            action: TeamAction::Spawn,
            agent_id: None,
            outcome,
            detail: "ok".to_string(),
            notes: Vec::new(),
        }
    }

    /// An empty surviving lock must not be written: a zero-record
    /// archive would read back as corrupt and demand a rebuild, so the
    /// applied path warns and skips; the aborted path bails instead.
    #[test]
    fn empty_lock_is_never_written() {
        let dir = write_scratch_dir("empty-lock");
        let lock_path = dir.join("tagma.lock");

        let resp = converge_resp(TeamConvergeOutcome::Applied, Vec::new());
        render_converge(
            &converge_args(),
            &TeamConvergeOutcome::Applied,
            &resp,
            &lock_path,
            "tagma.toml",
        )
        .unwrap();
        assert!(!lock_path.exists());

        let resp = converge_resp(TeamConvergeOutcome::Aborted, Vec::new());
        let err = render_converge(
            &converge_args(),
            &TeamConvergeOutcome::Aborted,
            &resp,
            &lock_path,
            "tagma.toml",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no lock written"), "{err}");
        assert!(!lock_path.exists());
    }

    /// A lock with surviving records writes on both applied and aborted
    /// runs; the aborted run still fails the command after publishing.
    #[test]
    fn surviving_lock_writes_on_applied_and_aborted() {
        let dir = write_scratch_dir("surviving-lock");
        let lock_path = dir.join("tagma.lock");
        let entries = vec![lock_entry("dev")];

        let resp = converge_resp(TeamConvergeOutcome::Applied, entries.clone());
        render_converge(
            &converge_args(),
            &TeamConvergeOutcome::Applied,
            &resp,
            &lock_path,
            "tagma.toml",
        )
        .unwrap();
        assert!(lock_path.exists());

        std::fs::remove_file(&lock_path).unwrap();
        let resp = converge_resp(TeamConvergeOutcome::Aborted, entries);
        let err = render_converge(
            &converge_args(),
            &TeamConvergeOutcome::Aborted,
            &resp,
            &lock_path,
            "tagma.toml",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("mid-execution"), "{err}");
        assert!(lock_path.exists());
    }

    /// The success summary counts landed rows only: a failed row keeps
    /// its action label, and counting it would inflate the successes.
    #[test]
    fn count_results_counts_only_applied_rows() {
        let rows = vec![
            action_result(kallipai_common::protocol::TeamRowOutcome::Applied),
            action_result(kallipai_common::protocol::TeamRowOutcome::Failed),
            action_result(kallipai_common::protocol::TeamRowOutcome::Applied),
        ];
        let counts = count_results(&rows);
        assert_eq!(counts["spawned"], 2);
        assert_eq!(counts["restored"], 0);
    }

    /// The three orphan classifications over a fixed slice: a body whose
    /// id is in the mapping is skipped, a role rebound elsewhere is
    /// superseded, a declared role missing from the lock is named as
    /// such, and everything else is referenced by neither.
    #[test]
    fn orphan_lines_classify_the_three_branches() {
        let declared: std::collections::HashSet<String> =
            ["declared".to_string()].into_iter().collect();
        let mapping = vec![lock_entry("bound")];
        let bound_id = mapping[0].id.to_string();
        let inactive = vec![
            agent("bound"),
            agent("declared"),
            agent("orphan"),
            kallipai_common::protocol::TeamInactiveAgent {
                id: mapping[0].id.clone(),
                role: "shadow".to_string(),
            },
        ];
        let lines = orphan_warning_lines(&declared, &mapping, &inactive);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].contains("superseded"), "{lines:?}");
        assert!(lines[0].contains(&bound_id), "{lines:?}");
        assert!(lines[1].contains("declared but absent"), "{lines:?}");
        assert!(lines[2].contains("referenced by neither"), "{lines:?}");
    }

    fn agent(role: &str) -> kallipai_common::protocol::TeamInactiveAgent {
        kallipai_common::protocol::TeamInactiveAgent {
            id: kallipai_common::agentid::AgentId::random(),
            role: role.to_string(),
        }
    }

    /// A lock rebuild against a daemon that does not expose the listing
    /// fails loud with the upgrade guidance and writes no archive.
    #[tokio::test]
    async fn lock_rebuild_fails_loud_on_a_404_listing() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/agents"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("{\"agents\": []}"))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/team/inactive-agents"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let client = kallipai_client::TagmaClient::builder(&server.uri())
            .build()
            .unwrap();
        let dir = write_scratch_dir("rebuild-404");
        let args = crate::args::team::TeamLockRebuildArgs {
            dir: Some(dir.path().to_path_buf()),
            json: false,
        };
        let err = run_lock_rebuild(&client, &args)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("upgrade the daemon"), "{err}");
        assert!(err.contains("lock-rebuild"), "{err}");
        assert!(!dir.path().join("tagma.lock").exists());
    }
}
