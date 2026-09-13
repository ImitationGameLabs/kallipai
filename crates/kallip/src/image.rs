//! `kallip image read` — the agent-side image ingest entrance.

//! Self-scoped: the target agent id comes from `KALLIP_ID` (the command
//! runs in the agent shell). A local path stores the bytes in the
//! tagma's local blob store; `--blob` re-ingests a stored copy; `--id`
//! keeps reading a files record. The tagma enforces the modalities.

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
    match disambiguate(args)? {
        Target::Id(id) => {
            let req = AttachmentIngestRequest {
                record_id: id,
                modality: Modality::Image,
                media_type: args.media_type.clone(),
                caption: args.caption.clone(),
            };
            let response = client.attachment_ingest(&agent_id, &req).await?;
            println!("Ingested {id} into turn {}.", response.turn_id);
        }
        Target::Path(local) => {
            let (bytes, media_type, name) = read_local_image(local, args).await?;
            let response = client
                .attachment_store(
                    &agent_id,
                    bytes,
                    &media_type,
                    &name,
                    args.caption.as_deref(),
                )
                .await?;
            println!(
                "Stored locally as blob {} and recorded at turn {}.",
                response.blob_id, response.turn_id
            );
            println!(
                "Read it again later with `kallip image read --blob {}`.",
                response.blob_id
            );
        }
        Target::Blob(hash) => {
            let media_type = args
                .media_type
                .clone()
                .unwrap_or_else(|| "image/png".to_owned());
            let response = client
                .attachment_store_blob(&agent_id, hash, &media_type, args.caption.as_deref())
                .await?;
            println!(
                "Re-ingested blob {} into turn {}.",
                response.blob_id, response.turn_id
            );
        }
    }
    Ok(())
}

/// The resolved form of `image read`'s target: a local path, a blob
/// content address, or a files record id.
enum Target<'a> {
    Id(uuid::Uuid),
    Path(&'a str),
    Blob(&'a str),
}

/// `--id` and `--path` pin the form; otherwise a parseable UUID without path
/// separators reads as a record id and anything else as a local path.
fn disambiguate(args: &ImageReadArgs) -> Result<Target<'_>> {
    if args.target.is_empty() {
        anyhow::bail!("no target given; pass a record id or a local image path");
    }
    if args.blob {
        return Ok(Target::Blob(&args.target));
    }
    if args.id {
        return Ok(Target::Id(parse_record_id(&args.target)?));
    }
    if args.path {
        return Ok(Target::Path(&args.target));
    }
    if !args.target.contains('/')
        && !args.target.contains('\\')
        && let Ok(id) = args.target.parse()
    {
        return Ok(Target::Id(id));
    }
    Ok(Target::Path(&args.target))
}

fn parse_record_id(raw: &str) -> Result<uuid::Uuid> {
    raw.parse()
        .context("record id must be a UUID (see `kallip file ls`)")
}

/// Read a local image file and resolve its media type, from
/// `--media-type` or the file extension. Returns the bytes, the
/// media type, and the file name (for the tracing header).
async fn read_local_image(local: &str, args: &ImageReadArgs) -> Result<(Vec<u8>, String, String)> {
    let file = std::path::Path::new(local);
    let meta = tokio::fs::metadata(file).await.map_err(|e| {
        anyhow::anyhow!(
            "cannot read {local} ({e}); pass --id or --blob to read an already-stored image"
        )
    })?;
    if meta.is_dir() {
        anyhow::bail!("{local} is a directory; pass an image file");
    }
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("cannot derive a file name from {local}"))?;
    let media_type = match args.media_type.clone() {
        Some(mt) => mt,
        None => media_type_for(name)?.to_owned(),
    };
    let bytes = tokio::fs::read(file)
        .await
        .context("failed to read the image file")?;
    Ok((bytes, media_type, name.to_owned()))
}

/// The media type for a stored image file, from its extension. Unknown
/// extensions fall back to `image/png`. SVG is refused: it is not a
/// raster image the context pipeline renders. An explicit --media-type
/// bypasses this table.
fn media_type_for(name: &str) -> anyhow::Result<&'static str> {
    match std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => Ok("image/jpeg"),
        Some("gif") => Ok("image/gif"),
        Some("webp") => Ok("image/webp"),
        Some("bmp") => Ok("image/bmp"),
        Some("tif") | Some("tiff") => Ok("image/tiff"),
        Some("ico") => Ok("image/x-icon"),
        Some("avif") => Ok("image/avif"),
        Some("svg") => Err(anyhow::anyhow!(
            "svg is not a raster image; convert to png or pass --media-type"
        )),
        _ => Ok("image/png"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_for(target: &str) -> ImageReadArgs {
        ImageReadArgs {
            target: target.to_owned(),
            id: false,
            blob: false,
            path: false,
            media_type: None,
            caption: None,
        }
    }

    #[test]
    fn bare_uuid_reads_as_a_record_id() {
        let id = "0f0e0d0c-0b0a-4938-8273-6d60d1603a51";
        assert!(matches!(
            disambiguate(&args_for(id)).unwrap(),
            Target::Id(_)
        ));
    }

    #[test]
    fn uuid_with_a_separator_reads_as_a_path() {
        let target = "0f0e0d0c-0b0a-4938-8273-6d60d1603a51.png";
        assert!(matches!(
            disambiguate(&args_for(target)).unwrap(),
            Target::Path(_)
        ));
    }

    #[test]
    fn non_uuid_target_reads_as_a_path() {
        assert!(matches!(
            disambiguate(&args_for("shot.png")).unwrap(),
            Target::Path(_)
        ));
    }

    #[test]
    fn forced_id_rejects_a_non_uuid() {
        let mut args = args_for("shot.png");
        args.id = true;
        assert!(disambiguate(&args).is_err());
    }

    #[test]
    fn forced_path_wins_over_a_uuid_shape() {
        let mut args = args_for("0f0e0d0c-0b0a-4938-8273-6d60d1603a51");
        args.path = true;
        assert!(matches!(disambiguate(&args).unwrap(), Target::Path(_)));
    }

    #[test]
    fn media_type_follows_the_extension() {
        assert_eq!(media_type_for("a.png").unwrap(), "image/png");
        assert_eq!(media_type_for("b.JPG").unwrap(), "image/jpeg");
        assert_eq!(media_type_for("c.webp").unwrap(), "image/webp");
        assert_eq!(media_type_for("d").unwrap(), "image/png");
        assert_eq!(media_type_for("e.txt").unwrap(), "image/png");
        assert_eq!(media_type_for("f.bmp").unwrap(), "image/bmp");
        assert_eq!(media_type_for("g.tiff").unwrap(), "image/tiff");
        assert_eq!(media_type_for("h.ico").unwrap(), "image/x-icon");
        assert_eq!(media_type_for("i.avif").unwrap(), "image/avif");
    }

    #[test]
    fn svg_is_refused_with_an_actionable_message() {
        let err = media_type_for("logo.svg").unwrap_err();
        assert!(err.to_string().contains("--media-type"));
    }

    #[test]
    fn empty_target_is_rejected() {
        assert!(disambiguate(&args_for("")).is_err());
    }

    #[test]
    fn blob_flag_pins_the_blob_form() {
        let mut args = args_for("sha256-deadbeef");
        args.blob = true;
        assert!(matches!(disambiguate(&args).unwrap(), Target::Blob(_)));
    }

    #[test]
    fn a_bare_blob_hash_is_never_guessed() {
        // No separators and not a UUID: the auto rule reads a path. A
        // blob id is only used when --blob pins the form.
        assert!(matches!(
            disambiguate(&args_for("deadbeefdeadbeef")).unwrap(),
            Target::Path(_)
        ));
    }
}
