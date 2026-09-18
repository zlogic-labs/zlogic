use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use zlogic_protocol::config::ModelCapabilities;
use zlogic_protocol::message::{ImagePart, ImageSource};

const IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

const MAX_ENCODED_BYTES: usize = 28 * 1024 * 1024;
const MAX_DECODED_BYTES: usize = 20 * 1024 * 1024;

pub enum Resolved {
    Image { mime: String, base64: String },
    Placeholder(String),
}

pub fn resolve(img: &ImagePart, caps: &ModelCapabilities, warnings: &mut Vec<String>) -> Resolved {
    if caps.vision == Some(false) {
        warnings.push("image dropped: the target model declares no vision capability".into());
        return Resolved::Placeholder(
            "[image omitted: this model cannot read images — tell the user to switch models]"
                .into(),
        );
    }

    let data = match &img.source {
        ImageSource::Base64 { data } => data,
        ImageSource::Path { path } => {
            warnings.push(format!("unresolved image path reached the wire: {path}"));
            return Resolved::Placeholder(format!("[image omitted: {path}]"));
        }
    };

    let mime = img.mime_type.trim().to_ascii_lowercase();
    if !IMAGE_MIMES.contains(&mime.as_str()) {
        warnings.push(format!("image dropped: unsupported media type {mime}"));
        return Resolved::Placeholder(format!(
            "[image omitted: {mime} is not a supported image format]"
        ));
    }

    let payload = match data.strip_prefix("data:") {
        None => data.as_str(),
        Some(rest) => {
            let Some((meta, b64)) = rest.split_once(",") else {
                warnings.push("image dropped: malformed data URL".into());
                return Resolved::Placeholder("[image omitted: malformed image data]".into());
            };
            let declared = meta.split(';').next().unwrap_or("").to_ascii_lowercase();
            if !declared.is_empty() && declared != mime {
                warnings.push(format!(
                    "image dropped: data URL type {declared} contradicts media type {mime}"
                ));
                return Resolved::Placeholder(
                    "[image omitted: inconsistent image metadata]".into(),
                );
            }
            b64
        }
    };

    if payload.len() > MAX_ENCODED_BYTES {
        warnings.push(format!(
            "image dropped: {} encoded bytes exceeds the limit",
            payload.len()
        ));
        return Resolved::Placeholder("[image omitted: file is too large]".into());
    }

    let Ok(bytes) = STANDARD.decode(payload) else {
        warnings.push("image dropped: payload is not valid base64".into());
        return Resolved::Placeholder("[image omitted: image data is corrupted]".into());
    };
    if bytes.is_empty() {
        warnings.push("image dropped: payload decodes to zero bytes".into());
        return Resolved::Placeholder("[image omitted: image file is empty]".into());
    }
    if bytes.len() > MAX_DECODED_BYTES {
        warnings.push(format!(
            "image dropped: {} decoded bytes exceeds the limit",
            bytes.len()
        ));
        return Resolved::Placeholder("[image omitted: file is too large]".into());
    }
    let Some(actual_mime) = detected_image_mime(&bytes) else {
        warnings.push("image dropped: decoded bytes have no supported image signature".into());
        return Resolved::Placeholder("[image omitted: image data is corrupted]".into());
    };
    if actual_mime != mime {
        warnings.push(format!(
            "image dropped: payload is {actual_mime}, metadata declares {mime}"
        ));
        return Resolved::Placeholder("[image omitted: inconsistent image metadata]".into());
    }

    Resolved::Image {
        mime,
        base64: STANDARD.encode(&bytes),
    }
}

fn detected_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub fn bedrock_format(mime: &str) -> &str {
    match mime.rsplit('/').next().unwrap_or("png") {
        "jpg" => "jpeg",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlogic_protocol::config::ThinkingCapability;

    fn caps(vision: Option<bool>) -> ModelCapabilities {
        ModelCapabilities {
            vision,
            thinking: ThinkingCapability::default(),
        }
    }

    fn img(mime: &str, data: &str) -> ImagePart {
        ImagePart {
            mime_type: mime.into(),
            source: ImageSource::Base64 { data: data.into() },
        }
    }

    fn png_b64() -> String {
        STANDARD.encode([
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52,
        ])
    }

    fn placeholder(r: Resolved) -> String {
        match r {
            Resolved::Placeholder(t) => t,
            Resolved::Image { .. } => panic!("expected a downgrade to placeholder text"),
        }
    }

    #[test]
    fn a_valid_image_passes_through() {
        let mut w = Vec::new();
        let r = resolve(&img("image/png", &png_b64()), &caps(None), &mut w);
        match r {
            Resolved::Image { mime, base64 } => {
                assert_eq!(mime, "image/png");
                assert_eq!(base64, png_b64());
            }
            Resolved::Placeholder(t) => panic!("should not have downgraded: {t}"),
        }
        assert!(w.is_empty());
    }

    #[test]
    fn a_non_vision_model_gets_an_explanatory_placeholder() {
        let mut w = Vec::new();
        let t = placeholder(resolve(
            &img("image/png", &png_b64()),
            &caps(Some(false)),
            &mut w,
        ));
        assert!(t.contains("cannot read images"), "{t}");
        assert!(
            t.contains("tell the user"),
            "must ask the model to explain: {t}"
        );
        assert!(!w.is_empty());
    }

    #[test]
    fn unknown_vision_capability_is_treated_as_capable() {
        let mut w = Vec::new();
        assert!(matches!(
            resolve(&img("image/png", &png_b64()), &caps(None), &mut w),
            Resolved::Image { .. }
        ));
    }

    #[test]
    fn unresolved_paths_degrade_and_warn() {
        let mut w = Vec::new();
        let part = ImagePart {
            mime_type: "image/png".into(),
            source: ImageSource::Path {
                path: "/tmp/a.png".into(),
            },
        };
        let t = placeholder(resolve(&part, &caps(None), &mut w));
        assert!(t.contains("/tmp/a.png"));
        assert!(!w.is_empty());
    }

    #[test]
    fn non_visual_mimes_are_rejected() {
        for mime in [
            "image/svg+xml",
            "image/heic",
            "application/pdf",
            "text/plain",
        ] {
            let mut w = Vec::new();
            let t = placeholder(resolve(&img(mime, &png_b64()), &caps(None), &mut w));
            assert!(t.contains("not a supported image format"), "{mime}: {t}");
        }
    }

    #[test]
    fn empty_and_corrupt_payloads_are_caught() {
        let mut w = Vec::new();
        assert!(placeholder(resolve(&img("image/png", ""), &caps(None), &mut w)).contains("empty"));

        let mut w = Vec::new();
        let t = placeholder(resolve(
            &img("image/png", "!!!not base64!!!"),
            &caps(None),
            &mut w,
        ));
        assert!(t.contains("corrupted"), "{t}");

        let mut w = Vec::new();
        let t = placeholder(resolve(
            &img("image/png", "iVBORw0KGgoAAAANSUhEU"),
            &caps(None),
            &mut w,
        ));
        assert!(t.contains("corrupted"), "{t}");
    }

    #[test]
    fn a_doubly_wrapped_data_url_is_unwrapped() {
        let mut w = Vec::new();
        let data = format!("data:image/png;base64,{}", png_b64());
        match resolve(&img("image/png", &data), &caps(None), &mut w) {
            Resolved::Image { base64, .. } => assert_eq!(base64, png_b64()),
            Resolved::Placeholder(t) => panic!("should strip the prefix rather than reject: {t}"),
        }
    }

    #[test]
    fn contradictory_metadata_is_rejected() {
        let mut w = Vec::new();
        let data = format!("data:image/jpeg;base64,{}", png_b64());
        let t = placeholder(resolve(&img("image/png", &data), &caps(None), &mut w));
        assert!(t.contains("inconsistent"), "{t}");
    }

    #[test]
    fn oversized_payloads_are_rejected() {
        let mut w = Vec::new();
        let huge = "A".repeat(MAX_ENCODED_BYTES + 4);
        let t = placeholder(resolve(&img("image/png", &huge), &caps(None), &mut w));
        assert!(t.contains("too large"), "{t}");
    }

    #[test]
    fn mime_is_normalized_case_insensitively() {
        let mut w = Vec::new();
        match resolve(&img("IMAGE/PNG", &png_b64()), &caps(None), &mut w) {
            Resolved::Image { mime, .. } => assert_eq!(mime, "image/png"),
            Resolved::Placeholder(t) => panic!("case must not cause a rejection: {t}"),
        }
    }

    #[test]
    fn bedrock_wants_bare_format_names() {
        assert_eq!(bedrock_format("image/png"), "png");
        assert_eq!(bedrock_format("image/jpeg"), "jpeg");
        assert_eq!(
            bedrock_format("image/jpg"),
            "jpeg",
            "jpg is not a valid Converse format"
        );
    }
}
