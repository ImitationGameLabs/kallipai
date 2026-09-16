//! `file` command subtree of the `kallip` CLI (clap derive).

use clap::{Args, Subcommand};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// File commands — files-service client, credentials from the spawn env
// ---------------------------------------------------------------------------

/// The `kallip file` family: content transfer against the files service.
/// The acting principal is the tagma named by the bearer token (the
/// spawn env); `--space self` is its own region, `shared` is the space's
/// shared region.
#[derive(Subcommand)]
pub(crate) enum FileCommand {
    /// Upload a local file to a space path.
    Put(FilePutArgs),
    /// Download a record by id (to --out, or stdout when omitted).
    Get(FileGetArgs),
    /// Deliver a record into another principal's inbox.
    Send(FileSendArgs),
    /// List records in a slice of the caller's space.
    Ls(FileLsArgs),
}

/// Args for `kallip file put`.
#[derive(Args)]
pub(crate) struct FilePutArgs {
    /// Space path to store under (e.g. /users/alice/shared/report.pdf).
    pub path: String,
    /// Local file to upload (streamed, never buffered whole).
    #[arg(long = "file", value_name = "FILE")]
    pub file: PathBuf,
    /// Print the response as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Args for `kallip file get`.
#[derive(Args)]
pub(crate) struct FileGetArgs {
    /// Record id to download.
    pub id: String,
    /// Write the content here instead of stdout.
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
}

/// Args for `kallip file send`. Exactly one target.
#[derive(Args)]
pub(crate) struct FileSendArgs {
    /// Record id to deliver.
    pub id: String,
    /// Deliver into this tagma's inbox (same space required).
    #[arg(
        long,
        value_name = "TAGMA",
        required_unless_present = "to_user",
        group = "file-send-target"
    )]
    pub to_tagma: Option<String>,
    /// Deliver into this user's inbox.
    #[arg(
        long,
        value_name = "USER",
        required_unless_present = "to_tagma",
        group = "file-send-target"
    )]
    pub to_user: Option<String>,
    /// Print the response as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Args for `kallip file ls`.
#[derive(Args)]
pub(crate) struct FileLsArgs {
    /// Which slice: `self` (the caller's own region, inbox included) or
    /// `shared` (the space's shared region).
    #[arg(long, value_parser = ["self", "shared"])]
    pub space: String,
    /// Narrow to paths under this relative prefix (e.g. inbox/).
    #[arg(long)]
    pub prefix: Option<String>,
    /// Max entries (the server clamps to its own cap).
    #[arg(long)]
    pub limit: Option<u64>,
    /// Print the listing as JSON.
    #[arg(long)]
    pub json: bool,
}
