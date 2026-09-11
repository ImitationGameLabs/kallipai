//! Crate-private token-estimation seam.
//!
//! All token counting for context management routes through here. Backed by `tokenx-rs` — a
//! single-pass character scanner, zero dependencies, no vocabulary files. Accuracy is ~96% on
//! English prose (tokenx's benchmark vs `cl100k_base`), CJK-aware at ~1 token/char, and less
//! precise on JSON/structural text — it is an *estimate* for budget gates and compaction
//! triggers, not an exact count. This replaces the prior `chars/4` heuristic, which
//! underestimated CJK text ~4× (a Chinese character is ~1 real token, not 0.25) and so caused
//! budget/compaction gates to fire far too late for CJK-heavy agents.
//!
//! Image parts are estimated by vision-token approximation, never by their serialized
//! bytes — see [`estimate_image_vision_tokens`].

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use just_llm_client::types::generation::{ContentPart, ImageSource, Message, MessageContent};

/// Estimate tokens for rendered text via `tokenx`.
pub(crate) fn estimate_text(text: &str) -> usize {
    tokenx_rs::estimate_token_count(text)
}

/// The stand-in text for an image part inside an estimation view: the JSON
/// envelope stays counted, the bytes do not (they are not tokens — see
/// [`estimate_image_vision_tokens`]).
const IMAGE_ESTIMATE_MARKER: &str = "[image]";

/// The vision-token estimate for an image whose dimensions cannot be read
/// (URL/FileId sources, or a header this crate does not parse): on the
/// order of a thousand tokens — an order of magnitude above a low-detail
/// tile, well below a full-resolution photo, so under- and overcounting
/// both stay bounded. Never zero: the budget gate must keep seeing image
/// turns as costly.
const IMAGE_TOKEN_FALLBACK: usize = 1024;

/// Providers bill image inputs as vision tokens roughly proportional to
/// the pixel count (Anthropic documents ~`pixels / 750`); the bytes
/// themselves are not tokenized as text. Estimating the JSON-serialized
/// Base64 instead overcounted large images by orders of magnitude, which
/// made image turns look over budget and wedged compaction into slicing
/// them — silently dropping the image while keeping the text.
///
/// Local approximation: `pixels / 750` from header-decoded dimensions,
/// falling back to [`IMAGE_TOKEN_FALLBACK`] when the source carries no
/// locally readable bytes.
///
/// Dual-track maintenance: when the upstream
/// `render_messages`-based estimator becomes image-aware, this rule is
/// retired or aligned with it — the two tracks must not drift.
pub(crate) fn estimate_image_vision_tokens(source: &ImageSource) -> usize {
    let header = match source {
        ImageSource::Base64 { data, .. } => decode_header_bytes(data),
        // Url and FileId sources carry no locally readable bytes.
        _ => None,
    };
    let Some((width, height)) = header.as_deref().and_then(image_dimensions) else {
        return IMAGE_TOKEN_FALLBACK;
    };
    (width as usize * height as usize).div_ceil(750)
}

/// Decode the leading Base64 bytes of an inline image — enough for every
/// header format [`image_dimensions`] understands. Canonical `=` padding
/// terminates standard decoding, so the slice is cut at the end of the
/// group holding the first `=` (a whole-file header string simply decodes
/// whole); a truncated tail is fine either way — headers live in the
/// first few dozen bytes.
fn decode_header_bytes(data: &str) -> Option<Vec<u8>> {
    let head = &data[..data.len().min(88)];
    let cut = match head.find('=') {
        Some(i) => ((i / 4) + 1) * 4,
        None => head.len() - head.len() % 4,
    };
    STANDARD.decode(&head[..cut.min(head.len())]).ok()
}

/// Dimensions parsed from an image's leading bytes: `(width, height)`.
/// Header-only — PNG `IHDR`, JPEG SOF markers, WebP `VP8 `/`VP8L`/`VP8X`.
/// `None` for anything else; callers fall back rather than guess.
pub(crate) fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() >= 24 && bytes[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        // Fixed layout: 8-byte signature, 4-byte chunk length, the `IHDR`
        // type, then width and height as big-endian u32.
        let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return Some((width, height));
    }
    if bytes.len() >= 4 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
        return jpeg_dimensions(bytes);
    }
    if bytes.len() >= 30 && bytes[0..4] == *b"RIFF" && bytes[8..12] == *b"WEBP" {
        return webp_dimensions(bytes);
    }
    None
}

/// Walk the JPEG marker segments to the first SOF (start-of-frame)
/// marker, which carries the dimensions: `0xC0..=0xCF` minus the two
/// non-SOF markers in that range (`0xC4` DHT, `0xCC` DAC).
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2usize;
    while i + 9 <= bytes.len() {
        if bytes[i] != 0xFF {
            return None;
        }
        let marker = bytes[i + 1];
        let is_sof = (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xCC;
        if is_sof {
            let height = u16::from_be_bytes(bytes[i + 5..i + 7].try_into().ok()?) as u32;
            let width = u16::from_be_bytes(bytes[i + 7..i + 9].try_into().ok()?) as u32;
            return Some((width, height));
        }
        // Every other marker carries a big-endian segment length here.
        let length = u16::from_be_bytes(bytes[i + 2..i + 4].try_into().ok()?) as usize;
        if length < 2 {
            return None;
        }
        i += 2 + length;
    }
    None
}

/// WebP dimensions across the three container flavors: the simple lossy
/// `VP8 ` chunk, the lossless `VP8L` bit stream, and the extended `VP8X`
/// canvas header. All store little-endian 14-bit (or 24-bit minus one)
/// values.
fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let flavor = &bytes[12..16];
    if flavor == b"VP8 " {
        // Payload: 3-byte frame tag, the 0x9D 0x01 0x2A start code,
        // then width/height as LE u16 with the top 2 bits reserved.
        let width = u16::from_le_bytes(bytes[26..28].try_into().ok()?) & 0x3FFF;
        let height = u16::from_le_bytes(bytes[28..30].try_into().ok()?) & 0x3FFF;
        Some((u32::from(width), u32::from(height)))
    } else if flavor == b"VP8L" {
        // 14 bits of width-1 then 14 bits of height-1, packed into a
        // little-endian u32 right after a 1-byte signature byte.
        if bytes[20] != 0x2F {
            return None;
        }
        let bits = u32::from_le_bytes(bytes[21..25].try_into().ok()?);
        let width = (bits & 0x3FFF) + 1;
        let height = ((bits >> 14) & 0x3FFF) + 1;
        Some((width, height))
    } else if flavor == b"VP8X" {
        // 24-bit minus-one canvas size after 4 flag/reserved bytes.
        let width =
            u32::from(bytes[24]) | (u32::from(bytes[25]) << 8) | (u32::from(bytes[26]) << 16);
        let height =
            u32::from(bytes[27]) | (u32::from(bytes[28]) << 8) | (u32::from(bytes[29]) << 16);
        Some((width + 1, height + 1))
    } else {
        None
    }
}

/// Estimate tokens for one message by rendering its wire-format JSON — the role tag, content
/// key, and tool-call structure are all in the JSON, so the structural overhead is counted
/// directly and no per-message / per-tool-call magic constants are needed. Image parts are
/// swapped for a tiny marker and estimated as vision tokens (see
/// [`estimate_image_vision_tokens`]), never counted as Base64 text.
///
/// `Message` derives `Serialize`; its JSON matches `render_messages` closely for the
/// OpenAI-compatible providers, so this cached per-message estimate stays consistent with the
/// live full-render estimate in `estimate.rs`. Deliberately separate from that path so the
/// cache needs no `GenerationClient`.
pub(crate) fn estimate_message_tokens(message: &Message) -> usize {
    let (view, image_tokens) = estimation_view(message);
    match serde_json::to_string(&view) {
        Ok(json) => estimate_text(&json) + image_tokens,
        // Unreachable for a derive(Serialize) enum of String/Vec/Option fields; fall back to
        // content-only if serialization ever fails so estimation never panics.
        Err(_) => estimate_text(view.content().unwrap_or_default()) + image_tokens,
    }
}

/// The estimation view of one message: image parts swapped for the tiny
/// [`IMAGE_ESTIMATE_MARKER`] (the JSON envelope stays counted), plus the
/// summed [`estimate_image_vision_tokens`] of the swapped images.
/// Messages without image parts clone through unchanged.
fn estimation_view(message: &Message) -> (Message, usize) {
    let parts: &[ContentPart] = match message {
        Message::User {
            content: MessageContent::Parts(parts),
        }
        | Message::System {
            content: MessageContent::Parts(parts),
        } => parts,
        // Only User/System messages can carry parts; everything else has
        // no image surface.
        _ => return (message.clone(), 0),
    };
    if !parts.iter().any(|p| matches!(p, ContentPart::Image { .. })) {
        return (message.clone(), 0);
    }
    let mut image_tokens = 0usize;
    let swapped = parts
        .iter()
        .map(|part| match part {
            ContentPart::Image { source, .. } => {
                image_tokens += estimate_image_vision_tokens(source);
                ContentPart::Text {
                    text: IMAGE_ESTIMATE_MARKER.to_owned(),
                }
            }
            other => other.clone(),
        })
        .collect();
    let view = match message {
        Message::User { .. } => Message::User {
            content: MessageContent::Parts(swapped),
        },
        _ => Message::System {
            content: MessageContent::Parts(swapped),
        },
    };
    (view, image_tokens)
}

/// The estimation views of a whole message list (see [`estimation_view`]):
/// the swap applied throughout, plus the summed vision-token
/// approximation of every swapped image. The full and incremental
/// estimators render these views so the Base64 blob never reaches the
/// text estimate.
pub(crate) fn estimation_views(messages: &[Message]) -> (Vec<Message>, usize) {
    let mut views = Vec::with_capacity(messages.len());
    let mut image_tokens = 0usize;
    for message in messages {
        let (view, tokens) = estimation_view(message);
        image_tokens += tokens;
        views.push(view);
    }
    (views, image_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_text_is_positive_for_nonempty() {
        assert!(estimate_text("hello world") > 0);
    }

    #[test]
    fn cjk_estimates_near_one_token_per_char() {
        // tokenx scores CJK at 1 token/char (vs char/4's 0.25) — the core reason for the swap.
        let cjk = estimate_text("你好世界测试"); // 6 CJK chars
        assert!(cjk >= 6, "CJK should estimate ~1 token/char: got {cjk}");
        // And CJK estimates higher than equal-length Latin, which packs into fewer word-tokens.
        let latin = estimate_text("abcdef"); // 6 latin chars → ~1 word-token
        assert!(
            cjk > latin,
            "CJK ({cjk}) should exceed same-length Latin ({latin})"
        );
    }

    #[test]
    fn estimate_message_tokens_includes_envelope() {
        // A bare content string estimates fewer tokens than the same content wrapped in a
        // user message (the JSON envelope adds the role tag and content key).
        let bare = estimate_text("hello");
        let wrapped = estimate_message_tokens(&Message::user("hello"));
        assert!(
            wrapped > bare,
            "envelope must add tokens: {wrapped} vs {bare}"
        );
    }

    // -- image-aware estimation -------------------------------------------

    /// Minimal hand-built headers: only the bytes the dimension parsers
    /// read, Base64-wrapped. No binary fixtures.
    fn png(width: u32, height: u32) -> String {
        let mut b = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&height.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0]);
        STANDARD.encode(b)
    }

    fn jpeg(width: u16, height: u16) -> String {
        let mut b = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
        b.extend_from_slice(&height.to_be_bytes());
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&[3, 0, 22, 0, 0, 0xFF, 0xD9]);
        STANDARD.encode(b)
    }

    fn webp_lossy(width: u16, height: u16) -> String {
        let mut b = b"RIFF".to_vec();
        b.extend_from_slice(&[0, 0, 0, 0]);
        b.extend_from_slice(b"WEBPVP8 ");
        b.extend_from_slice(&[0, 0, 0, 0, 0x30, 0x01, 0x00, 0x9D, 0x01, 0x2A]);
        b.extend_from_slice(&width.to_le_bytes());
        b.extend_from_slice(&height.to_le_bytes());
        STANDARD.encode(b)
    }

    fn webp_extended(width: u32, height: u32) -> String {
        let mut b = b"RIFF".to_vec();
        b.extend_from_slice(&[0, 0, 0, 0]);
        b.extend_from_slice(b"WEBPVP8X");
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        for value in [width - 1, height - 1] {
            b.extend_from_slice(&[value as u8, (value >> 8) as u8, (value >> 16) as u8]);
        }
        STANDARD.encode(b)
    }

    fn image_message(data: &str) -> Message {
        Message::user_parts(vec![
            ContentPart::Text {
                text: "a chart".to_owned(),
            },
            ContentPart::Image {
                source: ImageSource::Base64 {
                    data: data.to_owned(),
                    media_type: "image/png".to_owned(),
                },
                detail: None,
            },
        ])
    }

    #[test]
    fn image_dimensions_parse_each_header_format() {
        assert_eq!(
            image_dimensions(&STANDARD.decode(png(64, 48)).unwrap()),
            Some((64, 48))
        );
        assert_eq!(
            image_dimensions(&STANDARD.decode(jpeg(160, 90)).unwrap()),
            Some((160, 90))
        );
        assert_eq!(
            image_dimensions(&STANDARD.decode(webp_lossy(300, 200)).unwrap()),
            Some((300, 200))
        );
        assert_eq!(
            image_dimensions(&STANDARD.decode(webp_extended(1024, 768)).unwrap()),
            Some((1024, 768))
        );
        assert_eq!(image_dimensions(b"not an image at all"), None);
        assert_eq!(image_dimensions(&[]), None);
    }

    #[test]
    fn vision_tokens_follow_pixels_per_the_documented_rule() {
        let source = ImageSource::Base64 {
            data: png(750, 750),
            media_type: "image/png".to_owned(),
        };
        assert_eq!(estimate_image_vision_tokens(&source), 750);
        let big = ImageSource::Base64 {
            data: png(1500, 1000),
            media_type: "image/png".to_owned(),
        };
        assert_eq!(estimate_image_vision_tokens(&big), 2000);
    }

    #[test]
    fn unreadable_headers_fall_back_never_to_zero() {
        let source = ImageSource::Base64 {
            data: STANDARD.encode(b"garbage bytes that are no image header"),
            media_type: "image/png".to_owned(),
        };
        assert_eq!(estimate_image_vision_tokens(&source), IMAGE_TOKEN_FALLBACK);
        let url = ImageSource::Url {
            url: "https://example.invalid/a.png".to_owned(),
        };
        assert_eq!(estimate_image_vision_tokens(&url), IMAGE_TOKEN_FALLBACK);
    }

    #[test]
    fn estimation_ignores_base64_size_and_tracks_dimensions() {
        // Same 64x64 image, wildly different Base64 lengths: the estimate
        // must not move — the bytes are not tokens.
        let small = image_message(&png(64, 64));
        let mut padded = png(64, 64);
        padded.push_str(&STANDARD.encode(vec![0u8; 200_000]));
        let large = image_message(&padded);
        let small_estimate = estimate_message_tokens(&small);
        let large_estimate = estimate_message_tokens(&large);
        assert_eq!(
            small_estimate, large_estimate,
            "Base64 length must not change the estimate"
        );
        // A larger image does cost more.
        let bigger = image_message(&png(640, 640));
        assert!(estimate_message_tokens(&bigger) > small_estimate);
    }

    #[test]
    fn estimation_views_swap_images_for_the_marker() {
        let messages = vec![image_message(&png(64, 64))];
        let (views, image_tokens) = estimation_views(&messages);
        assert!(image_tokens > 0);
        let Message::User {
            content: MessageContent::Parts(parts),
        } = &views[0]
        else {
            panic!("expected a user parts message");
        };
        assert!(
            !parts.iter().any(|p| matches!(p, ContentPart::Image { .. })),
            "the view must not carry the image bytes"
        );
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, ContentPart::Text { text } if text == IMAGE_ESTIMATE_MARKER)),
            "the view must carry the marker"
        );
    }

    #[test]
    fn decode_header_bytes_cuts_at_the_padded_group() {
        // A real 1x1 PNG header Base64-wraps with `=` padding (29 bytes →
        // 40 chars ending in one `=`): the cut branch is the common case for
        // small images, and the header still parses.
        let data = png(1, 1);
        assert!(data.contains('='));
        let bytes = decode_header_bytes(&data).unwrap();
        assert_eq!(image_dimensions(&bytes), Some((1, 1)));
    }

    #[test]
    fn decode_header_bytes_clamps_when_padding_sits_before_the_tail() {
        // `=` in the final incomplete group: the cut would overrun the
        // slice, so it clamps to the slice end; the misaligned tail then
        // fails the decode cleanly instead of panicking.
        let data = format!("{}={}", "A".repeat(40), "B");
        assert_eq!(data.len(), 42);
        assert_eq!(decode_header_bytes(&data), None);
    }

    #[test]
    fn plain_messages_clone_through_the_view_unchanged() {
        let messages = vec![Message::user("just text")];
        let (views, image_tokens) = estimation_views(&messages);
        assert_eq!(image_tokens, 0);
        assert_eq!(views[0], messages[0]);
    }
}
