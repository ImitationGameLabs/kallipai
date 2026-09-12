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
    let (record_id, media_type) = match disambiguate(args)? {
        Target::Id(id) => (id, args.media_type.clone()),
        Target::Path(local) => {
            let stored = store_local_image(local, args).await?;
            println!(
                "Stored through the files service into the local content-addressed store (blob {}); recorded at space path {}.",
                stored.blob_id, stored.space_path
            );
            println!("Saved as record {}.", stored.record_id);
            println!(
                "Read it again later with `kallip image read {}`.",
                stored.record_id
            );
            (stored.record_id, Some(stored.media_type))
        }
    };
    let req = AttachmentIngestRequest {
        record_id,
        modality: Modality::Image,
        media_type,
        caption: args.caption.clone(),
    };
    let response = client.attachment_ingest(&agent_id, &req).await?;
    println!("Ingested {record_id} into turn {}.", response.turn_id);
    Ok(())
}

/// The resolved form of `image read`'s target: a files record id or a local path.
enum Target<'a> {
    Id(uuid::Uuid),
    Path(&'a str),
}

/// `--id` and `--path` pin the form; otherwise a parseable UUID without path
/// separators reads as a record id and anything else as a local path.
fn disambiguate(args: &ImageReadArgs) -> Result<Target<'_>> {
    if args.target.is_empty() {
        anyhow::bail!("no target given; pass a record id or a local image path");
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

/// A file stored through the files service, ready to ingest.
struct StoredImage {
    record_id: uuid::Uuid,
    blob_id: String,
    space_path: String,
    media_type: String,
}

/// Store a local file under the caller's private `images/` region (the files
/// service resolves the identity prefix for relative paths). The media type
/// comes from --media-type or the file extension.
async fn store_local_image(local: &str, args: &ImageReadArgs) -> Result<StoredImage> {
    let file = std::path::Path::new(local);
    let meta = tokio::fs::metadata(file).await.map_err(|e| {
        anyhow::anyhow!(
            "cannot read {local} ({e}); pass a record id to read an already-stored image"
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
    let space_path = space_path_for(name);
    let files = kallip::file::FilesClient::from_env()?;
    let put = files.put_file(&space_path, file).await?;
    Ok(StoredImage {
        record_id: put.record_id,
        blob_id: put.blob_id,
        space_path,
        media_type,
    })
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

/// The space path a stored image lands at: the caller's private `images/`
/// region plus the original file name (the files service resolves the
/// identity prefix for relative paths).
fn space_path_for(name: &str) -> String {
    format!("images/{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_for(target: &str) -> ImageReadArgs {
        ImageReadArgs {
            target: target.to_owned(),
            id: false,
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
    fn space_path_lands_in_the_private_images_region() {
        assert_eq!(space_path_for("shot.png"), "images/shot.png");
    }
}
