//! `image` command subtree of the `kallip` CLI (clap derive).

use clap::{Args, Subcommand};

/// The `kallip image` family: read images into the conversation.
#[derive(Subcommand)]
pub(crate) enum ImageCommand {
    /// Ingest an image into this agent's live context. The target is a local
    /// path (stored through the files service first) or a files record id.
    Read(ImageReadArgs),
}

/// Args for `kallip image read`.
#[derive(Args)]
pub(crate) struct ImageReadArgs {
    /// A local image path, or a files-service record id.
    pub target: String,
    /// Interpret the target as a record id.
    #[arg(long, conflicts_with = "path")]
    pub id: bool,
    /// Interpret the target as a local path (e.g. a file literally named
    /// like a UUID).
    #[arg(long, conflicts_with = "id")]
    pub path: bool,
    /// Re-ingest an already-stored blob by its content address. A blob
    /// id is never guessed from a bare target: this flag pins the form.
    #[arg(long, conflicts_with_all = ["id", "path"])]
    pub blob: bool,
    /// Media type of the record (default: derived from the file extension
    /// when storing, else `image/png`).
    #[arg(long, value_name = "TYPE")]
    pub media_type: Option<String>,
    /// Caption carried alongside the reference.
    #[arg(long, value_name = "TEXT")]
    pub caption: Option<String>,
}
