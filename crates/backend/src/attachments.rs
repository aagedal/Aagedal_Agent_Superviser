//! Image attachments shared by native GPUI, Tauri web UI, OpenCode ACP, and the built-in agent.

use std::path::Path;

use anyhow::{Context, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use superviser_core::project::Project;
use superviser_core::sandbox::Isolation;

/// Maximum size of a single attached image (20 MiB).
pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
/// Maximum number of images attached to one message.
pub const MAX_IMAGES_PER_MESSAGE: usize = 8;
/// Prefix used for auto-appended image file path metadata notes so UIs can keep chat bubbles clean.
pub const IMAGE_PATHS_HEADER: &str = "[Attached image file paths]";

/// An image attached to a chat message (dragged & dropped, pasted from clipboard, or picked).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageAttachment {
    /// Human-readable file or paste label (e.g. `"screenshot.png"`, `"Pasted image"`).
    pub name: String,
    /// Canonical MIME type (e.g. `"image/png"`, `"image/jpeg"`, `"image/webp"`, `"image/gif"`).
    pub mime_type: String,
    /// Standard base64-encoded image bytes (without a `data:...;base64,` prefix).
    pub data_base64: String,
    /// Original byte length of the image (optional in incoming JSON payloads).
    #[serde(default)]
    pub size_bytes: usize,
    /// Original local filesystem path when dragged & dropped or picked from disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

impl ImageAttachment {
    /// Validates and base64-encodes raw image bytes into an attachment.
    pub fn from_bytes(name: String, mime_type: &str, bytes: &[u8]) -> anyhow::Result<Self> {
        if bytes.is_empty() {
            bail!("{name} is empty");
        }
        if bytes.len() > MAX_IMAGE_BYTES {
            bail!(
                "{name} is too large ({} — max {} MB)",
                format_bytes(bytes.len()),
                MAX_IMAGE_BYTES / (1024 * 1024)
            );
        }
        let canonical = normalize_mime(mime_type)
            .or_else(|| detect_mime_from_bytes(bytes))
            .with_context(|| format!("{name} has unsupported image format ({mime_type})"))?;
        Ok(Self {
            name,
            mime_type: canonical.to_string(),
            data_base64: BASE64.encode(bytes),
            size_bytes: bytes.len(),
            source_path: None,
        })
    }

    /// Reads an image file from disk, validates its type & size, and encodes it (keeping its path).
    pub fn from_path(path: &Path) -> anyhow::Result<Self> {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("image")
            .to_string();
        let meta = std::fs::metadata(path)
            .with_context(|| format!("could not read {}", path.display()))?;
        if !meta.is_file() {
            bail!("{name} is not a file");
        }
        if meta.len() as usize > MAX_IMAGE_BYTES {
            bail!(
                "{name} is too large ({} — max {} MB)",
                format_bytes(meta.len() as usize),
                MAX_IMAGE_BYTES / (1024 * 1024)
            );
        }
        let bytes = std::fs::read(path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let mime = detect_mime_from_bytes(&bytes)
            .or_else(|| mime_from_extension(path))
            .with_context(|| format!("{name} is not a supported image (PNG, JPEG, WebP, GIF, BMP, TIFF)"))?;
        let mut att = Self::from_bytes(name, mime, &bytes)?;
        let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        att.source_path = Some(resolved.display().to_string());
        Ok(att)
    }

    /// Decodes the base64 payload back into raw bytes (e.g. to render a preview thumbnail).
    pub fn decode_bytes(&self) -> anyhow::Result<Vec<u8>> {
        BASE64
            .decode(self.data_base64.trim())
            .context("invalid base64 image data")
    }

    /// Formats as an RFC 2397 `data:` URI for OpenAI multimodal `image_url` payloads.
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime_type, self.data_base64)
    }

    /// Parses a `data:image/...;base64,...` URI back into an [`ImageAttachment`].
    pub fn from_data_url(name: String, url: &str) -> Option<Self> {
        let rest = url.strip_prefix("data:")?;
        let (header, data) = rest.split_once(',')?;
        let mime_raw = header.strip_suffix(";base64")?;
        let mime = normalize_mime(mime_raw)?;
        let decoded_len = BASE64.decode(data.trim()).ok()?.len();
        Some(Self {
            name,
            mime_type: mime.to_string(),
            data_base64: data.trim().to_string(),
            size_bytes: decoded_len,
            source_path: None,
        })
    }

    /// Validates an attachment received from a UI frontend (e.g. Tauri IPC).
    pub fn validate(&self) -> anyhow::Result<()> {
        if normalize_mime(&self.mime_type).is_none() {
            bail!("unsupported image MIME type: {}", self.mime_type);
        }
        let trimmed = self.data_base64.trim();
        if trimmed.is_empty() {
            bail!("{} has no image data", self.name);
        }
        let approx_bytes = (trimmed.len() * 3) / 4;
        if approx_bytes > MAX_IMAGE_BYTES + 64 {
            bail!("{} exceeds the {} MB limit", self.name, MAX_IMAGE_BYTES / (1024 * 1024));
        }
        Ok(())
    }
}

/// Canonicalizes supported image MIME types (`image/jpg` -> `image/jpeg`, etc.).
pub fn normalize_mime(mime: &str) -> Option<&'static str> {
    match mime.trim().to_ascii_lowercase().as_str() {
        "image/png" => Some("image/png"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/webp" => Some("image/webp"),
        "image/gif" => Some("image/gif"),
        "image/bmp" | "image/x-ms-bmp" => Some("image/bmp"),
        "image/tiff" | "image/tif" => Some("image/tiff"),
        _ => None,
    }
}

/// Infers an image MIME type from a file path's extension.
pub fn mime_from_extension(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        "bmp" => Some("image/bmp"),
        "tif" | "tiff" => Some("image/tiff"),
        _ => None,
    }
}

/// Whether `path` has a supported image file extension (for drag-and-drop / paste filtering).
pub fn is_supported_image_path(path: &Path) -> bool {
    mime_from_extension(path).is_some()
}

/// Infers an image MIME type from leading magic bytes.
pub fn detect_mime_from_bytes(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && bytes[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return Some("image/png");
    }
    if bytes.len() >= 3 && bytes[..3] == [0xFF, 0xD8, 0xFF] {
        return Some("image/jpeg");
    }
    if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.len() >= 2 && &bytes[..2] == b"BM" {
        return Some("image/bmp");
    }
    if bytes.len() >= 4
        && (bytes[..4] == [b'I', b'I', 0x2A, 0x00] || bytes[..4] == [b'M', b'M', 0x00, 0x2A])
    {
        return Some("image/tiff");
    }
    None
}

/// Formats a metadata note listing the local file paths of dragged/picked images so the agent
/// can inspect metadata (EXIF, dimensions) or run local image processing tools on them.
pub fn format_image_paths_note(
    images: &[ImageAttachment],
    project: Option<&Project>,
) -> Option<String> {
    let mut lines = Vec::new();
    for img in images {
        let Some(raw_path) = img.source_path.as_deref().map(str::trim).filter(|s| !s.is_empty())
        else {
            continue;
        };
        let display_path = match project {
            Some(proj) if proj.isolation == Isolation::LinuxVm => {
                let main = std::fs::canonicalize(proj.main())
                    .unwrap_or_else(|_| proj.main().to_path_buf());
                let p = Path::new(raw_path);
                if let Ok(rel) = p.strip_prefix(&main) {
                    format!("/workspace/{}", rel.display())
                } else {
                    format!("{raw_path} (host path, outside Linux VM)")
                }
            }
            _ => raw_path.to_string(),
        };
        lines.push(format!("- {}: {}", img.name, display_path));
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!("{IMAGE_PATHS_HEADER}\n{}", lines.join("\n")))
}

/// Compact human-readable byte size (e.g. `"182 KB"`, `"2.4 MB"`).
pub fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINY_PNG: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D',
        b'R',
    ];

    #[test]
    fn encodes_and_roundtrips_data_url() {
        let att = ImageAttachment::from_bytes("shot.png".into(), "image/png", TINY_PNG).unwrap();
        assert_eq!(att.mime_type, "image/png");
        assert_eq!(att.size_bytes, TINY_PNG.len());
        assert_eq!(att.decode_bytes().unwrap(), TINY_PNG);

        let url = att.data_url();
        assert!(url.starts_with("data:image/png;base64,"));
        let parsed = ImageAttachment::from_data_url("replayed.png".into(), &url).unwrap();
        assert_eq!(parsed.mime_type, "image/png");
        assert_eq!(parsed.data_base64, att.data_base64);
        assert_eq!(parsed.size_bytes, TINY_PNG.len());
    }

    #[test]
    fn detects_magic_bytes_and_extensions() {
        assert_eq!(detect_mime_from_bytes(TINY_PNG), Some("image/png"));
        assert_eq!(detect_mime_from_bytes(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(detect_mime_from_bytes(b"RIFF\x00\x00\x00\x00WEBP"), Some("image/webp"));
        assert_eq!(detect_mime_from_bytes(b"GIF89a"), Some("image/gif"));
        assert!(is_supported_image_path(Path::new("/tmp/SCR-2026.PNG")));
        assert!(is_supported_image_path(Path::new("/tmp/photo.jpeg")));
        assert!(!is_supported_image_path(Path::new("/tmp/readme.md")));
    }

    #[test]
    fn rejects_empty_or_oversized_or_unsupported() {
        assert!(ImageAttachment::from_bytes("empty.png".into(), "image/png", &[]).is_err());
        assert!(ImageAttachment::from_bytes("doc.pdf".into(), "application/pdf", b"%PDF").is_err());
    }

    #[test]
    fn formats_image_paths_note_for_host_and_vm() {
        let mut att = ImageAttachment::from_bytes("shot.png".into(), "image/png", TINY_PNG).unwrap();
        assert_eq!(format_image_paths_note(std::slice::from_ref(&att), None), None);

        att.source_path = Some("/tmp/proj/assets/shot.png".into());
        let note = format_image_paths_note(std::slice::from_ref(&att), None).unwrap();
        assert!(note.starts_with(IMAGE_PATHS_HEADER));
        assert!(note.contains("- shot.png: /tmp/proj/assets/shot.png"));

        let mut vm_proj = Project::new("/tmp/proj".into());
        vm_proj.isolation = Isolation::LinuxVm;
        let vm_note = format_image_paths_note(std::slice::from_ref(&att), Some(&vm_proj)).unwrap();
        assert!(vm_note.contains("- shot.png: /workspace/assets/shot.png"));
    }
}
