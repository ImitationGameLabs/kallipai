//! Context composition: assembles layers into `Vec<Message>`, and owns the
//! multimodal assembly seam: [`ingest_message`] builds the multimodal user
//! message from fetched media bytes plus the sidecar text. The ingest route
//! and the restore-time re-assembly path both go through here.

use super::Turn;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use just_llm_client::types::generation::{ContentPart, ImageSource, Message, MessageContent};
use kallipai_common::protocol::Modality;
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
/// Each image part's bytes are seeded into the attachment store at pin
/// time (content-addressed), so the pin's anchors are self-sufficient;
/// a part with no pointer line, a non-decodable body, or a failed store
/// write is logged as dropped and the pin keeps text only.
pub(crate) fn extract_pin_attachments(
    msg: &Message,
    blobs: Option<&std::sync::Arc<dyn kallipai_blob_store::BlobStore>>,
) -> Vec<PinAttachment> {
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
            // Not a stored source (e.g. a remote URL): nothing to anchor.
            dropped += 1;
            continue;
        };
        let Some(record_id) = ids.next() else {
            dropped += 1;
            continue;
        };
        // Seed the anchor from the live bytes: decode, write the master
        // copy into the attachment store (content-addressed, so writing
        // bytes that are already stored is a no-op), and anchor on the
        // id. A decode or write failure — or no wired store — drops the
        // attachment: the pin is saved as text only, never as a
        // reference whose bytes no store holds.
        let blob_id = STANDARD.decode(data).ok().and_then(|bytes| match blobs {
            Some(blobs) => store_blob_sync(blobs, &bytes),
            None => None,
        });
        let Some(blob_id) = blob_id else {
            dropped += 1;
            continue;
        };
        refs.push(PinAttachment {
            record_id: Some(record_id),
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

/// Synchronous master-copy write for pin-time seeding. The pin path is
/// fully synchronous (the store lock is held), and the write is a
/// low-frequency content-addressed put, so a throwaway current-thread
/// runtime is cheaper and simpler than threading async through the pin
/// chain. `None` on write failure: the caller drops the attachment.
fn store_blob_sync(
    blobs: &std::sync::Arc<dyn kallipai_blob_store::BlobStore>,
    bytes: &[u8],
) -> Option<String> {
    // The write runs on its own OS thread with a throwaway runtime: the
    // pin chain is synchronous and usually already inside a tokio
    // runtime, where block_on would panic. The thread pays a one-shot
    // runtime build per pin -- a low-frequency operation.
    let blobs = blobs.clone();
    let bytes = bytes.to_vec();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        runtime
            .block_on(blobs.put(&mut std::io::Cursor::new(bytes)))
            .ok()
            .map(|outcome| outcome.id.as_str().to_owned())
    })
    .join()
    .ok()
    .flatten()
}

/// The pointer line a reference answers to. Provenance-only record ids
/// (path/blob-form ingests) answer to the nil-id line their ingest wrote.
fn pointer_line(record_id: Option<uuid::Uuid>) -> String {
    format!("[image {}]", record_id.unwrap_or(uuid::Uuid::nil()))
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
    /// The media is deterministically gone (the local master copy is
    /// missing, or the anchor does not parse).
    Gone,
    /// A transient failure (5xx / timeout / network) with its description.
    Transient(String),
}

/// What one restore-time re-assembly pass gave up on or deferred.
pub struct ReassemblyReport {
    /// `(turn_id, blob_id, reason)` — deterministic failures. The caller
    /// records nothing to history — invalidations are not remembered
    /// across restarts; the list feeds the caller's warning log.
    pub invalidated: Vec<(u64, String, String)>,
    /// `(turn_id, blob_id, reason)` — transient failures. The turn stays
    /// text-only and the reference stays valid: the bytes may come back,
    /// and the next restore tries again.
    pub skipped: Vec<(u64, String, String)>,
}

/// Retry budget for a transient reference fetch within one restore pass.
const REFETCH_ATTEMPTS: usize = 3;

/// Drive one reference's fetch through the transient-retry budget:
/// `REFETCH_ATTEMPTS` tries with backoff, then the last transient
/// verdict stands. Deterministic verdicts (`Bytes`/`Gone`) return
/// immediately. Shared by the conversation-window pass and the pins pass.
async fn fetch_with_retry(
    blob_id: String,
    record_id: Option<uuid::Uuid>,
    fetch: &mut impl FnMut(
        String,
        Option<uuid::Uuid>,
    )
        -> std::pin::Pin<Box<dyn std::future::Future<Output = FetchedImage> + Send>>,
) -> FetchedImage {
    let mut attempt = 0;
    loop {
        match (fetch)(blob_id.clone(), record_id).await {
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
/// pointer line) — this pass reads each sidecar reference's master copy
/// from the attachment store and swaps the assembled multimodal message
/// back in, so a restarted agent's context matches what it held before
/// the restart, with zero network.
///
/// A missing copy (`FetchedImage::Gone`) is deterministic and reported
/// for observability; its pointer line is marked "(image unavailable at
/// restore)" in the in-memory message only — the persisted line stays
/// untouched, so the next restore re-evaluates. A transient read failure
/// retries within the pass and then leaves the turn text-only. Either
/// way the pass never fails the restore — text-only boots are always
/// possible, images are additive.
pub async fn reassemble_attachments(
    store: &mut ContextStore,
    sidecar: &BTreeMap<u64, Vec<crate::history::AttachmentRef>>,
    // Boxed so the closure can be process-state-driven without dragging the AsyncFn
    // trait's lifetime generalization through every `Send` bound up the boot path.
    // The first argument is the reference anchor (the content address); the
    // second is the provenance record id, when one exists. This pass stays
    // store-agnostic and only classifies verdicts.
    mut fetch: impl FnMut(
        String,
        Option<uuid::Uuid>,
    )
        -> std::pin::Pin<Box<dyn std::future::Future<Output = FetchedImage> + Send>>,
) -> ReassemblyReport {
    let mut report = ReassemblyReport {
        invalidated: Vec::new(),
        skipped: Vec::new(),
    };
    for (&turn_id, refs) in sidecar {
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
        let Some(mut text) = turn.messages[0].content().map(str::to_owned) else {
            // Pinned image turns hold their assembled parts (pins persist the
            // live message), so there is nothing to re-assemble for them.
            continue;
        };
        let mut text_marked = false;
        let mut images = Vec::new();
        for r in refs {
            match fetch_with_retry(r.blob_id.clone(), r.record_id, &mut fetch).await {
                FetchedImage::Bytes(bytes) => {
                    images.push(IngestImage {
                        media_type: r.media_type.clone(),
                        bytes,
                    });
                }
                FetchedImage::Gone => {
                    report.invalidated.push((
                        turn_id,
                        r.blob_id.clone(),
                        "local blob copy is missing".to_owned(),
                    ));
                    // Mark the pointer line so the member sees why the image
                    // is absent; deduped, so images going missing one by one
                    // never stack repeated markers on one line.
                    let pointer = pointer_line(r.record_id);
                    let marked = format!("{pointer} (image unavailable at restore)");
                    if text.contains(&pointer) && !text.contains(&marked) {
                        text = text.replace(&pointer, &marked);
                        text_marked = true;
                    }
                }
                FetchedImage::Transient(reason) => {
                    report.skipped.push((turn_id, r.blob_id.clone(), reason));
                }
            }
        }
        if text_marked {
            // In-memory presentation only; nothing is written back to the
            // history log or manifest (see the pass doc).
            turn.messages[0] = Message::user(text.clone());
        }
        if images.is_empty() {
            if text_marked {
                store.mark_needs_full_estimate();
            }
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
/// A `Gone` fetch drops the reference from the pinned turn (so it does
/// not ride back into pins.json) and marks its pointer line "(image
/// unavailable at restore)" in the in-memory message only; a transient
/// failure leaves the pin text-only for the next restore to retry.
pub async fn reassemble_pin_attachments(
    store: &mut ContextStore,
    mut fetch: impl FnMut(
        String,
        Option<uuid::Uuid>,
    )
        -> std::pin::Pin<Box<dyn std::future::Future<Output = FetchedImage> + Send>>,
) -> ReassemblyReport {
    let mut report = ReassemblyReport {
        invalidated: Vec::new(),
        skipped: Vec::new(),
    };
    let pin_ids: Vec<u64> = store.pinned_turns().map(|t| t.id.0).collect();
    for turn_id in pin_ids {
        let Some(turn) = store.turns_mut().iter_mut().find(|t| t.id.0 == turn_id) else {
            continue;
        };
        if turn.messages.len() != 1 {
            continue;
        };
        let live: Vec<PinAttachment> = turn.pinned_attachments().to_vec();
        if live.is_empty() {
            continue;
        };
        let mut text = turn.messages[0].content().map(str::to_owned);
        let mut images = Vec::new();
        let mut gone = Vec::new();
        let mut text_marked = false;
        for r in &live {
            match fetch_with_retry(r.blob_id.clone(), r.record_id, &mut fetch).await {
                FetchedImage::Bytes(bytes) => {
                    images.push(IngestImage {
                        media_type: r.media_type.clone(),
                        bytes,
                    });
                }
                FetchedImage::Gone => {
                    gone.push(r.blob_id.clone());
                    report.invalidated.push((
                        turn_id,
                        r.blob_id.clone(),
                        "local blob copy is missing".to_owned(),
                    ));
                    // Mark the pointer line so the member sees why the image
                    // is absent. The marked text form rides persist into
                    // pins.json -- an honest label on a permanently abandoned
                    // reference; the dropped anchor never re-triggers it, so
                    // it lands exactly once. Deduped against the live text.
                    if let Some(t) = text.as_mut() {
                        let pointer = pointer_line(r.record_id);
                        let marked = format!("{pointer} (image unavailable at restore)");
                        if t.contains(&pointer) && !t.contains(&marked) {
                            *t = t.replace(&pointer, &marked);
                            text_marked = true;
                        }
                    }
                }
                FetchedImage::Transient(reason) => {
                    report.skipped.push((turn_id, r.blob_id.clone(), reason));
                }
            }
        }
        if !gone.is_empty() {
            // A deterministically-gone reference must not ride back into
            // pins.json on the next projection: drop it from the turn.
            if let Some(attachments) = turn.pinned_attachments_mut() {
                attachments.retain(|a| !gone.contains(&a.blob_id));
            }
        }
        if text_marked && let Some(t) = text.as_ref() {
            // The marked text form rides persist into pins.json (see the
            // Gone arm); it lands once and never re-triggers.
            turn.messages[0] = Message::user(t.clone());
        }
        if images.is_empty() {
            if text_marked {
                store.mark_needs_full_estimate();
            }
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
/// The modalities the store's assembled content requires: window turns
/// and pinned turns are examined as actually assembled — a message
/// counts toward [`Modality::Image`] only when it carries a real image
/// part, so a degraded text form (a pointer line left behind by a
/// failed fetch) makes no demand. Pure in-memory, O(window + pins): no
/// history scan — the wake gate reads what the runtime would send.
pub(crate) fn required_modalities(store: &ContextStore) -> BTreeSet<Modality> {
    let mut required = BTreeSet::new();
    let has_images = store
        .turns()
        .iter()
        .chain(store.pinned_turns())
        .any(|t| t.messages.iter().any(message_has_images));
    if has_images {
        required.insert(Modality::Image);
    }
    required
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
    fn seed_history_with_image(dir: &std::path::Path, record_id: uuid::Uuid, blob_id: &str) {
        HistoryWriter::new(dir.to_owned())
            .append(
                Some(0),
                &[Message::user("[image {id}] caption\n[image x]")],
                8,
                crate::history::RecordKind::Turn,
                None,
                &[crate::history::AttachmentRef {
                    modality: kallipai_common::protocol::Modality::Image,
                    record_id: Some(record_id),
                    media_type: "image/png".to_owned(),
                    blob_id: blob_id.to_owned(),
                    caption: Some("caption".to_owned()),
                }],
            )
            .unwrap();
    }
    /// The seeded history's window sidecar, hydrated the way restore does.
    fn sidecar_of(
        dir: &std::path::Path,
        turn_ids: &[u64],
    ) -> BTreeMap<u64, Vec<crate::history::AttachmentRef>> {
        crate::history::hydrate_turns(dir, turn_ids).1.sidecar
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
        seed_history_with_image(dir.path(), record_id, "sha256-missing-copy");

        let mut store = ContextStore::new();
        store.push_turn(vec![Message::user("[image x] caption")]);
        let report =
            reassemble_attachments(&mut store, &sidecar_of(dir.path(), &[0]), |_blob, id| {
                Box::pin(async move {
                    assert_eq!(id, Some(record_id));
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
    async fn gone_reference_is_reported_and_stays_text_only() {
        let dir = tempfile::tempdir().unwrap();
        let record_id = uuid::Uuid::from_u128(0xB0B);
        seed_history_with_image(dir.path(), record_id, "sha256-missing-copy");

        let mut store = ContextStore::new();
        store.push_turn(vec![Message::user(format!("[image {record_id}] caption"))]);

        let report =
            reassemble_attachments(&mut store, &sidecar_of(dir.path(), &[0]), |_, _blob| {
                Box::pin(async move { FetchedImage::Gone })
            })
            .await;
        assert_eq!(report.invalidated.len(), 1);
        assert_eq!(report.invalidated[0].0, 0);
        assert_eq!(report.invalidated[0].1, "sha256-missing-copy");
        assert!(text_of(&store.turns()[0].messages).contains(&format!("[image {record_id}]")));
        assert!(
            text_of(&store.turns()[0].messages).contains("(image unavailable at restore)"),
            "a gone image marks its pointer line in memory"
        );
        // The marker is an in-memory presentation only: the history file
        // on disk keeps the plain pointer line.
        let file = std::fs::read_dir(dir.path().join("history"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let line = std::fs::read_to_string(file).unwrap();
        assert!(!line.contains("(image unavailable at restore)"));
    }

    /// The serde default on blob_id keeps a legacy line written before
    /// anchors were mandatory alive, and its empty anchor reaches the
    /// fetcher verbatim (where the parse failure classifies it as Gone, never Transient).
    #[tokio::test]
    async fn a_blob_id_less_legacy_line_survives_and_reaches_the_fetcher() {
        let dir = tempfile::tempdir().unwrap();
        let record_id = uuid::Uuid::from_u128(0xDADA);
        seed_history_with_image(dir.path(), record_id, "");
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let mut store = ContextStore::new();
        store.push_turn(vec![Message::user(format!("[image {record_id}] caption"))]);
        let report = reassemble_attachments(
            &mut store,
            &sidecar_of(dir.path(), &[0]),
            move |blob, _id| {
                let sink = sink.clone();
                Box::pin(async move {
                    sink.lock().unwrap().push(blob);
                    FetchedImage::Gone
                })
            },
        )
        .await;
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["".to_owned()],
            "the default anchor reaches the fetcher verbatim"
        );
        assert_eq!(report.invalidated.len(), 1);
    }

    #[tokio::test]
    async fn transient_failures_stay_text_only_without_invalidation() {
        let dir = tempfile::tempdir().unwrap();
        let record_id = uuid::Uuid::from_u128(0xC0FFEE);
        seed_history_with_image(dir.path(), record_id, "sha256-missing-copy");

        let mut store = ContextStore::new();
        store.push_turn(vec![Message::user("[image x] caption")]);

        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = fetches.clone();
        let report = reassemble_attachments(
            &mut store,
            &sidecar_of(dir.path(), &[0]),
            move |_blob, id| {
                let counter = counter.clone();
                let _ = id;
                Box::pin(async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    FetchedImage::Transient("upstream 503".to_owned())
                })
            },
        )
        .await;
        assert!(report.invalidated.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].1, "sha256-missing-copy");
        assert!(text_of(&store.turns()[0].messages).contains("[image x]"));
        assert_eq!(
            fetches.load(std::sync::atomic::Ordering::SeqCst),
            REFETCH_ATTEMPTS,
            "each transient ref is attempted exactly the retry budget"
        );

        // Nothing was invalidated: the next pass tries again.
        let report =
            reassemble_attachments(&mut store, &sidecar_of(dir.path(), &[0]), |_, _blob| {
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
        assert!(extract_pin_attachments(&msg, None).is_empty());
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
        let dir = tempfile::tempdir().unwrap();
        let backend = kallipai_blob_store::LocalBackend::arc(dir.path().to_owned());
        let refs = extract_pin_attachments(&msg, Some(&backend));
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].record_id, Some(record_id));
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
        assert!(extract_pin_attachments(&msg, None).is_empty());
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
        let record_id = uuid::Uuid::from_u128(0xB0B);
        let mut store = ContextStore::new();
        let dir = tempfile::tempdir().unwrap();
        let backend = kallipai_blob_store::LocalBackend::arc(dir.path().to_owned());
        store.set_attachment_blobs(Some(backend));
        store.pin("shot", parts_pin(record_id)).unwrap();
        let report = reassemble_pin_attachments(&mut store, |_blob, id| {
            Box::pin(async move {
                assert_eq!(id, Some(record_id));
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
        let record_id = uuid::Uuid::from_u128(0xB0B);
        let mut store = ContextStore::new();
        let dir = tempfile::tempdir().unwrap();
        let backend = kallipai_blob_store::LocalBackend::arc(dir.path().to_owned());
        store.set_attachment_blobs(Some(backend));
        store.pin("shot", parts_pin(record_id)).unwrap();
        let report = reassemble_pin_attachments(&mut store, |_, _blob| {
            Box::pin(async move { FetchedImage::Gone })
        })
        .await;
        assert_eq!(report.invalidated.len(), 1);
        assert!(report.invalidated[0].1.starts_with("sha256-"));
        assert!(
            store
                .pinned_turns()
                .next()
                .unwrap()
                .pinned_attachments()
                .is_empty()
        );
    }

    /// The pin-side marker rides persist into pins.json exactly once: a
    /// cold-restored (text-form) pin whose blob is gone gains one marker
    /// per gone pointer line, and projection-reload round trips never
    /// add another (the dropped anchor has no trigger left).
    #[tokio::test]
    async fn pin_gone_marker_lands_in_pins_json_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let backend = kallipai_blob_store::LocalBackend::arc(dir.path().to_owned());
        let mut store = ContextStore::new();
        store.set_attachment_blobs(Some(backend));
        store
            .pin("shot", parts_pin(uuid::Uuid::from_u128(0xB0B)))
            .unwrap();

        // Cold restore: the pin reloads from its persisted text form.
        let pins = store.to_pins_doc();
        let manifest = store.to_manifest_doc();
        let mut store = ContextStore::from_persisted(&pins, Vec::new(), &manifest);
        reassemble_pin_attachments(&mut store, |_, _| {
            Box::pin(async move { FetchedImage::Gone })
        })
        .await;
        let text = store.to_pins_doc().pins[0]
            .message
            .content()
            .unwrap()
            .to_owned();
        assert_eq!(text.matches("(image unavailable at restore)").count(), 1);

        // A projection-reload round trip re-runs the pass with no
        // references left; the marker count must not grow.
        let pins = store.to_pins_doc();
        let manifest = store.to_manifest_doc();
        let mut store = ContextStore::from_persisted(&pins, Vec::new(), &manifest);
        let report = reassemble_pin_attachments(&mut store, |_, _| {
            Box::pin(async move { FetchedImage::Gone })
        })
        .await;
        assert!(report.invalidated.is_empty());
        let text = store.to_pins_doc().pins[0]
            .message
            .content()
            .unwrap()
            .to_owned();
        assert_eq!(text.matches("(image unavailable at restore)").count(), 1);
    }

    /// Two gone images mark their own pointer lines once each: the
    /// dedup guard keeps one line's marker from stacking.
    #[tokio::test]
    async fn two_gone_pins_mark_each_line_once() {
        let dir = tempfile::tempdir().unwrap();
        let backend = kallipai_blob_store::LocalBackend::arc(dir.path().to_owned());
        let mut store = ContextStore::new();
        store.set_attachment_blobs(Some(backend));
        let a = uuid::Uuid::from_u128(0xA);
        let b = uuid::Uuid::from_u128(0xB);
        let msg = ingest_message(
            &format!("cap\n[image {a}]\n[image {b}]"),
            &[
                IngestImage {
                    media_type: "image/png".to_owned(),
                    bytes: png_header(8, 8),
                },
                IngestImage {
                    media_type: "image/png".to_owned(),
                    bytes: png_header(9, 9),
                },
            ],
        );
        store.pin("two", msg).unwrap();

        // Cold restore: the pin reloads from its persisted text form.
        let pins = store.to_pins_doc();
        let manifest = store.to_manifest_doc();
        let mut store = ContextStore::from_persisted(&pins, Vec::new(), &manifest);
        let report = reassemble_pin_attachments(&mut store, |_, _| {
            Box::pin(async move { FetchedImage::Gone })
        })
        .await;
        assert_eq!(report.invalidated.len(), 2);
        let text = store.to_pins_doc().pins[0]
            .message
            .content()
            .unwrap()
            .to_owned();
        assert_eq!(text.matches("(image unavailable at restore)").count(), 2);
    }

    #[tokio::test]
    async fn pin_transient_failures_stay_textual_within_the_retry_budget() {
        let record_id = uuid::Uuid::from_u128(0xB0B);
        let mut store = ContextStore::new();
        let dir = tempfile::tempdir().unwrap();
        let backend = kallipai_blob_store::LocalBackend::arc(dir.path().to_owned());
        store.set_attachment_blobs(Some(backend));
        store.pin("shot", parts_pin(record_id)).unwrap();
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = fetches.clone();
        let report = reassemble_pin_attachments(&mut store, move |_blob, id| {
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
    async fn blob_anchor_rides_the_fetch_call() {
        let dir = tempfile::tempdir().unwrap();
        let record_id = uuid::Uuid::from_u128(0xD00D);
        let anchor = kallipai_blob_store::BlobId::for_bytes(&png_header(8, 8))
            .as_str()
            .to_owned();
        seed_history_with_image(dir.path(), record_id, &anchor);

        let mut store = ContextStore::new();
        store.push_turn(vec![Message::user("[image x] caption")]);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let report = reassemble_attachments(
            &mut store,
            &sidecar_of(dir.path(), &[0]),
            move |blob, id| {
                let sink = sink.clone();
                Box::pin(async move {
                    assert_eq!(id, Some(record_id));
                    sink.lock().unwrap().push(blob);
                    FetchedImage::Bytes(png_header(64, 48))
                })
            },
        )
        .await;
        assert!(report.invalidated.is_empty());
        assert!(report.skipped.is_empty());
        assert_eq!(
            *seen.lock().unwrap(),
            vec![anchor.clone()],
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
        let dir = tempfile::tempdir().unwrap();
        let backend = kallipai_blob_store::LocalBackend::arc(dir.path().to_owned());
        let refs = extract_pin_attachments(&msg, Some(&backend));
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].blob_id,
            kallipai_blob_store::BlobId::for_bytes(&bytes).as_str(),
            "the anchor is the content address of the decoded bytes"
        );
    }

    /// The wake gate's requirement reads the assembled content only: real
    /// image parts demand Image, degraded pointer lines and plain text do
    /// not, and pinned turns count as much as window turns.
    #[test]
    fn required_modalities_read_the_assembled_content() {
        // Plain window and no pins: no demand at all.
        let mut store = ContextStore::new();
        store.push_turn(vec![Message::user("plain text")]);
        assert!(required_modalities(&store).is_empty());

        // A window turn assembled with real image parts demands Image.
        store.push_turn(vec![ingest_message(
            "chart",
            &[IngestImage {
                media_type: "image/png".to_owned(),
                bytes: png_header(8, 8),
            }],
        )]);
        assert_eq!(
            required_modalities(&store),
            BTreeSet::from([Modality::Image])
        );

        // A degraded pointer line (text form, no image parts) makes no
        // demand: the wake gate must not block on content nobody holds.
        let mut degraded = ContextStore::new();
        degraded.push_turn(vec![Message::user("[image x] caption")]);
        assert!(required_modalities(&degraded).is_empty());

        // A pinned turn with image parts demands Image even with a plain
        // conversation window.
        let mut pinned = ContextStore::new();
        pinned.push_turn(vec![Message::user("plain")]);
        pinned
            .pin("shot", parts_pin(uuid::Uuid::from_u128(0xF00)))
            .unwrap();
        assert_eq!(
            required_modalities(&pinned),
            BTreeSet::from([Modality::Image])
        );
    }
}
