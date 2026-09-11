//! Attachment-ingest assembly: the multimodal user message built from
//! fetched media bytes plus the sidecar text. Pure and byte-driven — the
//! ingest route fetches the bytes and calls [`ingest_message`], and tests
//! construct bytes in memory.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use just_llm_client::types::generation::{ContentPart, ImageSource, Message};

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
#[cfg(test)]
mod tests {
    use super::*;

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
}
