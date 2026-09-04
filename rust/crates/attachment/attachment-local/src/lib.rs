//! Content-addressed local attachment backend rooted below `DSH_HOME`.
//!
//! Stored objects live at `$DSH_HOME/attachments/v1/objects/<aa>/<sha256>`.
//! Admission verifies magic bytes and header dimensions. `request_image`
//! decodes to 8-bit sRGB, downscales the long edge, and JPEG-encodes the
//! model-request projection.

use dsh_attachment::{
    AttachmentBackend, AttachmentError, AttachmentStore, ImageAttachmentLimits, ImageAttachmentRef,
    ImageMediaType, SaveImageAttachment, StoredImageAttachment,
};
use dsh_cordis::{Context, Result};
use dsh_home_paths::resolve_dsh_home;
use sha2::{Digest, Sha256};
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

/// Default maximum encoded bytes for one submitted image.
pub const DEFAULT_MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
/// Default maximum images in one prompt.
pub const DEFAULT_MAX_IMAGES_PER_MESSAGE: usize = 20;
/// Default maximum aggregate image bytes in one prompt.
pub const DEFAULT_MAX_MESSAGE_IMAGE_BYTES: usize = 200 * 1024 * 1024;
/// Default maximum intrinsic pixels for one submitted image.
pub const DEFAULT_MAX_IMAGE_PIXELS: u64 = 64_000_000;
/// Default per-side pixel cap for one submitted image.
pub const DEFAULT_MAX_IMAGE_DIMENSION: u32 = 8192;

const ID_PATTERN_PREFIX: &str = "sha256:";

/// Plugin construction inputs.
#[derive(Debug, Clone)]
pub struct Config {
    /// Explicit harness home; omitted follows `$DSH_HOME`, then `~/.dsh`.
    pub dsh_home: Option<String>,
    /// Maximum encoded bytes accepted for one submitted image.
    pub max_image_bytes: usize,
    /// Maximum image count accepted in one submitted message.
    pub max_images_per_message: usize,
    /// Maximum aggregate encoded image bytes accepted in one submitted message.
    pub max_message_image_bytes: usize,
    /// Maximum intrinsic width multiplied by height.
    pub max_image_pixels: u64,
    /// Maximum intrinsic width and height.
    pub max_image_dimension: u32,
    /// Max side length for a request-image projection.
    pub normalized_image_max_dimension: u32,
    /// Max encoded bytes for a request-image projection.
    pub normalized_image_max_bytes: usize,
}

impl Config {
    /// Resolve plugin config. Omitted numeric fields take the TypeScript defaults.
    pub fn resolve(value: Option<&serde_json::Value>) -> std::result::Result<Self, String> {
        fn u64_field(
            value: Option<&serde_json::Value>,
            key: &str,
            default: u64,
        ) -> std::result::Result<u64, String> {
            match value.and_then(|value| value.get(key)) {
                None => Ok(default),
                Some(raw) => raw
                    .as_u64()
                    .filter(|number| *number >= 1)
                    .ok_or_else(|| format!("attachment-local: {key} must be a positive integer")),
            }
        }
        Ok(Self {
            dsh_home: value
                .and_then(|value| value.get("dshHome"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            max_image_bytes: usize::try_from(u64_field(
                value,
                "maxImageBytes",
                DEFAULT_MAX_IMAGE_BYTES as u64,
            )?)
            .map_err(|_| "attachment-local: maxImageBytes is too large".to_string())?,
            max_images_per_message: usize::try_from(u64_field(
                value,
                "maxImagesPerMessage",
                DEFAULT_MAX_IMAGES_PER_MESSAGE as u64,
            )?)
            .map_err(|_| "attachment-local: maxImagesPerMessage is too large".to_string())?,
            max_message_image_bytes: usize::try_from(u64_field(
                value,
                "maxMessageImageBytes",
                DEFAULT_MAX_MESSAGE_IMAGE_BYTES as u64,
            )?)
            .map_err(|_| "attachment-local: maxMessageImageBytes is too large".to_string())?,
            max_image_pixels: u64_field(value, "maxImagePixels", DEFAULT_MAX_IMAGE_PIXELS)?,
            max_image_dimension: u32::try_from(u64_field(
                value,
                "maxImageDimension",
                u64::from(DEFAULT_MAX_IMAGE_DIMENSION),
            )?)
            .map_err(|_| "attachment-local: maxImageDimension is too large".to_string())?,
            normalized_image_max_dimension: u32::try_from(u64_field(
                value,
                "normalizedImageMaxDimension",
                u64::from(DEFAULT_NORMALIZED_IMAGE_MAX_DIMENSION),
            )?)
            .map_err(|_| {
                "attachment-local: normalizedImageMaxDimension is too large".to_string()
            })?,
            normalized_image_max_bytes: usize::try_from(u64_field(
                value,
                "normalizedImageMaxBytes",
                DEFAULT_NORMALIZED_IMAGE_MAX_BYTES as u64,
            )?)
            .map_err(|_| "attachment-local: normalizedImageMaxBytes is too large".to_string())?,
        })
    }
}

/// Header-derived image facts.
#[derive(Debug, Clone)]
pub struct DetectedImage {
    /// Verified media type.
    pub media_type: ImageMediaType,
    /// Intrinsic width.
    pub width: u32,
    /// Intrinsic height.
    pub height: u32,
}

/// Detect PNG / JPEG / GIF / WebP from magic bytes and parse dimensions.
pub fn detect_image(data: &[u8]) -> Result<DetectedImage, AttachmentError> {
    if data.is_empty() {
        return Err(AttachmentError::new("Image is empty.", "INVALID_IMAGE"));
    }
    if data.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        return png_size(data);
    }
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return jpeg_size(data);
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return gif_size(data);
    }
    if data.len() >= 16 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        return webp_size(data);
    }
    Err(AttachmentError::new(
        "Image type could not be detected from its bytes.",
        "INVALID_IMAGE",
    ))
}

fn png_size(data: &[u8]) -> Result<DetectedImage, AttachmentError> {
    if data.len() < 24 || &data[12..16] != b"IHDR" {
        return Err(AttachmentError::new(
            "PNG header is invalid.",
            "INVALID_IMAGE",
        ));
    }
    let width = u32::from_be_bytes(data[16..20].try_into().expect("png width"));
    let height = u32::from_be_bytes(data[20..24].try_into().expect("png height"));
    Ok(DetectedImage {
        media_type: ImageMediaType::Png,
        width,
        height,
    })
}

fn gif_size(data: &[u8]) -> Result<DetectedImage, AttachmentError> {
    if data.len() < 10 {
        return Err(AttachmentError::new(
            "GIF header is invalid.",
            "INVALID_IMAGE",
        ));
    }
    let width = u16::from_le_bytes([data[6], data[7]]) as u32;
    let height = u16::from_le_bytes([data[8], data[9]]) as u32;
    Ok(DetectedImage {
        media_type: ImageMediaType::Gif,
        width,
        height,
    })
}

fn jpeg_size(data: &[u8]) -> Result<DetectedImage, AttachmentError> {
    let mut i = 2usize;
    while i + 9 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        if i + 4 > data.len() {
            break;
        }
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if matches!(marker, 0xC0 | 0xC1 | 0xC2 | 0xC3) && i + 9 < data.len() {
            let height = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
            let width = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
            return Ok(DetectedImage {
                media_type: ImageMediaType::Jpeg,
                width,
                height,
            });
        }
        i += 2 + len;
    }
    Err(AttachmentError::new(
        "JPEG SOF marker is missing.",
        "INVALID_IMAGE",
    ))
}

fn webp_size(data: &[u8]) -> Result<DetectedImage, AttachmentError> {
    if data.len() < 30 {
        return Err(AttachmentError::new(
            "WebP header is invalid.",
            "INVALID_IMAGE",
        ));
    }
    let kind = &data[12..16];
    if kind == b"VP8X" && data.len() >= 30 {
        let width = 1 + u32::from_le_bytes([data[24], data[25], data[26], 0]);
        let height = 1 + u32::from_le_bytes([data[27], data[28], data[29], 0]);
        return Ok(DetectedImage {
            media_type: ImageMediaType::Webp,
            width,
            height,
        });
    }
    if kind == b"VP8 " && data.len() >= 30 {
        let width = u16::from_le_bytes([data[26], data[27]]) as u32 & 0x3fff;
        let height = u16::from_le_bytes([data[28], data[29]]) as u32 & 0x3fff;
        return Ok(DetectedImage {
            media_type: ImageMediaType::Webp,
            width,
            height,
        });
    }
    if kind == b"VP8L" && data.len() >= 25 {
        let bits = u32::from_le_bytes(data[21..25].try_into().expect("webp l"));
        let width = (bits & 0x3fff) + 1;
        let height = ((bits >> 14) & 0x3fff) + 1;
        return Ok(DetectedImage {
            media_type: ImageMediaType::Webp,
            width,
            height,
        });
    }
    Err(AttachmentError::new(
        "WebP bitstream is unsupported.",
        "INVALID_IMAGE",
    ))
}

fn display_name(value: Option<&str>) -> Option<String> {
    let value = value?;
    let slash = value.rfind('/').map(|i| i + 1).unwrap_or(0);
    let back = value.rfind('\\').map(|i| i + 1).unwrap_or(0);
    let leaf = &value[slash.max(back)..];
    let clean: String = leaf
        .chars()
        .filter(|ch| *ch >= ' ' && *ch != '\u{7f}')
        .take(255)
        .collect();
    let clean = clean.trim().to_string();
    if clean.is_empty() {
        None
    } else {
        Some(clean)
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn digest(data: &[u8]) -> String {
    hex_encode(&Sha256::digest(data))
}

fn object_path(root: &Path, sha256: &str) -> PathBuf {
    root.join("objects").join(&sha256[..2]).join(sha256)
}

fn ensure_reference(r#ref: &ImageAttachmentRef) -> Result<String, AttachmentError> {
    let id = r#ref.attachment_id.as_str();
    let Some(hex) = id.strip_prefix(ID_PATTERN_PREFIX) else {
        return Err(AttachmentError::new(
            "Attachment reference is invalid.",
            "INVALID_ATTACHMENT_REF",
        ));
    };
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(AttachmentError::new(
            "Attachment reference is invalid.",
            "INVALID_ATTACHMENT_REF",
        ));
    }
    Ok(hex.to_ascii_lowercase())
}

fn mkdir_private(path: &Path) -> Result<(), AttachmentError> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|error| AttachmentError::new(error.to_string(), "ATTACHMENT_WRITE_FAILED"))
}

/// Admit one image against the configured limits.
pub fn prepare_image(
    input: &SaveImageAttachment,
    limits: &ImageAttachmentLimits,
) -> Result<(Vec<u8>, ImageAttachmentRef), AttachmentError> {
    if input.data.len() > limits.max_image_bytes {
        return Err(AttachmentError::new(
            "Image exceeds the configured byte limit.",
            "IMAGE_TOO_LARGE",
        ));
    }
    let detected = detect_image(&input.data)?;
    if detected.media_type != input.media_type {
        return Err(AttachmentError::new(
            "Declared image type does not match its bytes.",
            "IMAGE_TYPE_MISMATCH",
        ));
    }
    if detected.width == 0
        || detected.height == 0
        || detected.width > limits.max_image_dimension
        || detected.height > limits.max_image_dimension
        || u64::from(detected.width) * u64::from(detected.height) > limits.max_image_pixels
    {
        return Err(AttachmentError::new(
            "Image exceeds the configured pixel limit.",
            "IMAGE_TOO_LARGE",
        ));
    }
    let sha256 = digest(&input.data);
    Ok((
        input.data.clone(),
        ImageAttachmentRef {
            attachment_id: format!("sha256:{sha256}"),
            media_type: detected.media_type,
            bytes: input.data.len(),
            width: detected.width,
            height: detected.height,
            name: display_name(input.name.as_deref()),
        },
    ))
}

/// Publish one already verified image below a versioned attachment root.
pub fn commit_prepared(
    root: &Path,
    data: &[u8],
    prepared: ImageAttachmentRef,
) -> Result<ImageAttachmentRef, AttachmentError> {
    let sha256 = ensure_reference(&prepared)?;
    if digest(data) != sha256 || data.len() != prepared.bytes {
        return Err(AttachmentError::new(
            "Prepared attachment bytes do not match their reference.",
            "ATTACHMENT_CORRUPT",
        ));
    }
    let bucket = root.join("objects").join(&sha256[..2]);
    let staging = root.join("tmp");
    mkdir_private(&bucket)?;
    mkdir_private(&staging)?;
    let temporary = staging.join(Uuid::new_v4().to_string());
    let target = object_path(root, &sha256);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let write = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        Ok::<(), std::io::Error>(())
    })();
    if let Err(error) = write {
        let _ = std::fs::remove_file(&temporary);
        return Err(AttachmentError::new(
            error.to_string(),
            "ATTACHMENT_WRITE_FAILED",
        ));
    }
    match std::fs::hard_link(&temporary, &target) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut existing = Vec::new();
            File::open(&target)
                .and_then(|mut file| file.read_to_end(&mut existing))
                .map_err(|io| AttachmentError::new(io.to_string(), "ATTACHMENT_READ_FAILED"))?;
            if digest(&existing) != sha256 {
                let _ = std::fs::remove_file(&temporary);
                return Err(AttachmentError::new(
                    "Stored attachment failed integrity verification.",
                    "ATTACHMENT_CORRUPT",
                ));
            }
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            return Err(AttachmentError::new(
                error.to_string(),
                "ATTACHMENT_WRITE_FAILED",
            ));
        }
    }
    let _ = std::fs::remove_file(&temporary);
    Ok(prepared)
}

/// Read and verify one content-addressed image.
pub fn read_image_file(
    root: &Path,
    r#ref: &ImageAttachmentRef,
) -> Result<StoredImageAttachment, AttachmentError> {
    let sha256 = ensure_reference(r#ref)?;
    let path = object_path(root, &sha256);
    let mut data = Vec::new();
    match File::open(&path).and_then(|mut file| file.read_to_end(&mut data)) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(AttachmentError::new(
                "Attachment object is missing.",
                "ATTACHMENT_NOT_FOUND",
            ));
        }
        Err(error) => {
            return Err(AttachmentError::new(
                error.to_string(),
                "ATTACHMENT_READ_FAILED",
            ));
        }
    }
    if digest(&data) != sha256 {
        return Err(AttachmentError::new(
            "Stored attachment failed integrity verification.",
            "ATTACHMENT_CORRUPT",
        ));
    }
    let metadata = detect_image(&data)?;
    if metadata.media_type != r#ref.media_type
        || data.len() != r#ref.bytes
        || metadata.width != r#ref.width
        || metadata.height != r#ref.height
    {
        return Err(AttachmentError::new(
            "Stored attachment metadata does not match its reference.",
            "ATTACHMENT_CORRUPT",
        ));
    }
    Ok(StoredImageAttachment {
        r#ref: r#ref.clone(),
        data,
    })
}

/// Persistent content-addressed local attachment store.
pub struct LocalAttachmentStore {
    root: PathBuf,
    limits: ImageAttachmentLimits,
}

impl LocalAttachmentStore {
    /// Persist under `root` (`DSH_HOME/attachments/v1`).
    pub fn new(root: impl Into<PathBuf>, limits: ImageAttachmentLimits) -> Self {
        Self {
            root: root.into(),
            limits,
        }
    }
}

impl AttachmentBackend for LocalAttachmentStore {
    fn validate_image(&self, input: &SaveImageAttachment) -> Result<(), AttachmentError> {
        prepare_image(input, &self.limits).map(|_| ())
    }

    fn save_image(
        &self,
        input: SaveImageAttachment,
    ) -> Result<ImageAttachmentRef, AttachmentError> {
        let (data, prepared) = prepare_image(&input, &self.limits)?;
        commit_prepared(&self.root, &data, prepared)
    }

    fn read_image(
        &self,
        r#ref: &ImageAttachmentRef,
    ) -> Result<StoredImageAttachment, AttachmentError> {
        read_image_file(&self.root, r#ref)
    }
}

fn limits_from(config: &Config) -> ImageAttachmentLimits {
    ImageAttachmentLimits {
        max_image_bytes: config.max_image_bytes,
        max_images_per_message: config.max_images_per_message,
        max_message_image_bytes: config.max_message_image_bytes,
        max_image_pixels: config.max_image_pixels,
        max_image_dimension: config.max_image_dimension,
        media_types: vec![
            ImageMediaType::Png,
            ImageMediaType::Jpeg,
            ImageMediaType::Webp,
            ImageMediaType::Gif,
        ],
    }
}

/// Default max side length for a request-image projection.
pub const DEFAULT_NORMALIZED_IMAGE_MAX_DIMENSION: u32 = 2048;
/// Default max encoded bytes for a request-image projection.
pub const DEFAULT_NORMALIZED_IMAGE_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Decode, convert to sRGB RGBA, downscale, and JPEG-encode a request image.
///
/// Qualities try 85 then 80 when the first encoding exceeds
/// [`DEFAULT_NORMALIZED_IMAGE_MAX_BYTES`].
///
/// # Errors
/// Decode failure or an encoding that still exceeds the byte cap.
pub fn request_image(data: &[u8]) -> Result<Vec<u8>, AttachmentError> {
    request_image_with_limits(
        data,
        DEFAULT_NORMALIZED_IMAGE_MAX_DIMENSION,
        DEFAULT_NORMALIZED_IMAGE_MAX_BYTES,
    )
}

/// Same as [`request_image`] with explicit caps.
///
/// # Errors
/// Decode failure or an encoding that still exceeds `max_bytes`.
pub fn request_image_with_limits(
    data: &[u8],
    max_dimension: u32,
    max_bytes: usize,
) -> Result<Vec<u8>, AttachmentError> {
    let decoded = image::load_from_memory(data).map_err(|error| {
        AttachmentError::new(
            format!("Image could not be decoded: {error}"),
            "INVALID_IMAGE",
        )
    })?;
    let mut rgba = decoded.to_rgba8();
    let (width, height) = rgba.dimensions();
    let longest = width.max(height);
    if longest > max_dimension {
        let scale = f64::from(max_dimension) / f64::from(longest);
        let next_w = ((f64::from(width) * scale).round() as u32).max(1);
        let next_h = ((f64::from(height) * scale).round() as u32).max(1);
        rgba =
            image::imageops::resize(&rgba, next_w, next_h, image::imageops::FilterType::Triangle);
    }
    let rgb = image::DynamicImage::ImageRgba8(rgba).to_rgb8();
    for quality in [85_u8, 80] {
        let mut encoded = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, quality);
        if encoder
            .encode(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                image::ExtendedColorType::Rgb8,
            )
            .is_ok()
            && encoded.len() <= max_bytes
        {
            return Ok(encoded);
        }
    }
    Err(AttachmentError::new(
        "Normalized request image exceeds the encoded byte cap.",
        "IMAGE_TOO_LARGE",
    ))
}

/// Provide `ctx.attachments` under `$DSH_HOME/attachments/v1`.
pub fn install(ctx: &Context, config: Config) -> Result<Arc<AttachmentStore>> {
    let home = resolve_dsh_home(config.dsh_home.as_deref());
    let root = home.join("attachments").join("v1");
    mkdir_private(&root).map_err(|error| dsh_cordis::CordisError::plugin(error.to_string()))?;
    let limits = limits_from(&config);
    let store = Arc::new(AttachmentStore::new(
        Box::new(LocalAttachmentStore::new(root, limits.clone())),
        limits,
    ));
    ctx.provide(Arc::clone(&store))?;
    Ok(store)
}

/// 1×1 PNG used by tests and snapshots.
pub const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00,
    0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x05, 0xFE, 0xD4, 0xEF, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    fn tmp_home(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("dsh-attach-{name}-{nanos}"))
    }

    #[test]
    fn detect_tiny_png() {
        let detected = detect_image(TINY_PNG).unwrap();
        assert_eq!(detected.media_type, ImageMediaType::Png);
        assert_eq!((detected.width, detected.height), (1, 1));
    }

    #[test]
    fn save_then_reread_from_host() {
        let home = tmp_home("round");
        let ctx = Context::new();
        let config = Config {
            dsh_home: Some(home.display().to_string()),
            max_image_bytes: DEFAULT_MAX_IMAGE_BYTES,
            max_images_per_message: DEFAULT_MAX_IMAGES_PER_MESSAGE,
            max_message_image_bytes: DEFAULT_MAX_MESSAGE_IMAGE_BYTES,
            max_image_pixels: DEFAULT_MAX_IMAGE_PIXELS,
            max_image_dimension: DEFAULT_MAX_IMAGE_DIMENSION,
            normalized_image_max_dimension: DEFAULT_NORMALIZED_IMAGE_MAX_DIMENSION,
            normalized_image_max_bytes: DEFAULT_NORMALIZED_IMAGE_MAX_BYTES,
        };
        install(&ctx, config).unwrap();
        let store = ctx.service::<AttachmentStore>().unwrap();
        let saved = store
            .save_image(SaveImageAttachment {
                data: TINY_PNG.to_vec(),
                media_type: ImageMediaType::Png,
                name: Some("C:\\\\Users\\\\a\\\\dot.png".into()),
            })
            .unwrap();
        assert_eq!(saved.width, 1);
        assert_eq!(saved.name.as_deref(), Some("dot.png"));
        assert!(saved.attachment_id.starts_with("sha256:"));
        let loaded = store.read_image(&saved).unwrap();
        assert_eq!(loaded.data, TINY_PNG);
        let sha = saved.attachment_id.strip_prefix("sha256:").unwrap();
        let path = home
            .join("attachments")
            .join("v1")
            .join("objects")
            .join(&sha[..2])
            .join(sha);
        assert_eq!(std::fs::read(&path).unwrap(), TINY_PNG);
        ctx.dispose();
        let _ = std::fs::remove_dir_all(&home);
    }

    fn valid_png(width: u32, height: u32) -> Vec<u8> {
        let mut img = image::RgbImage::new(width, height);
        for pixel in img.pixels_mut() {
            *pixel = image::Rgb([10, 20, 30]);
        }
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        png
    }

    #[test]
    fn request_image_keeps_tiny_png_under_the_byte_cap() {
        let encoded = request_image(&valid_png(1, 1)).unwrap();
        assert!(!encoded.is_empty());
        assert!(encoded.len() <= DEFAULT_NORMALIZED_IMAGE_MAX_BYTES);
        let decoded = image::load_from_memory(&encoded).unwrap();
        assert_eq!(decoded.dimensions(), (1, 1));
    }

    #[test]
    fn request_image_downscales_long_edge() {
        let encoded = request_image(&valid_png(3000, 100)).unwrap();
        let decoded = image::load_from_memory(&encoded).unwrap();
        assert!(
            decoded.width().max(decoded.height()) <= DEFAULT_NORMALIZED_IMAGE_MAX_DIMENSION,
            "{:?}",
            decoded.dimensions()
        );
    }

    #[test]
    fn request_image_refuses_when_byte_cap_is_tiny() {
        let err = request_image_with_limits(&valid_png(1, 1), 2048, 1).unwrap_err();
        assert_eq!(err.code(), "IMAGE_TOO_LARGE");
    }

    #[test]
    fn type_mismatch_is_refused() {
        let err = detect_image(TINY_PNG).unwrap();
        assert_eq!(err.media_type, ImageMediaType::Png);
        let limits = limits_from(&Config::resolve(None).unwrap());
        let err = prepare_image(
            &SaveImageAttachment {
                data: TINY_PNG.to_vec(),
                media_type: ImageMediaType::Jpeg,
                name: None,
            },
            &limits,
        )
        .unwrap_err();
        assert_eq!(err.code(), "IMAGE_TYPE_MISMATCH");
    }
}
