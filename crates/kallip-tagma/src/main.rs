use anyhow::Result;
use clap::Parser;
use kallip_tagma::{Args, init_logging, install_instance_roots, run};

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Instance roots before logging: log placement itself hangs off the
    // state root, and every later path resolves through them. Failure is
    // the unnamed-boot case — stderr-only, as init_logging degrades anyway.
    install_instance_roots()?;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    init_logging(&filter);

    // The whole body lives in `run` so a fatal error at any point —
    // startup or the serving loop — is logged through the subscriber
    // before main returns it; the Rust runtime's stderr report alone
    // would bypass the instance log file. Returning the error keeps
    // the exit code; in the stderr-logging mode the runtime line and
    // this one differ in shape (debug form carries the source chain).
    if let Err(e) = run(args).await {
        tracing::error!(error = format!("{e:#}"), "fatal error, exiting");
        return Err(e);
    }
    Ok(())
}
