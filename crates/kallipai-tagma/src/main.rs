use anyhow::Result;
use clap::Parser;
use kallipai_tagma::{Args, env_governance, init_logging, install_instance_roots, run};

fn main() -> Result<()> {
    let args = Args::parse();

    // Instance roots before logging: log placement itself hangs off the
    // state root, and every later path resolves through them. Failure is
    // the unnamed-boot case — stderr-only, as init_logging degrades anyway.
    install_instance_roots()?;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    init_logging(&filter);

    // Env governance before the async runtime exists: edition 2024 marks
    // remove_var unsafe because a concurrent getenv on another thread is
    // undefined, and once the runtime is up its worker threads make that
    // guarantee unprovable. Here the main thread is still the only thread.
    // Agent shells inherit this process env wholesale, so a secret left in
    // it reaches every agent bash; a retired variable fails the boot
    // instead of being silently ignored.
    let operator_pin = env_governance::capture_operator_pin();
    let removed = env_governance::scrub_boot_secrets();
    if !removed.is_empty() {
        tracing::info!(
            keys = ?removed,
            "removed boot secrets from the process environment"
        );
    }
    env_governance::ensure_no_retired_env()?;

    // The whole body lives in `run` so a fatal error at any point —
    // startup or the serving loop — is logged through the subscriber
    // before main returns it; the Rust runtime's stderr report alone
    // would bypass the instance log file. Returning the error keeps
    // the exit code; in the stderr-logging mode the runtime line and
    // this one differ in shape (debug form carries the source chain).
    // The pre-runtime governance block above is the one exception to
    // "the whole body lives in run": its retired-env refusal returns
    // here, reaching stderr and the exit code only, never the
    // instance log.
    // The runtime is built here rather than via #[tokio::main] so the
    // governance window above stays single-threaded.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    if let Err(e) = runtime.block_on(run(args, operator_pin)) {
        tracing::error!(error = format!("{e:#}"), "fatal error, exiting");
        return Err(e);
    }
    Ok(())
}
