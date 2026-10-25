//! `kallipai-telemetry-prune` -- bound the local observability stack's data.
//!
//! Defaults to a dry run: it prints every unit it would delete and the totals
//! before and after, and touches nothing. `--apply` deletes the planned units
//! that are still there and reports each one it skips, with the reason.
//! The root is not a flag: it is the same path the stack mounts (see the crate
//! docs), so a run cannot be pointed at someone else's directory. The two
//! budgets may be moved through the environment (KALLIPAI_TELEMETRY_MAX_BYTES,
//! KALLIPAI_TELEMETRY_MAX_AGE_HOURS) for a one-off investigation.
//!
//! A root that does not exist yet is a no-op with a zero exit: on a machine
//! where the stack has never run, a scheduled prune must stay quiet.
use anyhow::Result;
use clap::Parser;
use kallipai_telemetry_prune::{
    Policy, apply, default_root, existing_root, plan, render, render_applied, scan,
};
use std::env;
use std::time::SystemTime;

#[derive(Parser, Debug)]
#[command(
    name = "kallipai-telemetry-prune",
    about = "Delete the oldest tempo blocks and wal segments to keep the local telemetry stack within its age and size budget"
)]
struct Cli {
    /// Delete the planned units instead of only reporting them.
    #[arg(long)]
    apply: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = default_root(
        env::var("XDG_DATA_HOME").ok().as_deref(),
        env::var("HOME").ok().as_deref(),
    )?;
    let Some(root) = existing_root(&root)? else {
        println!(
            "telemetry root {} does not exist; nothing to prune",
            root.display()
        );
        return Ok(());
    };
    let policy = Policy::from_env(|key| env::var(key).ok())?;
    let scanned = scan(&root)?;
    let now = SystemTime::now();
    let planned = plan(&scanned, now, &policy);
    print!("{}", render(&scanned, &planned, &policy, now, !cli.apply));
    if cli.apply {
        let applied = apply(&scanned, &planned)?;
        print!(
            "{}",
            render_applied(&scanned, &applied, planned.deletions.len())
        );
    }
    Ok(())
}
