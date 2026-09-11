//! Workspace-binary resolution, mirroring the tagma sandbox harness lookup:
//! `KALLIP_BIN_DIR` (container/dev states it) → `CARGO_BIN_EXE_<name>`
//! (cargo test injects it for same-package bins) → bare name. The
//! bare name is executable only through a PATH-searching spawn:
//! the daemon launching the helper resolves it that way, while
//! the tagma is execve'd by the helper without a PATH search —
//! launch() refuses a non-file tagma resolution with an
//! actionable error instead.

use std::path::PathBuf;

pub fn resolve(name: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("KALLIP_BIN_DIR")
        && let p = std::path::Path::new(&dir).join(name)
        && p.is_file()
    {
        return p;
    }
    let var = format!("CARGO_BIN_EXE_{name}");
    if let Ok(p) = std::env::var(&var) {
        return PathBuf::from(p);
    }
    PathBuf::from(name)
}
