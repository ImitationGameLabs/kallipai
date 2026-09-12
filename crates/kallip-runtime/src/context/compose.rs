//! Context composition: assembles layers into `Vec<Message>`, and owns the
//! multimodal assembly seam: [`ingest_message`] builds the multimodal user
//! message from fetched media bytes plus the sidecar text. The ingest route
//! and the restore-time re-assembly path both go through here.

use super::Turn;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use just_llm_client::types::generation::{ContentPart, ImageSource, Message, MessageContent};
use tokio::sync::Mutex;

use super::manifest::PinAttachment;
use super::store::ContextStore;

/// Build the context for the next LLM call.
///
/// `turns` are stored `[pinned…][conversation…]` (see `ContextStore::pinned_turn_count`), so a
/// single iteration yields persistent pinned context first, then the conversation in order.
/// Returns all messages without budget filtering — the caller is responsible for estimating
/// tokens and triggering summarize_and_evict.
pub async fn compose_context(store: Arc<Mutex<ContextStore>>) -> Vec<Message> {
    let guard = store.lock().await;
    let mut messages = Vec::new();
    for turn in guard.turns() {
        messages.extend(turn.messages.iter().cloned());
    }
    messages
}

/// One fetched image: the media bytes plus their type (`image/png`, ...).
pub struct IngestImage {
    pub media_type: String,
    pub bytes: Vec<u8>,
}

/// Assemble the user message for an attachment ingest: the text part (the
/// caption, or the rendered placeholder the caller chose) followed by one
/// image part per fetched attachment. Images ride as
/// [`ImageSource::Base64`] — the upstream provider adapters convert to each
/// provider's wire shape.
pub fn ingest_message(text: &str, images: &[IngestImage]) -> Message {
    let mut parts = Vec::with_capacity(images.len() + 1);
    parts.push(ContentPart::Text {
        text: text.to_owned(),
    });
    for image in images {
        parts.push(ContentPart::Image {
            source: ImageSource::Base64 {
                data: STANDARD.encode(&image.bytes),
                media_type: image.media_type.clone(),
            },
            detail: None,
        });
    }
    Message::user_parts(parts)
}

/// The attachment references a pinned message's pointer lines name: each
/// `[image <uuid>]` line is paired, in order, with an image part's media
/// type. Parts-mode only — a plain-text message names no bytes to carry.
/// An image part with no pointer line (or a non-stored source, like a
/// remote URL) has nothing to re-fetch on restore and is logged as dropped.
pub(crate) fn extract_pin_attachments(msg: &Message) -> Vec<PinAttachment> {
    let Some(parts) = msg.content_parts() else {
        return Vec::new();
    };
    if !parts.iter().any(|p| matches!(p, ContentPart::Image { .. })) {
        return Vec::new();
    }
    let text = parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut ids = pointer_record_ids(&text).into_iter();
    let mut refs = Vec::new();
    let mut dropped = 0usize;
    for part in parts {
        let ContentPart::Image { source, .. } = part else {
            continue;
        };
        let ImageSource::Base64 { data, media_type } = source else {
            // Not a stored record (e.g. a remote URL): nothing to re-fetch.
            dropped += 1;
            continue;
        };
        let Some(record_id) = ids.next() else {
            dropped += 1;
            continue;
        };
        // Hash the live bytes for the mirror anchor: every write-through
        // path seeds the local store, so restore resolves this locally and
        // only reaches the files service when the copy is missing. A decode
        // failure just drops the anchor -- the reference stays files-backed.
        let blob_id = STANDARD.decode(data).ok().map(|bytes| {
            kallip_blob_store::BlobId::for_bytes(&bytes)
                .as_str()
                .to_owned()
        });
        refs.push(PinAttachment {
            record_id,
            media_type: media_type.clone(),
            blob_id,
        });
    }
    if dropped > 0 {
        tracing::warn!(
            dropped,
            "pinned message image parts exceed their pointer lines; pin saved as text only",
        );
    }
    refs
}

/// Scan text for `[image <uuid>]` pointer lines, in order. Bracket
/// contents that do not parse as a record id are skipped.
fn pointer_record_ids(text: &str) -> Vec<uuid::Uuid> {
    let mut ids = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[image ") {
        rest = &rest[start + "[image ".len()..];
        let Some(end) = rest.find(']') else {
            break;
        };
        if let Ok(id) = rest[..end].trim().parse::<uuid::Uuid>() {
            ids.push(id);
        }
        rest = &rest[end + 1..];
    }
    ids
}

/// The per-reference fetch verdict. The fetcher owns the HTTP semantics;
/// the re-assembly pass only needs the three-way classification.
#[derive(Debug)]
pub enum FetchedImage {
    /// The bytes arrived; the media type rides the sidecar reference.
    Bytes(Vec<u8>),
    /// The media is deterministically gone (the files record was deleted).
    Gone,
    /// A transient failure (5xx / timeout / network) with its description.
    Transient(String),
}

/// What one restore-time re-assembly pass gave up on or deferred.
pub struct ReassemblyReport {
    /// `(turn_id, record_id, reason)` — deterministic failures. The caller
    /// records `SystemEvent::ReferenceInvalidated` for each so later passes
    /// and the wake modality gate skip them.
    pub invalidated: Vec<(u64, uuid::Uuid, String)>,
    /// `(turn_id, record_id, reason)` — transient failures. The turn stays
    /// text-only and the reference stays valid: the bytes may come back,
    /// and the next restore tries again.
    pub skipped: Vec<(u64, uuid::Uuid, String)>,
}

/// Retry budget for a transient reference fetch within one restore pass.
const REFETCH_ATTEMPTS: usize = 3;

/// Drive one reference's fetch through the transient-retry budget:
/// `REFETCH_ATTEMPTS` tries with backoff, then the last transient
/// verdict stands. Deterministic verdicts (`Bytes`/`Gone`) return
/// immediately. Shared by the conversation-window pass and the pins pass.
async fn fetch_with_retry(
    record_id: uuid::Uuid,
    blob_id: Option<String>,
    fetch: &mut impl FnMut(
        uuid::Uuid,
        Option<String>,
    )
        -> std::pin::Pin<Box<dyn std::future::Future<Output = FetchedImage> + Send>>,
) -> FetchedImage {
    let mut attempt = 0;
    loop {
        match (fetch)(record_id, blob_id.clone()).await {
            FetchedImage::Transient(reason) => {
                attempt += 1;
                if attempt >= REFETCH_ATTEMPTS {
                    return FetchedImage::Transient(reason);
                }
                tokio::time::sleep(std::time::Duration::from_millis(200 * attempt as u64)).await;
            }
            other => return other,
        }
    }
}

/// Restore-time image re-assembly: the compose-path twin of the live
/// ingest. Hydrated turns carry the text form only (the caption and the
/// pointer line) — this pass fetches each sidecar reference's bytes and
/// swaps the assembled multimodal message back in, so a restarted agent's
/// context matches what it held before the restart. References recorded
/// as invalidated (see `SystemEvent::ReferenceInvalidated`) are skipped:
/// a deterministic failure is marked once, never replayed every boot.
///
/// A missing media file (`FetchedImage::Gone`) is deterministic and
/// reported for invalidation; a transient fetch failure retries within
/// the pass and then leaves the turn text-only without invalidating.
/// Either way the pass never fails the restore — text-only boots are
/// always possible, images are additive.
pub async fn reassemble_attachments(
    store: &mut ContextStore,
    agent_dir: &std::path::Path,
    // Boxed so the closure can be process-state-driven without dragging the AsyncFn
    // trait's lifetime generalization through every `Send` bound up the boot path.
    // The second argument is the reference's local-mirror hash (absent for
    // pre-mirror records): the fetcher decides how to use it; this pass stays
    // store-agnostic and only classifies verdicts.
    mut fetch: impl FnMut(
        uuid::Uuid,
        Option<String>,
    )
        -> std::pin::Pin<Box<dyn std::future::Future<Output = FetchedImage> + Send>>,
) -> ReassemblyReport {
    let mut report = ReassemblyReport {
        invalidated: Vec::new(),
        skipped: Vec::new(),
    };
    let invalidated = crate::history::scan_invalidated_refs(agent_dir);
    let wanted: Vec<u64> = store.turns().iter().map(|t| t.id.0).collect();
    let sidecar = crate::history::collect_sidecar_refs(agent_dir, &wanted, &invalidated);
    for (turn_id, refs) in sidecar {
        let Some(turn) = store.turns_mut().iter_mut().find(|t| t.id.0 == turn_id) else {
            continue;
        };
        // Ingest turns are single-message by construction (the route pushes
        // exactly the assembled user message); anything else carrying sidecar
        // refs would be a recording bug — leave it untouched rather than
        // guess which message the refs belong to.
        if turn.messages.len() != 1 {
            continue;
        }
        let Some(text) = turn.messages[0].content().map(str::to_owned) else {
            // Pinned image turns hold their assembled parts (pins persist the
            // live message), so there is nothing to re-assemble for them.
            continue;
        };
        let mut images = Vec::new();
        for r in &refs {
            match fetch_with_retry(r.record_id, r.blob_id.clone(), &mut fetch).await {
                FetchedImage::Bytes(bytes) => {
                    images.push(IngestImage {
                        media_type: r.media_type.clone(),
                        bytes,
                    });
                }
                FetchedImage::Gone => {
                    report.invalidated.push((
                        turn_id,
                        r.record_id,
                        "files record no longer exists".to_owned(),
                    ));
                }
                FetchedImage::Transient(reason) => {
                    report.skipped.push((turn_id, r.record_id, reason));
                }
            }
        }
        if images.is_empty() {
            continue;
        }
        let assembled = ingest_message(&text, &images);
        turn.messages = vec![assembled];
        turn.estimated_tokens = Turn::estimate_tokens(&turn.messages);
        // The prefix changed shape (bytes replaced the pointer line), so the
        // persisted token anchor can no longer be trusted for this round.
        store.mark_needs_full_estimate();
    }
    report
}

/// The pins-layer twin of [`reassemble_attachments`]: pinned turns are
/// persisted as text plus `PinAttachment` references, so restore
/// fetches each reference's bytes and swaps the assembled multimodal
/// message back in — the same mechanism the conversation window uses,
/// with the references carried by the pinned turns themselves
/// (extracted at pin time from the pointer lines — pins have no history sidecars).
///
/// Invalidation semantics match: a reference recorded as invalidated is
/// skipped (a deterministic failure is marked once, never replayed every
/// boot), a `Gone` fetch is reported for invalidation (the pin stays
/// text-only; the pointer line remains, so the text form stays
/// truthful), and a transient failure leaves the pin text-only for the
/// next restore to retry. The pass never fails the restore.
pub async fn reassemble_pin_attachments(
    store: &mut ContextStore,
    agent_dir: &std::path::Path,
    mut fetch: impl FnMut(
        uuid::Uuid,
        Option<String>,
    )
        -> std::pin::Pin<Box<dyn std::future::Future<Output = FetchedImage> + Send>>,
) -> ReassemblyReport {
    let mut report = ReassemblyReport {
        invalidated: Vec::new(),
        skipped: Vec::new(),
    };
    let invalidated = crate::history::scan_invalidated_refs(agent_dir);
    let pin_ids: Vec<u64> = store.pinned_turns().map(|t| t.id.0).collect();
    for turn_id in pin_ids {
        let Some(turn) = store.turns_mut().iter_mut().find(|t| t.id.0 == turn_id) else {
            continue;
        };
        if turn.messages.len() != 1 {
            continue;
        };
        let live: Vec<PinAttachment> = turn
            .pinned_attachments()
            .iter()
            .filter(|r| !invalidated.contains(&(turn_id, r.record_id)))
            .cloned()
            .collect();
        if live.is_empty() {
            continue;
        };
        let text = turn.messages[0].content().map(str::to_owned);
        let mut images = Vec::new();
        let mut gone = Vec::new();
        for r in &live {
            match fetch_with_retry(r.record_id, r.blob_id.clone(), &mut fetch).await {
                FetchedImage::Bytes(bytes) => {
                    images.push(IngestImage {
                        media_type: r.media_type.clone(),
                        bytes,
                    });
                }
                FetchedImage::Gone => {
                    gone.push(r.record_id);
                    report.invalidated.push((
                        turn_id,
                        r.record_id,
                        "files record no longer exists".to_owned(),
                    ));
                }
                FetchedImage::Transient(reason) => {
                    report.skipped.push((turn_id, r.record_id, reason));
                }
            }
        }
        if !gone.is_empty() {
            // A deterministically-gone reference must not ride back into
            // pins.json on the next projection: drop it from the turn.
            if let Some(attachments) = turn.pinned_attachments_mut() {
                attachments.retain(|a| !gone.contains(&a.record_id));
            }
        }
        if images.is_empty() {
            continue;
        };
        let Some(text) = text else {
            continue;
        };
        let assembled = ingest_message(&text, &images);
        turn.messages = vec![assembled];
        turn.estimated_tokens = Turn::estimate_tokens(&turn.messages);
        // The prefix changed shape (bytes replaced the pointer line), so the
        // persisted token anchor can no longer be trusted for this round.
        store.mark_needs_full_estimate();
    }
    report
}

/// Whether a message carries any image content part.
pub(crate) fn message_has_images(message: &Message) -> bool {
    message
        .content_parts()
        .is_some_and(|parts| parts.iter().any(|p| matches!(p, ContentPart::Image { .. })))
}

/// The image-stripped twin of one message: text parts kept, image parts
/// gone. A parts message with no text parts left becomes a single
/// [`crate::context::IMAGE_PLACEHOLDER`] text part — an empty body would
/// be a wire-shape rejection of its own. `None` when the message carries
/// no images and needs no strip.
pub(crate) fn strip_message_images(message: &Message) -> Option<Message> {
    let parts = message.content_parts()?;
    if !message_has_images(message) {
        return None;
    }
    let mut kept: Vec<ContentPart> = parts
        .iter()
        .filter(|p| matches!(p, ContentPart::Text { .. }))
        .cloned()
        .collect();
    if kept.is_empty() {
        kept.push(ContentPart::Text {
            text: crate::context::IMAGE_PLACEHOLDER.to_owned(),
        });
    }
    Some(match message {
        Message::User { .. } => Message::User {
            content: MessageContent::Parts(kept),
        },
        _ => Message::System {
            content: MessageContent::Parts(kept),
        },
    })
}

/// The pins-document text form of one message: image parts stripped (see
/// [`strip_message_images`]) and the kept text parts flattened into a
/// plain text message. A message without images clones through unchanged.
pub(crate) fn pin_text_form(message: &Message) -> Message {
    match strip_message_images(message) {
        Some(stripped) => {
            let text = stripped
                .content_parts()
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| match p {
                            ContentPart::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            Message::user(text)
        }
        None => message.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::context::store::AgenticContext;
    use crate::history::HistoryWriter;
    use just_llm_client::types::generation::MessageContent;
    #[test]
    fn ingest_message_assembles_text_then_base64_image_parts() {
        let message = ingest_message(
            "a chart",
            &[IngestImage {
                media_type: "image/png".to_owned(),
                bytes: vec![1, 2, 3],
            }],
        );
        let Message::User { content } = &message else {
            panic!("expected a user message");
        };
        let MessageContent::Parts(parts) = content else {
            panic!("expected parts content");
        };
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[0],
            ContentPart::Text {
                text: "a chart".to_owned()
            }
        );
        match &parts[1] {
            ContentPart::Image {
                source: ImageSource::Base64 { data, media_type },
                detail: None,
            } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(data, &STANDARD.encode([1, 2, 3]));
            }
            other => panic!("expected a base64 image part, got {other:?}"),
        }
    }

    // -- restore-time re-assembly -----------------------------------------

    /// Minimal PNG header (only the bytes the dimension parser reads).
    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut b = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&height.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0]);
        b
    }

    /// History holding the text form of turn 0 with one image reference.
    fn seed_history_with_image(
        dir: &std::path::Path,
        record_id: uuid::Uuid,
        blob_id: Option<&str>,
    ) {
        HistoryWriter::new(dir.to_owned())
            .append(
                Some(0),
                &[Message::user("[image {id}] caption\n[image x]")],
                8,
                crate::history::RecordKind::Turn,
                None,
                &[crate::history::AttachmentRef {
                    modality: kallip_common::protocol::Modality::Image,
                    record_id,
                    media_type: "image/png".to_owned(),
                    blob_id: blob_id.map(str::to_owned),
                    caption: Some("caption".to_owned()),
                }],
            )
            .unwrap();
    }

    fn text_of(turn_messages: &[Message]) -> String {
        turn_messages
            .iter()
            .filter_map(|m| m.content().map(str::to_owned))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn reassembly_rebuilds_the_parts_message_from_the_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let record_id = uuid::Uuid::from_u128(0xA11CE);
        seed_history_with_image(dir.path(), record_id, None);

        let mut store = ContextStore::new();
        store.push_turn(vec![Message::user("[image x] caption")]);
        let report = reassemble_attachments(&mut store, dir.path(), |id, _blob| {
            Box::pin(async move {
                assert_eq!(id, record_id);
                FetchedImage::Bytes(png_header(64, 48))
            })
        })
        .await;
        assert!(report.invalidated.is_empty());
        assert!(report.skipped.is_empty());
        assert!(
            store.needs_full_estimate(),
            "prefix changed → full estimate"
        );

        let turn = &store.turns()[0];
        let Message::User {
            content: MessageContent::Parts(parts),
        } = &turn.messages[0]
        else {
            panic!("expected the assembled parts message");
        };
        assert_eq!(parts.len(), 2);
        assert!(
            matches!(&parts[1], ContentPart::Image {
                source: ImageSource::Base64 { data, media_type },
                detail: None,
            } if media_type == "image/png"
                && data == &STANDARD.encode(png_header(64, 48))),
            "the fetched bytes ride as Base64: {parts:?}"
        );
    }

    #[tokio::test]
    async fn gone_reference_is_reported_once_and_never_fetched_again() {
        let dir = tempfile::tempdir().unwrap();
        let record_id = uuid::Uuid::from_u128(0xB0B);
        seed_history_with_image(dir.path(), record_id, None);

        let mut store = ContextStore::new();
        store.push_turn(vec![Message::user("[image x] caption")]);

        let report = reassemble_attachments(&mut store, dir.path(), |_, _blob| {
            Box::pin(async move { FetchedImage::Gone })
        })
        .await;
        assert_eq!(report.invalidated.len(), 1);
        assert_eq!(report.invalidated[0].0, 0);
        assert_eq!(report.invalidated[0].1, record_id);
        assert!(text_of(&store.turns()[0].messages).contains("[image x]"));

        // Record the invalidation (as the caller does) and re-run: the
        // loop cut — the deterministic failure is not replayed, the
        // fetcher never fires again.
        HistoryWriter::new(dir.path().to_owned())
            .append(
                None,
                &[],
                0,
                crate::history::RecordKind::System,
                Some(crate::history::SystemEvent::ReferenceInvalidated {
                    turn_id: 0,
                    record_id,
                    reason: "files record no longer exists".to_owned(),
                }),
                &[],
            )
            .unwrap();
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = fetches.clone();
        let report = reassemble_attachments(&mut store, dir.path(), move |id, _blob| {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = id;
                FetchedImage::Bytes(png_header(8, 8))
            })
        })
        .await;
        assert!(report.invalidated.is_empty());
        assert!(report.skipped.is_empty());
        assert_eq!(fetches.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn transient_failures_stay_text_only_without_invalidation() {
        let dir = tempfile::tempdir().unwrap();
        let record_id = uuid::Uuid::from_u128(0xC0FFEE);
        seed_history_with_image(dir.path(), record_id, None);

        let mut store = ContextStore::new();
        store.push_turn(vec![Message::user("[image x] caption")]);

        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = fetches.clone();
        let report = reassemble_attachments(&mut store, dir.path(), move |id, _blob| {
            let counter = counter.clone();
            let _ = id;
            Box::pin(async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                FetchedImage::Transient("upstream 503".to_owned())
            })
        })
        .await;
        assert!(report.invalidated.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].1, record_id);
        assert!(text_of(&store.turns()[0].messages).contains("[image x]"));
        assert_eq!(
            fetches.load(std::sync::atomic::Ordering::SeqCst),
            REFETCH_ATTEMPTS,
            "each transient ref is attempted exactly the retry budget"
        );

        // Nothing was invalidated: the next pass tries again.
        let report = reassemble_attachments(&mut store, dir.path(), |_, _blob| {
            Box::pin(async move { FetchedImage::Bytes(png_header(8, 8)) })
        })
        .await;
        assert!(report.invalidated.is_empty());
        assert!(report.skipped.is_empty());
        assert!(message_has_images(&store.turns()[0].messages[0]));
    }

    // -- pins re-assembly ---------------------------------------------------

    #[test]
    fn extract_keeps_plain_text_pins_unchanged() {
        let msg = Message::user("plain note");
        assert!(extract_pin_attachments(&msg).is_empty());
    }

    #[test]
    fn extract_pairs_pointer_lines_to_media_types() {
        let record_id = uuid::Uuid::from_u128(0xB0B);
        let msg = ingest_message(
            &format!("caption\n[image {record_id}]"),
            &[IngestImage {
                media_type: "image/png".to_owned(),
                bytes: png_header(8, 8),
            }],
        );
        let refs = extract_pin_attachments(&msg);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].record_id, record_id);
        assert_eq!(refs[0].media_type, "image/png");
    }

    #[test]
    fn extract_drops_images_without_pointer_lines() {
        let msg = ingest_message(
            "no pointers here",
            &[IngestImage {
                media_type: "image/png".to_owned(),
                bytes: png_header(8, 8),
            }],
        );
        assert!(extract_pin_attachments(&msg).is_empty());
    }

    fn parts_pin(record_id: uuid::Uuid) -> Message {
        ingest_message(
            &format!("caption\n[image {record_id}]"),
            &[IngestImage {
                media_type: "image/png".to_owned(),
                bytes: png_header(8, 8),
            }],
        )
    }

    #[tokio::test]
    async fn pin_reassembly_swaps_the_bytes_back_in() {
        let dir = tempfile::tempdir().unwrap();
        let record_id = uuid::Uuid::from_u128(0xB0B);
        let mut store = ContextStore::new();
        store.pin("shot", parts_pin(record_id)).unwrap();
        let report = reassemble_pin_attachments(&mut store, dir.path(), |id, _blob| {
            Box::pin(async move {
                assert_eq!(id, record_id);
                FetchedImage::Bytes(png_header(64, 48))
            })
        })
        .await;
        assert!(report.invalidated.is_empty());
        assert!(report.skipped.is_empty());
        assert!(
            store.needs_full_estimate(),
            "prefix changed → full estimate"
        );
        assert!(message_has_images(
            &store.pinned_turns().next().unwrap().messages[0]
        ));
    }

    #[tokio::test]
    async fn pin_gone_reference_invalidates_and_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let record_id = uuid::Uuid::from_u128(0xB0B);
        let mut store = ContextStore::new();
        store.pin("shot", parts_pin(record_id)).unwrap();
        let report = reassemble_pin_attachments(&mut store, dir.path(), |_, _blob| {
            Box::pin(async move { FetchedImage::Gone })
        })
        .await;
        assert_eq!(report.invalidated.len(), 1);
        assert_eq!(report.invalidated[0].1, record_id);
        assert!(
            store
                .pinned_turns()
                .next()
                .unwrap()
                .pinned_attachments()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn pin_transient_failures_stay_textual_within_the_retry_budget() {
        let dir = tempfile::tempdir().unwrap();
        let record_id = uuid::Uuid::from_u128(0xB0B);
        let mut store = ContextStore::new();
        store.pin("shot", parts_pin(record_id)).unwrap();
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = fetches.clone();
        let report = reassemble_pin_attachments(&mut store, dir.path(), move |id, _blob| {
            let counter = counter.clone();
            let _ = id;
            Box::pin(async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                FetchedImage::Transient("upstream 503".to_owned())
            })
        })
        .await;
        assert!(report.invalidated.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(
            fetches.load(std::sync::atomic::Ordering::SeqCst),
            REFETCH_ATTEMPTS,
            "each transient ref is attempted exactly the retry budget"
        );
        assert_eq!(
            store
                .pinned_turns()
                .next()
                .unwrap()
                .pinned_attachments()
                .len(),
            1,
            "transient failures keep the reference for the next restore"
        );
        assert!(message_has_images(
            &store.pinned_turns().next().unwrap().messages[0]
        ));
    }

    #[tokio::test]
    async fn pin_invalidated_reference_is_not_refetched() {
        let dir = tempfile::tempdir().unwrap();
        let record_id = uuid::Uuid::from_u128(0xB0B);
        let mut store = ContextStore::new();
        store.pin("shot", parts_pin(record_id)).unwrap();
        let turn_id = store.pinned_turns().next().unwrap().id.0;
        HistoryWriter::new(dir.path().to_owned())
            .append(
                None,
                &[],
                0,
                crate::history::RecordKind::System,
                Some(crate::history::SystemEvent::ReferenceInvalidated {
                    turn_id,
                    record_id,
                    reason: "files record no longer exists".to_owned(),
                }),
                &[],
            )
            .unwrap();
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = fetches.clone();
        let report = reassemble_pin_attachments(&mut store, dir.path(), move |id, _blob| {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = id;
                FetchedImage::Bytes(png_header(8, 8))
            })
        })
        .await;
        assert!(report.invalidated.is_empty());
        assert_eq!(fetches.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn blob_anchor_rides_the_fetch_call() {
        let dir = tempfile::tempdir().unwrap();
        let record_id = uuid::Uuid::from_u128(0xD00D);
        let anchor = kallip_blob_store::BlobId::for_bytes(&png_header(8, 8))
            .as_str()
            .to_owned();
        seed_history_with_image(dir.path(), record_id, Some(&anchor));

        let mut store = ContextStore::new();
        store.push_turn(vec![Message::user("[image x] caption")]);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let report = reassemble_attachments(&mut store, dir.path(), move |id, blob| {
            let sink = sink.clone();
            Box::pin(async move {
                assert_eq!(id, record_id);
                sink.lock().unwrap().push(blob);
                FetchedImage::Bytes(png_header(64, 48))
            })
        })
        .await;
        assert!(report.invalidated.is_empty());
        assert!(report.skipped.is_empty());
        assert_eq!(
            *seen.lock().unwrap(),
            vec![Some(anchor)],
            "the stored anchor must reach the fetcher, not a folded None"
        );
    }

    #[test]
    fn extract_computes_the_pin_anchor_from_the_live_bytes() {
        let record_id = uuid::Uuid::from_u128(0xB0B);
        let bytes = png_header(8, 8);
        let msg = ingest_message(
            &format!("caption\n[image {record_id}]"),
            &[IngestImage {
                media_type: "image/png".to_owned(),
                bytes: bytes.clone(),
            }],
        );
        let refs = extract_pin_attachments(&msg);
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].blob_id.as_deref(),
            Some(kallip_blob_store::BlobId::for_bytes(&bytes).as_str()),
            "the anchor is the content address of the decoded bytes"
        );
    }
}
