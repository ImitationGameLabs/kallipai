//! `kallip image read` — the agent-side image ingest entrance.
//!
//! Self-scoped: the target agent id comes from `KALLIP_ID` (the command
//! runs in the agent shell). The tagma enforces the bound set's
//! modalities, fetches the bytes from the files service, and records the
//! turn; this face reports the outcome.

use anyhow::{Context, Result};
use kallip_client::TagmaClient;
use kallip_common::protocol::{AttachmentIngestRequest, Modality};

use crate::args::{ImageCommand, ImageReadArgs};

pub(crate) async fn run_image(client: &TagmaClient, cmd: &ImageCommand) -> Result<()> {
    match cmd {
        ImageCommand::Read(args) => run_read(client, args).await,
    }
}

async fn run_read(client: &TagmaClient, args: &ImageReadArgs) -> Result<()> {
    let agent_id = crate::agent_id_from_env()?;
    let record_id = args
        .id
        .parse()
        .context("record id must be a UUID (see `kallip file ls`)")?;
    let req = AttachmentIngestRequest {
        record_id,
        modality: Modality::Image,
        media_type: args.media_type.clone(),
        caption: args.caption.clone(),
    };
    let response = client.attachment_ingest(&agent_id, &req).await?;
    println!("Ingested {record_id} into turn {}.", response.turn_id);
    Ok(())
}
