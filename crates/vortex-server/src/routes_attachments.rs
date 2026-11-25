//! Attachment upload/serving routes.
//!
//! Uploads are validated by magic bytes (authoritative over extension/MIME),
//! bounded per kind, and stored under server-generated UUID paths. Serving
//! sets nosniff + attachment disposition so direct navigation is safe while
//! `<img>`/`<video>` still render. All routes sit inside the guarded router.

use crate::state::{err_json, AppState};
use axum::extract::{DefaultBodyLimit, Multipart, Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use image::{ImageError, ImageFormat, ImageReader, Limits};
use std::io::Cursor;
use std::path::Path as FsPath;
use std::sync::Arc;
use vortex_core::store_attachments::sanitize_filename;
use vortex_types::chat::MAX_ATTACHMENT_UPLOAD_BYTES;
use vortex_types::{AttachmentKind, AttachmentOrigin};

/// Per-kind upload caps (bytes).
pub const IMAGE_MAX: usize = 10 * 1024 * 1024;
pub const PDF_MAX: usize = MAX_ATTACHMENT_UPLOAD_BYTES;
pub const TEXT_MAX: usize = 2 * 1024 * 1024;
// Leave room for base64 expansion under the 32 MiB model attachment budget.
pub const VIDEO_MAX: usize = MAX_ATTACHMENT_UPLOAD_BYTES;
/// Total multipart body cap for the upload route (overrides the 2 MiB default).
pub const ROUTE_BODY_MAX: usize = 25 * 1024 * 1024;
/// Attachments allowed on a single chat message.
pub const MAX_PER_MESSAGE: usize = 8;

const CONVERT_HINT: &str = "Unsupported file type. Upload MP4/M4V, WebM, MOV, or MPEG videos; \
     PNG, JPEG, WebP, GIF, BMP, TIFF, or ICO images; PDF documents; or UTF-8 text/source files. \
     Convert other videos to MP4 (H.264 for wider playback support) and other images \
     (such as HEIC, AVIF, or SVG) to PNG or JPEG. Export DOCX, ODT, or RTF to PDF or text, \
     and extract supported files from archives before uploading.";

fn ext_of(filename: &str) -> String {
    let base = filename.rsplit(['/', '\\']).next().unwrap_or("");
    match base.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

fn is_text_ext(ext: &str) -> bool {
    matches!(
        ext,
        "txt"
            | "md"
            | "markdown"
            | "csv"
            | "tsv"
            | "json"
            | "xml"
            | "yaml"
            | "yml"
            | "toml"
            | "log"
            | "rs"
            | "py"
            | "js"
            | "ts"
            | "jsx"
            | "tsx"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "cc"
            | "go"
            | "java"
            | "rb"
            | "php"
            | "sh"
            | "bash"
            | "sql"
            | "html"
            | "css"
            | "scss"
            | "swift"
            | "kt"
            | "lua"
            | "pl"
            | "r"
            | "m"
            | "hs"
            | "ini"
            | "cfg"
            | "conf"
            | "env"
            | "gitignore"
            | "dockerfile"
            | "makefile"
            | "cmake"
            | "gradle"
            | "properties"
            | "tex"
            | "rst"
            | "adoc"
            | "org"
            | "srt"
            | "vtt"
    )
}

fn is_non_text_ext(ext: &str) -> bool {
    matches!(
        ext,
        "png"
            | "jpg"
            | "jpeg"
            | "jpe"
            | "jfif"
            | "webp"
            | "gif"
            | "pdf"
            | "mp4"
            | "m4v"
            | "mpeg"
            | "mpg"
            | "mpe"
            | "svg"
            | "heic"
            | "heif"
            | "avif"
            | "docx"
            | "doc"
            | "odt"
            | "rtf"
            | "zip"
            | "tar"
            | "gz"
            | "tgz"
            | "bz2"
            | "xz"
            | "7z"
            | "rar"
            | "exe"
            | "dll"
            | "so"
            | "bin"
            | "o"
            | "a"
            | "class"
            | "jar"
            | "war"
            | "wasm"
            | "pyc"
            | "db"
            | "sqlite"
            | "mp3"
            | "wav"
            | "ogg"
            | "flac"
            | "m4a"
            | "aac"
            | "mov"
            | "avi"
            | "mkv"
            | "webm"
            | "bmp"
            | "tiff"
            | "tif"
            | "ico"
    )
}

fn text_media_type(ext: &str) -> &'static str {
    match ext {
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "json" => "application/json",
        "xml" => "application/xml",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        _ => "text/plain",
    }
}

fn bmff_video_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.get(4..8)? != b"ftyp" {
        return None;
    }
    let size = u32::from_be_bytes(bytes[..4].try_into().ok()?) as usize;
    if size < 16 || size > bytes.len() || !size.is_multiple_of(4) {
        return None;
    }
    // Check the major brand, not just the container: AVIF, HEIF, and M4A
    // also start with ftyp, sometimes with generic compatible brands.
    match &bytes[8..12] {
        b"qt  " => Some("video/quicktime"),
        b"isom"
        | [b'i', b's', b'o', b'2'..=b'9']
        | b"mp41"
        | b"mp42"
        | b"avc1"
        | b"av01"
        | b"M4V "
        | b"dash" => Some("video/mp4"),
        _ => None,
    }
}

// Read one EBML length and split off its payload. Unknown lengths are not
// allowed in the EBML header, which is all we inspect.
fn ebml_payload(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let first = *bytes.first()?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 {
        return None;
    }
    let (size_bytes, body) = bytes.split_at_checked(len)?;
    let mut size = u64::from(first ^ (0x80 >> (len - 1)));
    for &b in &size_bytes[1..] {
        size = (size << 8) | u64::from(b);
    }
    if size == (1u64 << (7 * len)) - 1 {
        return None;
    }
    body.split_at_checked(usize::try_from(size).ok()?)
}

fn ebml_doc_type(bytes: &[u8]) -> Option<&[u8]> {
    let (mut header, _) = ebml_payload(bytes.strip_prefix(b"\x1a\x45\xdf\xa3")?)?;
    if header.len() > 1024 {
        return None;
    }
    while let Some(&first) = header.first() {
        let id_len = first.leading_zeros() as usize + 1;
        if id_len > 4 {
            return None;
        }
        let (id, rest) = header.split_at_checked(id_len)?;
        let (data, rest) = ebml_payload(rest)?;
        if id == b"\x42\x82" {
            return Some(data);
        }
        header = rest;
    }
    None
}

/// Classify an upload by magic bytes first, then extension/MIME for text.
/// Returns `(kind, validated media type)` or a human guidance message.
pub fn classify(
    filename: &str,
    declared_mime: &str,
    bytes: &[u8],
) -> Result<(AttachmentKind, &'static str), String> {
    // Magic bytes are authoritative for media and PDFs.
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok((AttachmentKind::Image, "image/png"));
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Ok((AttachmentKind::Image, "image/jpeg"));
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Ok((AttachmentKind::Image, "image/gif"));
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Ok((AttachmentKind::Image, "image/webp"));
    }
    if bytes.len() >= 26 && bytes.starts_with(b"BM") && bytes[6..10] == [0; 4] {
        return Ok((AttachmentKind::Image, "image/bmp"));
    }
    if matches!(
        bytes,
        [b'I', b'I', b'*' | b'+', 0, ..] | [b'M', b'M', 0, b'*' | b'+', ..]
    ) {
        return Ok((AttachmentKind::Image, "image/tiff"));
    }
    if bytes.starts_with(b"\0\0\x01\0") {
        return Ok((AttachmentKind::Image, "image/vnd.microsoft.icon"));
    }
    if bytes.starts_with(b"%PDF-") {
        return Ok((AttachmentKind::Pdf, "application/pdf"));
    }
    if let Some(mime) = bmff_video_type(bytes) {
        return Ok((AttachmentKind::Video, mime));
    }
    // WebM and Matroska share the EBML signature, but have different DocTypes.
    if ebml_doc_type(bytes) == Some(b"webm") {
        return Ok((AttachmentKind::Video, "video/webm"));
    }
    // MPEG program stream pack or MPEG-1/2 video sequence header.
    if bytes.starts_with(b"\0\0\x01\xba") || bytes.starts_with(b"\0\0\x01\xb3") {
        return Ok((AttachmentKind::Video, "video/mpeg"));
    }

    let ext = ext_of(filename);
    if is_non_text_ext(&ext) {
        return Err(CONVERT_HINT.into());
    }
    // Archives are binary regardless of the name they carry.
    if bytes.starts_with(b"PK\x03\x04") {
        return Err(CONVERT_HINT.into());
    }
    // Text must be valid UTF-8 without NUL bytes.
    let text = std::str::from_utf8(bytes).map_err(|_| CONVERT_HINT.to_string())?;
    if bytes.contains(&0) {
        return Err(CONVERT_HINT.into());
    }
    let head: String = text
        .trim_start()
        .chars()
        .take(512)
        .collect::<String>()
        .to_ascii_lowercase();
    if head.contains("<svg") || head.starts_with("{\\rtf") {
        return Err(CONVERT_HINT.into());
    }
    if is_text_ext(&ext) || declared_mime.starts_with("text/") {
        return Ok((AttachmentKind::Text, text_media_type(&ext)));
    }
    Err(CONVERT_HINT.into())
}

fn cap_for(kind: AttachmentKind) -> usize {
    match kind {
        AttachmentKind::Image => IMAGE_MAX,
        AttachmentKind::Pdf => PDF_MAX,
        AttachmentKind::Text => TEXT_MAX,
        AttachmentKind::Video => VIDEO_MAX,
    }
}

fn normalize_image(bytes: &[u8], format: ImageFormat) -> Result<Vec<u8>, ImageError> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()?;
    let mut png = Cursor::new(Vec::new());
    image.write_to(&mut png, ImageFormat::Png)?;
    Ok(png.into_inner())
}

/// POST /api/conversations/{id}/attachments: one multipart `file` field.
pub async fn upload_attachments(
    State(state): State<Arc<AppState>>,
    Path(conv_id): Path<String>,
    mut multipart: Multipart,
) -> Response {
    match state.db.get_conversation(conv_id.clone()).await {
        Ok(Some(_)) => {}
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "conversation not found"),
        Err(_) => {
            crate::diagnostics::db_failure("get_conversation", Some(&conv_id));
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not load conversation",
            );
        }
    }

    let malformed = |status| {
        err_json(
            status,
            "upload failed: body too large or malformed multipart stream",
        )
    };
    let field = match multipart.next_field().await {
        Ok(Some(field)) if field.name() == Some("file") && field.file_name().is_some() => field,
        Ok(_) => return err_json(StatusCode::BAD_REQUEST, "upload one file in a 'file' field"),
        Err(e) => return malformed(e.status()),
    };
    let mut filename = field.file_name().unwrap_or("file").to_owned();
    let declared = field.content_type().unwrap_or("").to_owned();
    let bytes = match field.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => return malformed(e.status()),
    };
    match multipart.next_field().await {
        Ok(None) => {}
        Ok(Some(_)) => return err_json(StatusCode::BAD_REQUEST, "upload one file per request"),
        Err(e) => return malformed(e.status()),
    }
    if bytes.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "empty file");
    }
    let (kind, mut media_type) = match classify(&filename, &declared, &bytes) {
        Ok(v) => v,
        Err(msg) => return err_json(StatusCode::UNSUPPORTED_MEDIA_TYPE, msg),
    };
    if bytes.len() > cap_for(kind) {
        let hint = if kind == AttachmentKind::Video {
            ". Trim or compress the video and try again."
        } else {
            ""
        };
        return err_json(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "'{}' exceeds the {} MiB limit for {} files{hint}",
                sanitize_filename(&filename),
                cap_for(kind) / (1024 * 1024),
                kind.as_str()
            ),
        );
    }
    // OpenRouter accepts PNG but not these raster formats. Normalize once at
    // upload so preview, download, and the model use the same stored bytes.
    let format = match media_type {
        "image/bmp" => Some(ImageFormat::Bmp),
        "image/tiff" => Some(ImageFormat::Tiff),
        "image/vnd.microsoft.icon" => Some(ImageFormat::Ico),
        _ => None,
    };
    let bytes = if let Some(format) = format {
        let result = tokio::task::spawn_blocking(move || normalize_image(&bytes, format)).await;
        let png = match result {
            Ok(Ok(png)) if png.len() <= IMAGE_MAX => png,
            Ok(Ok(_)) | Ok(Err(ImageError::Limits(_))) => {
                return err_json(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Image exceeds the conversion limits (4096 pixels per side, 10 MiB after conversion). Resize it or export a smaller PNG or JPEG.",
                );
            }
            Ok(Err(_)) => {
                return err_json(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "Could not convert this image to PNG. Export it as PNG or JPEG and try again.",
                );
            }
            Err(_) => {
                return err_json(StatusCode::INTERNAL_SERVER_ERROR, "image conversion failed")
            }
        };
        let safe_name = sanitize_filename(&filename);
        let stem = FsPath::new(&safe_name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("image");
        filename = format!("{}.png", stem.chars().take(196).collect::<String>());
        media_type = "image/png";
        png.into()
    } else {
        bytes
    };
    match state
        .attachments
        .save_bytes(
            &conv_id,
            kind,
            AttachmentOrigin::Upload,
            &filename,
            media_type,
            &bytes,
        )
        .await
    {
        Ok(info) => Json(info).into_response(),
        Err(_) => {
            crate::diagnostics::db_failure("save_attachment", Some(&conv_id));
            err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not store attachment",
            )
        }
    }
}

/// GET /api/attachments/{id} — raw bytes when ready.
pub async fn get_attachment_bytes(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let meta = match state.attachments.get_meta(&id).await {
        Ok(Some(m)) => m,
        _ => return err_json(StatusCode::NOT_FOUND, "attachment not found"),
    };
    if meta.status != vortex_types::AttachmentStatus::Ready {
        return err_json(StatusCode::NOT_FOUND, "attachment is not ready");
    }
    let bytes = match state.attachments.read_bytes(&id).await {
        Ok(Some(b)) => b,
        _ => return err_json(StatusCode::NOT_FOUND, "attachment file is unavailable"),
    };
    let mut resp = Response::new(axum::body::Body::from(bytes));
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&meta.media_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename=\"{}\"",
            disposition_filename(&meta.filename)
        ))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=86400"),
    );
    resp
}

/// GET /api/attachments/{id}/meta — full metadata (UI polls pending videos).
pub async fn get_attachment_meta(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    match state.attachments.get_meta(&id).await {
        Ok(Some(m)) => Json(m).into_response(),
        Ok(None) => err_json(StatusCode::NOT_FOUND, "attachment not found"),
        Err(_) => {
            crate::diagnostics::db_failure("get_attachment", None);
            err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not load attachment metadata",
            )
        }
    }
}

/// GET /api/conversations/{id}/attachments — list (reference picker).
pub async fn list_attachments(
    State(state): State<Arc<AppState>>,
    Path(conv_id): Path<String>,
) -> Response {
    match state.attachments.list_for_conversation(&conv_id).await {
        Ok(list) => Json(list).into_response(),
        Err(_) => {
            crate::diagnostics::db_failure("list_attachments", Some(&conv_id));
            err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not list attachments",
            )
        }
    }
}

/// DELETE /api/attachments/{id} — row + file. Message refs dangle → tombstone.
pub async fn delete_attachment(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    match state.attachments.delete(&id).await {
        Ok(true) => Json(serde_json::json!({"ok": true})).into_response(),
        Ok(false) => err_json(StatusCode::NOT_FOUND, "attachment not found"),
        Err(_) => {
            crate::diagnostics::db_failure("delete_attachment", None);
            err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not delete attachment",
            )
        }
    }
}

/// Escape a filename for a quoted Content-Disposition value.
fn disposition_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '"' | '\\' | '\r' | '\n' => '_',
            _ => c,
        })
        .collect()
}

/// Route-level body limit override for the upload route.
pub fn upload_body_limit() -> DefaultBodyLimit {
    DefaultBodyLimit::max(ROUTE_BODY_MAX)
}

// Container headers are sufficient for upload classification; the provider
// decodes the media streams.
#[cfg(test)]
const VIDEO_HEADERS: [(&str, &str, &str, &[u8]); 4] = [
    (
        "clip.mp4",
        "video/mp4",
        "video/mp4",
        b"\0\0\0\x14ftypisom\0\0\x02\0mp41",
    ),
    (
        "clip.webm",
        "video/webm",
        "video/webm",
        b"\x1a\x45\xdf\xa3\x9f\x42\x86\x81\x01\x42\xf7\x81\x01\x42\xf2\x81\x04\x42\xf3\x81\x08\x42\x82\x84webm\x42\x87\x81\x02\x42\x85\x81\x02",
    ),
    (
        "clip.mov",
        "video/quicktime",
        "video/mov",
        b"\0\0\0\x14ftypqt  \0\0\0\0qt  ",
    ),
    ("clip.mpg", "video/mpeg", "video/mpeg", b"\0\0\x01\xba\x21"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_bytes_beat_extension_and_mime() {
        let png = b"\x89PNG\r\n\x1a\nrest";
        // A PNG named .txt with a text MIME is still an image.
        let (kind, mt) = classify("evil.txt", "text/plain", png).unwrap();
        assert_eq!(kind, AttachmentKind::Image);
        assert_eq!(mt, "image/png");
        // A text file named .png is NOT an image.
        assert!(classify("fake.png", "image/png", b"just text").is_err());
        assert!(classify("fake.png", "text/plain", b"just text").is_err());
        let (kind, mt) = classify("doc.bin", "application/octet-stream", b"%PDF-1.7 ...").unwrap();
        assert_eq!(kind, AttachmentKind::Pdf);
        assert_eq!(mt, "application/pdf");
    }

    #[test]
    fn image_signatures_detected() {
        assert_eq!(
            classify("a.jpg", "", b"\xff\xd8\xff\xe0xx").unwrap().1,
            "image/jpeg"
        );
        assert_eq!(classify("a.gif", "", b"GIF89a....").unwrap().1, "image/gif");
        let webp = b"RIFF\x00\x00\x00\x00WEBPVP8 ";
        assert_eq!(classify("a.webp", "", webp).unwrap().1, "image/webp");
    }

    #[test]
    fn video_signatures_not_extensions_determine_mime() {
        for (name, mime, _, bytes) in VIDEO_HEADERS {
            assert_eq!(
                classify("unknown.dat", "application/octet-stream", bytes).unwrap(),
                (AttachmentKind::Video, mime),
            );
            assert!(classify(name, "text/plain", b"not a video").is_err());
        }
        for unsupported in [
            b"\0\0\0\x14ftypheic\0\0\0\0isom".as_slice(),
            b"\0\0\0\x14ftypavif\0\0\0\0isom",
            b"\0\0\0\x14ftypM4A \0\0\0\0isom",
            b"\0\0\0\x14ftypisom", // truncated ftyp
            b"\x1a\x45\xdf\xa3\x8b\x42\x82\x88matroska",
            b"\x1a\x45\xdf\xa3\x87\x42\x83\x84webm", // not the DocType element
        ] {
            assert!(classify("fake.mp4", "video/mp4", unsupported).is_err());
        }
    }

    #[test]
    fn image_normalization_enforces_dimensions() {
        let mut bmp = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(4097, 1)
            .write_to(&mut bmp, ImageFormat::Bmp)
            .unwrap();
        assert!(matches!(
            normalize_image(bmp.get_ref(), ImageFormat::Bmp),
            Err(ImageError::Limits(_))
        ));
    }

    #[test]
    fn text_and_source_accepted() {
        assert_eq!(
            classify("notes.md", "", b"# hello").unwrap().0,
            AttachmentKind::Text
        );
        assert_eq!(
            classify("main.rs", "", b"fn main() {}").unwrap().0,
            AttachmentKind::Text
        );
        assert_eq!(
            classify("data.json", "", b"{\"a\":1}").unwrap().1,
            "application/json"
        );
        // No extension but declared text MIME.
        assert_eq!(
            classify("README", "text/plain", b"hello").unwrap().0,
            AttachmentKind::Text
        );
    }

    #[test]
    fn rejected_types_get_guidance() {
        assert!(classify("icon.svg", "image/svg+xml", b"<svg xmlns=...>").is_err());
        // SVG disguised as text.
        assert!(classify("icon.txt", "text/plain", b"  <svg></svg>").is_err());
        assert!(classify("doc.rtf", "", b"{\\rtf1 hello}").is_err());
        assert!(classify("doc.docx", "", b"PK\x03\x04binary").is_err());
        assert!(classify("photo.heic", "", b"\x00\x00\x00\x18ftypheic").is_err());
        assert!(classify("archive.zip", "", b"PK\x03\x04").is_err());
        // Binary with NUL is not text.
        assert!(classify("blob.txt", "text/plain", b"abc\x00def").is_err());
        // Unknown extension, non-text MIME, not UTF-8.
        assert!(classify("blob.xyz", "", b"\xff\xfe\x00\x01").is_err());
    }

    #[test]
    fn caps_match_kind() {
        assert_eq!(cap_for(AttachmentKind::Image), IMAGE_MAX);
        assert_eq!(cap_for(AttachmentKind::Pdf), PDF_MAX);
        assert_eq!(cap_for(AttachmentKind::Text), TEXT_MAX);
        assert_eq!(cap_for(AttachmentKind::Video), VIDEO_MAX);
    }

    #[test]
    fn disposition_filename_escapes_quotes() {
        assert_eq!(disposition_filename("a\"b\\c.png"), "a_b_c.png");
    }
}

#[cfg(test)]
mod route_tests {
    use super::*;
    use base64::prelude::{Engine as _, BASE64_STANDARD};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use vortex_llm::llm::{CompletionRequest, CompletionResponse, LlmClient, LlmEvent};
    use vortex_types::{
        AttachmentInfo, ChatRequestDto, ContentPart, Mode, ModelInfo, RunStatus, Settings,
    };

    /// Build a full AppState + the real guarded router, served on an
    /// ephemeral loopback port. Returns (base_url, state, tempdir).
    async fn test_app() -> (String, Arc<AppState>, tempfile::TempDir) {
        test_app_with_token(None).await
    }

    async fn test_app_with_token(
        token: Option<&str>,
    ) -> (String, Arc<AppState>, tempfile::TempDir) {
        let llm: Arc<dyn vortex_llm::LlmClient> =
            Arc::new(crate::mock_llm::DemoMockLlm::new("echo"));
        test_app_with_llm(llm, None, token).await
    }

    /// Same as [`test_app_with_token`] but with a custom LLM client and an
    /// optional `settings.model` override, so tests can pin the catalog the
    /// engine's shared cache will report.
    async fn test_app_with_llm(
        llm: Arc<dyn vortex_llm::LlmClient>,
        model: Option<&str>,
        token: Option<&str>,
    ) -> (String, Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = vortex_core::Db::open(dir.path().join("test.db")).unwrap();
        let mut settings = Settings::default();
        if let Some(model) = model {
            settings.model = model.to_string();
        }
        let attachments = Arc::new(
            vortex_core::AttachmentStore::new(dir.path().join("attachments"), db.clone()).unwrap(),
        );
        let hub = Arc::new(vortex_agents::Hub::new());
        let scheduler = Arc::new(vortex_agents::Scheduler::new(
            settings.agents.max_concurrent,
        ));
        let browser = Arc::new(vortex_agents::browser::BrowserManager::new(
            dir.path().join("profile"),
            true,
        ));
        let previews = Arc::new(vortex_agents::PreviewRegistry::new());
        let approvals = Arc::new(vortex_agents::approvals::ApprovalManager::new(
            db.clone(),
            hub.clone(),
            settings.agents.approval_timeout_secs,
        ));
        let engine_slot = Arc::new(std::sync::OnceLock::new());
        let registry = vortex_agents::build_registry(previews.clone(), engine_slot.clone());
        let http = vortex_agents::safe_http::SafeHttp::new(false);
        let engine = vortex_agents::Engine::new(
            llm,
            registry,
            hub.clone(),
            scheduler,
            approvals,
            db.clone(),
            browser,
            previews.clone(),
            None,
            http,
            settings.clone(),
            Some(attachments.clone()),
        );
        let _ = engine_slot.set(engine.clone());
        let mut config = vortex_core::AppConfig::default();
        config.llm.provider = "mock".into();
        let supervisor = vortex_agents::supervisor::Supervisor::new(
            db.clone(),
            engine.clone(),
            hub.clone(),
            Arc::new(vortex_agents::clock::SystemClock),
            2,
            tokio_util::sync::CancellationToken::new(),
        );
        let state = Arc::new(AppState {
            db: db.clone(),
            engine,
            hub,
            config,
            previews,
            attachments,
            is_mock_llm: true,
            version: "test",
            guard: crate::guard::GuardSettings {
                lan_enabled: token.is_some(),
                lan_token: Arc::from(token.unwrap_or_default()),
            },
            diagnostics: crate::diagnostics::DiagnosticsState::new(false),
            supervisor,
        });
        let app = crate::build_router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), state, dir)
    }

    fn multipart_body(files: &[(&str, &str, &[u8])]) -> (String, Vec<u8>) {
        let boundary = "----vortexboundary123";
        let mut body = Vec::new();
        for (filename, mime, bytes) in files {
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
                     filename=\"{filename}\"\r\nContent-Type: {mime}\r\n\r\n"
                )
                .as_bytes(),
            );
            body.extend_from_slice(bytes);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        (format!("multipart/form-data; boundary={boundary}"), body)
    }

    async fn new_conversation(state: &Arc<AppState>) -> String {
        state
            .db
            .create_conversation("t", Mode::Chat, None)
            .await
            .unwrap()
            .id
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDRfakepngdata";

    #[tokio::test]
    async fn lan_media_reads_accept_query_token_and_writes_stay_guarded() {
        let (base, state, _dir) = test_app_with_token(Some("testToken")).await;
        let conv = new_conversation(&state).await;
        let client = reqwest::Client::new();
        let upload = format!("{base}/api/conversations/{conv}/attachments");
        let (ct, body) = multipart_body(&[("cat.png", "image/png", PNG)]);
        assert_eq!(
            client
                .post(&upload)
                .header("Content-Type", &ct)
                .body(body.clone())
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        let info: AttachmentInfo = client
            .post(&upload)
            .bearer_auth("testToken")
            .header("Content-Type", ct)
            .body(body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let media = format!("{base}/api/attachments/{}", info.id);
        assert_eq!(client.get(&media).send().await.unwrap().status(), 403);
        assert_eq!(
            client
                .get(format!("{media}/meta"))
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
        assert_eq!(client.get(&upload).send().await.unwrap().status(), 403);
        assert_eq!(client.delete(&media).send().await.unwrap().status(), 403);
        let bytes = client
            .get(format!("{media}?token=testToken"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), PNG);
    }

    #[tokio::test]
    async fn upload_serve_list_delete_roundtrip() {
        let (base, state, _dir) = test_app().await;
        let conv = new_conversation(&state).await;
        let client = reqwest::Client::new();
        let (ct, body) = multipart_body(&[("cat.png", "image/png", PNG)]);
        let resp = client
            .post(format!("{base}/api/conversations/{conv}/attachments"))
            .header("Content-Type", ct)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let info: AttachmentInfo = resp.json().await.unwrap();
        let id = info.id;
        assert_eq!(info.kind, AttachmentKind::Image);
        assert_eq!(info.media_type, "image/png");
        assert_eq!(info.origin, AttachmentOrigin::Upload);

        // Serve bytes with safe headers.
        let resp = client
            .get(format!("{base}/api/attachments/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-type"], "image/png");
        assert_eq!(resp.headers()["x-content-type-options"], "nosniff");
        assert!(resp.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .contains("attachment"));
        assert_eq!(resp.bytes().await.unwrap().as_ref(), PNG);

        // Meta + list.
        let resp = client
            .get(format!("{base}/api/attachments/{id}/meta"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let resp = client
            .get(format!("{base}/api/conversations/{conv}/attachments"))
            .send()
            .await
            .unwrap();
        let list: Vec<AttachmentInfo> = resp.json().await.unwrap();
        assert_eq!(list.len(), 1);

        // Delete → 404 afterwards.
        let resp = client
            .delete(format!("{base}/api/attachments/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let resp = client
            .get(format!("{base}/api/attachments/{id}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn unsupported_type_rejected_with_415() {
        let (base, state, _dir) = test_app().await;
        let conv = new_conversation(&state).await;
        let client = reqwest::Client::new();
        let (ct, body) = multipart_body(&[("icon.svg", "image/svg+xml", b"<svg></svg>")]);
        let resp = client
            .post(format!("{base}/api/conversations/{conv}/attachments"))
            .header("Content-Type", ct)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 415);
        let v: serde_json::Value = resp.json().await.unwrap();
        let error = v["error"].as_str().unwrap();
        for hint in [
            "MP4/M4V, WebM, MOV, or MPEG",
            "PNG, JPEG, WebP, GIF, BMP, TIFF, or ICO",
            "Convert other videos to MP4",
            "SVG) to PNG or JPEG",
            "extract supported files from archives",
        ] {
            assert!(error.contains(hint), "missing {hint:?}: {error}");
        }
        // Nothing stored.
        assert!(state
            .attachments
            .list_for_conversation(&conv)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn oversized_text_rejected_with_413() {
        let (base, state, _dir) = test_app().await;
        let conv = new_conversation(&state).await;
        let client = reqwest::Client::new();
        let big = vec![b'a'; TEXT_MAX + 1];
        let (ct, body) = multipart_body(&[("big.txt", "text/plain", &big)]);
        let resp = client
            .post(format!("{base}/api/conversations/{conv}/attachments"))
            .header("Content-Type", ct)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 413);
    }

    #[tokio::test]
    async fn route_body_limit_override_accepts_large_pdf() {
        // A 3 MiB PDF exceeds the router-wide 2 MiB default but is under the
        // upload route's 25 MiB override and the 20 MiB PDF cap → must succeed.
        let (base, state, _dir) = test_app().await;
        let conv = new_conversation(&state).await;
        let client = reqwest::Client::new();
        let mut pdf = b"%PDF-1.7\n".to_vec();
        pdf.resize(3 * 1024 * 1024, b'x');
        let (ct, body) = multipart_body(&[("big.pdf", "application/pdf", &pdf)]);
        let resp = client
            .post(format!("{base}/api/conversations/{conv}/attachments"))
            .header("Content-Type", ct)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "route-level body limit must override the 2 MiB default"
        );
        let info: AttachmentInfo = resp.json().await.unwrap();
        assert_eq!(info.kind, AttachmentKind::Pdf);
    }

    #[tokio::test]
    async fn chat_with_attachments_persists_refs_and_allows_empty_text() {
        let (base, state, _dir) = test_app().await;
        let conv = new_conversation(&state).await;
        let info = state
            .attachments
            .save_bytes(
                &conv,
                AttachmentKind::Image,
                AttachmentOrigin::Upload,
                "cat.png",
                "image/png",
                PNG,
            )
            .await
            .unwrap();
        let client = reqwest::Client::new();
        // Empty text + attachment is allowed.
        let dto = ChatRequestDto {
            content: String::new(),
            mode: None,
            attachment_ids: vec![info.id.clone()],
        };
        let resp = client
            .post(format!("{base}/api/conversations/{conv}/chat"))
            .json(&dto)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let msgs = state.db.get_messages(conv.clone()).await.unwrap();
        let user = msgs
            .iter()
            .find(|m| m.role == vortex_types::Role::User)
            .unwrap();
        assert_eq!(user.attachments.len(), 1);
        assert_eq!(user.attachments[0].id, info.id);

        // Foreign attachment id is rejected.
        let other_conv = new_conversation(&state).await;
        let foreign = state
            .attachments
            .save_bytes(
                &other_conv,
                AttachmentKind::Image,
                AttachmentOrigin::Upload,
                "x.png",
                "image/png",
                PNG,
            )
            .await
            .unwrap();
        let dto = ChatRequestDto {
            content: "hi".into(),
            mode: None,
            attachment_ids: vec![foreign.id],
        };
        let resp = client
            .post(format!("{base}/api/conversations/{conv}/chat"))
            .json(&dto)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    // ---- Phase 3: attachment model input (route integration) ----

    /// LLM client that captures every [`CompletionRequest`] and counts
    /// catalog fetches, so tests can assert on the exact provider payload
    /// and on shared model-catalog cache use. Never touches the network.
    struct CaptureLlm {
        catalog: Vec<ModelInfo>,
        fail_catalog: bool,
        captured: Arc<tokio::sync::Mutex<Vec<CompletionRequest>>>,
        catalog_calls: Arc<AtomicUsize>,
    }

    #[derive(Clone)]
    struct MockHandles {
        captured: Arc<tokio::sync::Mutex<Vec<CompletionRequest>>>,
        catalog_calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl LlmClient for CaptureLlm {
        async fn complete(
            &self,
            req: &CompletionRequest,
            _events: tokio::sync::mpsc::Sender<LlmEvent>,
        ) -> Result<CompletionResponse, vortex_llm::LlmError> {
            self.captured.lock().await.push(req.clone());
            if req
                .messages
                .iter()
                .rev()
                .find(|msg| msg.role == vortex_types::Role::User)
                .is_some_and(|msg| msg.content == "Reject this context fixture")
            {
                return Err(vortex_llm::LlmError::Provider(
                    400,
                    "context window exceeded".into(),
                ));
            }
            Ok(CompletionResponse {
                content: "ok".into(),
                tool_calls: Vec::new(),
                usage: None,
                finish_reason: "stop".into(),
                reported_completion_tokens: None,
                annotations: None,
                model: None,
            })
        }

        async fn models(&self) -> Result<Vec<ModelInfo>, vortex_llm::LlmError> {
            self.catalog_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_catalog {
                return Err(vortex_llm::LlmError::Network("catalog offline".into()));
            }
            Ok(self.catalog.clone())
        }

        fn describe(&self) -> &'static str {
            "capture-mock"
        }
    }

    fn capture_llm(
        catalog: Vec<ModelInfo>,
        fail_catalog: bool,
    ) -> (Arc<dyn LlmClient>, MockHandles) {
        let handles = MockHandles {
            captured: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            catalog_calls: Arc::new(AtomicUsize::new(0)),
        };
        let llm = Arc::new(CaptureLlm {
            catalog,
            fail_catalog,
            captured: handles.captured.clone(),
            catalog_calls: handles.catalog_calls.clone(),
        });
        (llm, handles)
    }

    fn model_info(id: &str, inputs: &[&str]) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: id.into(),
            context_length: Some(8192),
            supports_tools: Some(true),
            pricing_prompt: None,
            pricing_completion: None,
            created: None,
            pricing_request: None,
            pricing_image: None,
            input_modalities: inputs.iter().map(|s| s.to_string()).collect(),
            output_modalities: vec!["text".into()],
            ..ModelInfo::default()
        }
    }

    /// One known vision-capable and one known text-only catalog entry.
    fn vision_catalog() -> Vec<ModelInfo> {
        vec![
            model_info("vision/model", &["text", "image"]),
            model_info("text/only", &["text"]),
        ]
    }

    async fn save_png(state: &Arc<AppState>, conv: &str) -> AttachmentInfo {
        state
            .attachments
            .save_bytes(
                conv,
                AttachmentKind::Image,
                AttachmentOrigin::Upload,
                "cat.png",
                "image/png",
                PNG,
            )
            .await
            .unwrap()
    }

    async fn post_chat(
        base: &str,
        conv: &str,
        dto: &ChatRequestDto,
    ) -> (StatusCode, serde_json::Value) {
        let resp = reqwest::Client::new()
            .post(format!("{base}/api/conversations/{conv}/chat"))
            .json(dto)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        (status, resp.json().await.unwrap())
    }

    /// POST chat and require the run to start; returns the run id.
    async fn start_run(base: &str, conv: &str, dto: &ChatRequestDto) -> String {
        let (status, body) = post_chat(base, conv, dto).await;
        assert_eq!(status, 200, "chat start failed: {body}");
        body["run_id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn chat_beyond_history_window_preserves_replies_and_accepts_continuation() {
        let (client, handles) = capture_llm(Vec::new(), false);
        let (base, state, _dir) = test_app_with_llm(client, Some("test/model"), None).await;
        let conv = new_conversation(&state).await;
        for i in 0..42 {
            let content = if i == 41 {
                "Continue from where you left off.".into()
            } else {
                format!("Turn marker {i}")
            };
            let id = start_run(
                &base,
                &conv,
                &ChatRequestDto {
                    content,
                    mode: Some(Mode::Chat),
                    attachment_ids: Vec::new(),
                },
            )
            .await;
            wait_for_run_completed(&state, &id).await;
        }
        let stored = reqwest::get(format!("{base}/api/conversations/{conv}/messages"))
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<Vec<vortex_types::StoredMessage>>()
            .await
            .unwrap();
        assert_eq!(stored.len(), 84);
        assert_eq!(stored[0].content, "Turn marker 0");
        let summary = stored.last().unwrap().reply_summary.as_ref().unwrap();
        assert_eq!(summary.finish, vortex_types::dto::ReplyFinish::Stop);
        assert_eq!(summary.completion_tokens, None);
        let requests = handles.captured.lock().await;
        let last = requests.last().unwrap();
        assert!(!last
            .messages
            .iter()
            .any(|msg| msg.content == "Turn marker 0"));
        assert!(last
            .messages
            .iter()
            .any(|msg| msg.content == "Continue from where you left off."));
    }

    #[tokio::test]
    async fn provider_context_rejection_is_not_an_output_limit_reply() {
        let (client, _) = capture_llm(Vec::new(), false);
        let (base, state, _dir) = test_app_with_llm(client, Some("test/model"), None).await;
        let conv = new_conversation(&state).await;
        let turn = |content: &str| ChatRequestDto {
            content: content.into(),
            mode: Some(Mode::Chat),
            attachment_ids: Vec::new(),
        };
        let id = start_run(&base, &conv, &turn("Reject this context fixture")).await;
        let run = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(run) = state.db.get_run(id.clone()).await.unwrap() {
                    if run.status.is_terminal() {
                        break run;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(run.status, RunStatus::Failed);
        assert!(run.error.unwrap().contains("context window exceeded"));
        assert_eq!(state.db.get_messages(conv.clone()).await.unwrap().len(), 1);
        let next = start_run(&base, &conv, &turn("Try a shorter request")).await;
        wait_for_run_completed(&state, &next).await;
    }

    #[tokio::test]
    async fn saving_model_default_preserves_existing_chat_preference() {
        let (llm, _) = capture_llm(Vec::new(), false);
        let (base, _, _dir) = test_app_with_llm(llm, Some("test/original"), None).await;
        let client = reqwest::Client::new();
        let create = || async {
            client
                .post(format!("{base}/api/conversations"))
                .json(&serde_json::json!({"mode": "chat", "title": "Pin test"}))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<vortex_types::ConversationSummary>()
                .await
                .unwrap()
        };
        let before = create().await;
        let mut settings = client
            .get(format!("{base}/api/settings"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<vortex_types::Settings>()
            .await
            .unwrap();
        settings.model = "test/new-default".into();
        client
            .put(format!("{base}/api/settings"))
            .json(&settings)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let after = create().await;
        let chat_model = |id: &str| format!("{base}/api/conversations/{id}/model");
        assert_eq!(after.model.as_deref(), Some("test/new-default"));
        let get_model = |url| async {
            client
                .get(url)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        };
        assert_eq!(
            get_model(chat_model(&before.id)).await["model"],
            "test/original"
        );
        client
            .patch(chat_model(&before.id))
            .json(&serde_json::json!({"model": "test/override"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert_eq!(
            get_model(chat_model(&before.id)).await["model"],
            "test/override"
        );
        assert_eq!(
            get_model(chat_model(&after.id)).await["model"],
            "test/new-default"
        );
    }

    async fn post_retry(base: &str, run_id: &str) -> (StatusCode, serde_json::Value) {
        let resp = reqwest::Client::new()
            .post(format!("{base}/api/runs/{run_id}/retry"))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        (status, resp.json().await.unwrap())
    }

    async fn wait_for_run_completed(state: &Arc<AppState>, run_id: &str) {
        for _ in 0..400 {
            if let Ok(Some(run)) = state.db.get_run(run_id.to_string()).await {
                match run.status {
                    RunStatus::Completed => return,
                    RunStatus::Queued | RunStatus::Running | RunStatus::WaitingApproval => {}
                    other => panic!("run {run_id} ended as {other:?}: {:?}", run.error),
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("run {run_id} did not complete in time");
    }

    async fn wait_for_captures(handles: &MockHandles, n: usize) -> Vec<CompletionRequest> {
        for _ in 0..400 {
            let captured = handles.captured.lock().await.clone();
            if captured.len() >= n {
                return captured;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!(
            "expected {n} provider request(s), saw {}",
            handles.captured.lock().await.len()
        );
    }

    fn last_user(req: &CompletionRequest) -> &vortex_types::ChatMessage {
        req.messages
            .iter()
            .rev()
            .find(|m| m.role == vortex_types::Role::User)
            .expect("request must carry the user turn")
    }

    /// The current user turn must be hydrated to a real PNG data URL.
    fn assert_hydrated_png(msg: &vortex_types::ChatMessage) {
        const PREFIX: &str = "data:image/png;base64,";
        let parts = msg
            .parts
            .as_ref()
            .expect("current user turn must be hydrated");
        assert!(
            parts.iter().any(|p| matches!(
                p,
                ContentPart::ImageUrl { image_url }
                    if image_url.url.starts_with(PREFIX) && image_url.url.len() > PREFIX.len()
            )),
            "missing png data URL part: {parts:?}"
        );
    }

    async fn upload_file(base: &str, conv: &str, name: &str, bytes: &[u8]) -> AttachmentInfo {
        let (ct, body) = multipart_body(&[(name, "application/octet-stream", bytes)]);
        let response = reqwest::Client::new()
            .post(format!("{base}/api/conversations/{conv}/attachments"))
            .header("Content-Type", ct)
            .body(body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(status, 200, "uploading {name}: {body}");
        serde_json::from_str(&body).unwrap()
    }

    #[tokio::test]
    async fn uploaded_videos_are_served_and_sent_to_auto_router() {
        let (llm, handles) = capture_llm(
            vec![model_info("openrouter/auto", &["text", "image", "video"])],
            false,
        );
        let (base, state, _dir) = test_app_with_llm(llm, Some("openrouter/auto"), None).await;
        let conv = new_conversation(&state).await;
        let mut ids = Vec::new();
        for (name, mime, _, bytes) in VIDEO_HEADERS {
            let info = upload_file(&base, &conv, name, bytes).await;
            assert_eq!(info.kind, AttachmentKind::Video);
            assert_eq!(info.media_type, mime);
            let response = reqwest::get(format!("{base}/api/attachments/{}", info.id))
                .await
                .unwrap()
                .error_for_status()
                .unwrap();
            assert_eq!(response.headers()["content-type"], mime);
            assert_eq!(response.bytes().await.unwrap().as_ref(), bytes);
            ids.push(info.id);
        }

        let dto = ChatRequestDto {
            content: "Describe these videos".into(),
            mode: Some(Mode::Chat),
            attachment_ids: ids.clone(),
        };
        let run_id = start_run(&base, &conv, &dto).await;
        let requests = wait_for_captures(&handles, 1).await;
        assert_eq!(requests[0].model, "openrouter/auto");
        let parts = last_user(&requests[0]).parts.as_deref().unwrap();
        assert_eq!(parts.len(), 1 + VIDEO_HEADERS.len());
        assert!(matches!(&parts[0], ContentPart::Text { text } if text == &dto.content));
        for (part, (_, _, wire_mime, bytes)) in parts[1..].iter().zip(VIDEO_HEADERS) {
            let ContentPart::VideoUrl { video_url } = part else {
                panic!("expected a video part");
            };
            assert_eq!(
                video_url.url,
                format!("data:{wire_mime};base64,{}", BASE64_STANDARD.encode(bytes))
            );
        }
        let messages = state.db.get_messages(conv).await.unwrap();
        let user = messages
            .iter()
            .find(|m| m.role == vortex_types::Role::User)
            .unwrap();
        assert_eq!(user.content, dto.content);
        assert_eq!(
            user.attachments.iter().map(|a| &a.id).collect::<Vec<_>>(),
            ids.iter().collect::<Vec<_>>()
        );
        wait_for_run_completed(&state, &run_id).await;
    }

    #[tokio::test]
    async fn video_on_an_image_only_model_is_rejected_before_persistence() {
        let (llm, handles) = capture_llm(vision_catalog(), false);
        let (base, state, _dir) = test_app_with_llm(llm, Some("vision/model"), None).await;
        let conv = new_conversation(&state).await;
        let (name, _, _, bytes) = VIDEO_HEADERS[0];
        let clip = upload_file(&base, &conv, name, bytes).await;
        let (status, body) = post_chat(
            &base,
            &conv,
            &ChatRequestDto {
                content: "Describe the clip".into(),
                mode: None,
                attachment_ids: vec![clip.id],
            },
        )
        .await;
        assert_eq!(status, 400);
        let message = body["error"].as_str().unwrap();
        assert!(
            message.contains("does not support video input"),
            "{message}"
        );
        assert!(message.contains("openrouter/auto"), "{message}");
        assert!(state.db.get_messages(conv).await.unwrap().is_empty());
        assert!(handles.captured.lock().await.is_empty());
    }

    #[tokio::test]
    async fn additional_image_formats_are_normalized_to_png_for_chat() {
        let (llm, handles) = capture_llm(vision_catalog(), false);
        let (base, state, _dir) = test_app_with_llm(llm, Some("vision/model"), None).await;
        let conv = new_conversation(&state).await;
        // PNG inside ICO requires an alpha channel.
        let pixel = image::Rgba([10, 20, 30, 255]);
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(2, 2, pixel));
        let mut attachment_ids = Vec::new();
        for (ext, format) in [
            ("bmp", ImageFormat::Bmp),
            ("tif", ImageFormat::Tiff),
            ("ico", ImageFormat::Ico),
        ] {
            let mut encoded = Cursor::new(Vec::new());
            image.write_to(&mut encoded, format).unwrap();
            let info = upload_file(
                &base,
                &conv,
                &format!("test-{ext}.{ext}"),
                encoded.get_ref(),
            )
            .await;
            assert_eq!(info.kind, AttachmentKind::Image);
            assert_eq!(info.media_type, "image/png");
            assert_eq!(info.filename, format!("test-{ext}.png"));
            let stored = state
                .attachments
                .read_bytes(&info.id)
                .await
                .unwrap()
                .unwrap();
            let decoded = image::load_from_memory_with_format(&stored, ImageFormat::Png).unwrap();
            assert_eq!(decoded.to_rgb8(), image.to_rgb8());
            attachment_ids.push(info.id);
        }
        let dto = ChatRequestDto {
            content: "".into(),
            mode: Some(Mode::Chat),
            attachment_ids,
        };
        let run_id = start_run(&base, &conv, &dto).await;
        let requests = wait_for_captures(&handles, 1).await;
        let user = last_user(&requests[0]);
        let parts = user.parts.as_deref().unwrap();
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|part| matches!(part, ContentPart::ImageUrl { image_url } if image_url.url.starts_with("data:image/png;base64,"))));
        wait_for_run_completed(&state, &run_id).await;
    }

    #[tokio::test]
    async fn known_text_only_model_rejects_current_image_before_persistence() {
        let (llm, handles) = capture_llm(vision_catalog(), false);
        let (base, state, _dir) = test_app_with_llm(llm, Some("text/only"), None).await;
        let conv = new_conversation(&state).await;
        let png = save_png(&state, &conv).await;
        let (status, body) = post_chat(
            &base,
            &conv,
            &ChatRequestDto {
                content: "what is in this picture?".into(),
                mode: None,
                attachment_ids: vec![png.id],
            },
        )
        .await;
        assert_eq!(status, 400);
        let err = body["error"].as_str().unwrap();
        assert!(err.contains("does not support image input"), "{err}");
        assert!(err.contains("Switch to a model"), "{err}");
        // Rejected before any message or run was persisted, and the provider
        // was never called.
        assert!(state
            .db
            .get_messages(conv.clone())
            .await
            .unwrap()
            .is_empty());
        assert!(state
            .db
            .list_runs_for_conversation(conv)
            .await
            .unwrap()
            .is_empty());
        assert!(handles.captured.lock().await.is_empty());
    }

    #[tokio::test]
    async fn retry_rejects_current_image_after_switch_to_known_text_only_model() {
        let (llm, handles) = capture_llm(vision_catalog(), false);
        let (base, state, _dir) = test_app_with_llm(llm, Some("vision/model"), None).await;
        let conv = new_conversation(&state).await;
        let png = save_png(&state, &conv).await;
        let run_id = start_run(
            &base,
            &conv,
            &ChatRequestDto {
                content: "describe".into(),
                mode: None,
                attachment_ids: vec![png.id],
            },
        )
        .await;
        wait_for_run_completed(&state, &run_id).await;

        // Switch to a known text-only model: the retry carries the same
        // current image and must be refused with the switch-model hint.
        let mut settings = state.engine.current_settings().await;
        settings.model = "text/only".into();
        state.engine.update_settings(settings).await;

        let (status, body) = post_retry(&base, &run_id).await;
        assert_eq!(status, 400);
        let err = body["error"].as_str().unwrap();
        assert!(err.contains("does not support image input"), "{err}");
        assert!(err.contains("Switch to a model"), "{err}");

        // No new run was started and nothing new persisted: still one run and
        // the original user + assistant messages, one provider call total.
        let runs = state
            .db
            .list_runs_for_conversation(conv.clone())
            .await
            .unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(state.db.get_messages(conv).await.unwrap().len(), 2);
        assert_eq!(handles.captured.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn model_missing_from_catalog_is_unknown_and_allows_image() {
        let (llm, handles) = capture_llm(vision_catalog(), false);
        let (base, state, _dir) = test_app_with_llm(llm, Some("acme/not-listed"), None).await;
        let conv = new_conversation(&state).await;
        let png = save_png(&state, &conv).await;
        let run_id = start_run(
            &base,
            &conv,
            &ChatRequestDto {
                content: "describe".into(),
                mode: None,
                attachment_ids: vec![png.id.clone()],
            },
        )
        .await;
        let captured = wait_for_captures(&handles, 1).await;
        let user = last_user(&captured[0]);
        assert!(user.attachments.iter().any(|r| r.id == png.id));
        assert_hydrated_png(user);
        wait_for_run_completed(&state, &run_id).await;
        let msgs = state.db.get_messages(conv).await.unwrap();
        let stored = msgs
            .iter()
            .find(|m| m.role == vortex_types::Role::User)
            .unwrap();
        assert_eq!(stored.attachments.len(), 1);
        assert_eq!(stored.attachments[0].id, png.id);
    }

    #[tokio::test]
    async fn catalog_failure_is_unknown_and_allows_image() {
        let (llm, handles) = capture_llm(Vec::new(), true);
        let (base, state, _dir) = test_app_with_llm(llm, Some("vision/model"), None).await;
        let conv = new_conversation(&state).await;
        let png = save_png(&state, &conv).await;
        let run_id = start_run(
            &base,
            &conv,
            &ChatRequestDto {
                content: "describe".into(),
                mode: None,
                attachment_ids: vec![png.id],
            },
        )
        .await;
        wait_for_captures(&handles, 1).await;
        wait_for_run_completed(&state, &run_id).await;
    }

    #[tokio::test]
    async fn retry_preserves_attachment_refs_in_provider_request() {
        let (llm, handles) = capture_llm(vision_catalog(), false);
        let (base, state, _dir) = test_app_with_llm(llm, Some("vision/model"), None).await;
        let conv = new_conversation(&state).await;
        let png = save_png(&state, &conv).await;
        let run_id = start_run(
            &base,
            &conv,
            &ChatRequestDto {
                content: "describe".into(),
                mode: None,
                attachment_ids: vec![png.id.clone()],
            },
        )
        .await;
        wait_for_run_completed(&state, &run_id).await;
        wait_for_captures(&handles, 1).await;

        let (status, body) = post_retry(&base, &run_id).await;
        assert_eq!(status, 200, "retry failed: {body}");
        let retry_run = body["run_id"].as_str().unwrap().to_string();
        assert_ne!(retry_run, run_id);

        let captured = wait_for_captures(&handles, 2).await;
        wait_for_run_completed(&state, &retry_run).await;

        // The retried provider request must carry the same attachment refs
        // as the original, hydrated again for the model.
        let original = last_user(&captured[0]);
        let retried = last_user(&captured[1]);
        assert!(original.attachments.iter().any(|r| r.id == png.id));
        assert_eq!(
            retried
                .attachments
                .iter()
                .map(|r| r.id.clone())
                .collect::<Vec<_>>(),
            original
                .attachments
                .iter()
                .map(|r| r.id.clone())
                .collect::<Vec<_>>(),
            "retry must preserve attachment refs"
        );
        assert_hydrated_png(retried);

        // Retry reuses the stored user turn: it is not persisted twice.
        let msgs = state.db.get_messages(conv).await.unwrap();
        assert_eq!(
            msgs.iter()
                .filter(|m| m.role == vortex_types::Role::User)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn models_route_and_preflight_share_one_catalog_cache() {
        let (llm, handles) = capture_llm(vision_catalog(), false);
        let (base, state, _dir) = test_app_with_llm(llm, Some("vision/model"), None).await;
        let client = reqwest::Client::new();
        for _ in 0..2 {
            let resp = client
                .get(format!("{base}/api/models"))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            let body: serde_json::Value = resp.json().await.unwrap();
            assert_eq!(body["models"][0]["id"], "vision/model");
            assert!(body["configured"].as_bool().unwrap());
        }
        assert_eq!(
            handles.catalog_calls.load(Ordering::SeqCst),
            1,
            "the second /api/models call must be served from the shared cache"
        );

        // Chat preflight and hydration must reuse the same cached catalog.
        let conv = new_conversation(&state).await;
        let png = save_png(&state, &conv).await;
        let run_id = start_run(
            &base,
            &conv,
            &ChatRequestDto {
                content: "describe".into(),
                mode: None,
                attachment_ids: vec![png.id],
            },
        )
        .await;
        wait_for_captures(&handles, 1).await;
        wait_for_run_completed(&state, &run_id).await;
        assert_eq!(
            handles.catalog_calls.load(Ordering::SeqCst),
            1,
            "preflight/hydration must reuse the cached catalog"
        );
    }

    #[tokio::test]
    async fn current_turn_over_media_budget_rejected_before_persistence() {
        let (llm, handles) = capture_llm(vision_catalog(), false);
        let (base, state, _dir) = test_app_with_llm(llm, Some("vision/model"), None).await;
        let conv = new_conversation(&state).await;
        // Two 13 MiB PDFs encode to ~34.7 MiB of base64 wire data, over the
        // 32 MiB per-completion media budget.
        let mut pdf = b"%PDF-1.7\n".to_vec();
        pdf.resize(13 * 1024 * 1024, b'x');
        let mut ids = Vec::new();
        for i in 0..2 {
            let info = state
                .attachments
                .save_bytes(
                    &conv,
                    AttachmentKind::Pdf,
                    AttachmentOrigin::Upload,
                    &format!("doc{i}.pdf"),
                    "application/pdf",
                    &pdf,
                )
                .await
                .unwrap();
            ids.push(info.id);
        }
        drop(pdf);
        let (status, body) = post_chat(
            &base,
            &conv,
            &ChatRequestDto {
                content: "read both".into(),
                mode: None,
                attachment_ids: ids,
            },
        )
        .await;
        assert_eq!(status, 400);
        let err = body["error"].as_str().unwrap();
        assert!(err.contains("send fewer or smaller files"), "{err}");
        // Rejected before any message or run was persisted.
        assert!(state
            .db
            .get_messages(conv.clone())
            .await
            .unwrap()
            .is_empty());
        assert!(state
            .db
            .list_runs_for_conversation(conv)
            .await
            .unwrap()
            .is_empty());
        assert!(handles.captured.lock().await.is_empty());
    }
}
