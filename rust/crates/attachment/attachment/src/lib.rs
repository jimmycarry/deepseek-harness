//! Durable attachment storage seam (`ctx.attachments`).
//!
//! `save_image` validates and durably commits a provider-independent image,
//! then returns a serializable `ImageAttachmentRef`. Consumers never persist
//! browser paths, object URLs, provider URLs, or base64 in session events.

use dsh_brand::Branded;
use dsh_cordis::Service;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Brand token for a content-addressed attachment id.
pub struct AttachmentIdBrand;
/// Opaque storage identifier; never a filesystem path or bearer URL.
pub type AttachmentId = Branded<AttachmentIdBrand>;

/// Brand an attachment id.
pub fn attachment_id(value: impl Into<String>) -> AttachmentId {
    AttachmentId::new(value)
}

/// Raster image formats accepted by the version-one attachment path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageMediaType {
    /// PNG.
    #[serde(rename = "image/png")]
    Png,
    /// JPEG.
    #[serde(rename = "image/jpeg")]
    Jpeg,
    /// WebP.
    #[serde(rename = "image/webp")]
    Webp,
    /// GIF.
    #[serde(rename = "image/gif")]
    Gif,
}

impl ImageMediaType {
    /// Parse a declared media type string.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "image/png" => Some(Self::Png),
            "image/jpeg" => Some(Self::Jpeg),
            "image/webp" => Some(Self::Webp),
            "image/gif" => Some(Self::Gif),
            _ => None,
        }
    }

    /// Wire media type.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
            Self::Gif => "image/gif",
        }
    }
}

/// Durable, serializable reference to one immutable image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageAttachmentRef {
    /// Content-addressed id (`sha256:` + hex).
    #[serde(rename = "attachmentId")]
    pub attachment_id: String,
    /// Media type verified from the stored bytes.
    #[serde(rename = "mediaType")]
    pub media_type: ImageMediaType,
    /// Exact encoded byte length.
    pub bytes: usize,
    /// Intrinsic encoded width in pixels.
    pub width: u32,
    /// Intrinsic encoded height in pixels.
    pub height: u32,
    /// Optional display name stripped of local path information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Deployment-resolved limits used by upload admission.
#[derive(Debug, Clone)]
pub struct ImageAttachmentLimits {
    /// Maximum encoded bytes for one submitted image.
    pub max_image_bytes: usize,
    /// Maximum images in one prompt.
    pub max_images_per_message: usize,
    /// Maximum aggregate image bytes in one prompt.
    pub max_message_image_bytes: usize,
    /// Maximum intrinsic pixels for one submitted image.
    pub max_image_pixels: u64,
    /// Maximum intrinsic width and height in pixels.
    pub max_image_dimension: u32,
    /// Accepted media types.
    pub media_types: Vec<ImageMediaType>,
}

/// Request to validate and durably commit one image.
#[derive(Debug, Clone)]
pub struct SaveImageAttachment {
    /// Encoded bytes.
    pub data: Vec<u8>,
    /// Caller-declared media type, checked against the bytes.
    pub media_type: ImageMediaType,
    /// Optional display name; never interpreted as a path.
    pub name: Option<String>,
}

/// Stored image bytes returned after reference and digest verification.
#[derive(Debug, Clone)]
pub struct StoredImageAttachment {
    /// Recorded reference.
    pub r#ref: ImageAttachmentRef,
    /// Verified bytes.
    pub data: Vec<u8>,
}

/// Typed attachment failures.
#[derive(Debug, Error)]
pub enum AttachmentError {
    /// Empty, mismatched, or undecodable image.
    #[error("{message}")]
    Invalid {
        /// Human message.
        message: String,
        /// Closed taxonomy code.
        code: &'static str,
    },
}

impl AttachmentError {
    /// Construct a typed failure.
    pub fn new(message: impl Into<String>, code: &'static str) -> Self {
        Self::Invalid {
            message: message.into(),
            code,
        }
    }

    /// Machine-routing code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid { code, .. } => code,
        }
    }
}

/// `ctx.attachments`.
pub struct AttachmentStore {
    inner: Box<dyn AttachmentBackend>,
    /// Deployment-resolved image policy.
    pub image_limits: ImageAttachmentLimits,
}

/// Persist and read images.
pub trait AttachmentBackend: Send + Sync {
    /// Validate one image without persisting it.
    fn validate_image(&self, input: &SaveImageAttachment) -> Result<(), AttachmentError>;
    /// Validate and durably commit one image.
    fn save_image(&self, input: SaveImageAttachment) -> Result<ImageAttachmentRef, AttachmentError>;
    /// Read and verify one content-addressed image.
    fn read_image(&self, r#ref: &ImageAttachmentRef) -> Result<StoredImageAttachment, AttachmentError>;
}

impl AttachmentStore {
    /// Wrap a backend and its admission limits.
    pub fn new(inner: Box<dyn AttachmentBackend>, image_limits: ImageAttachmentLimits) -> Self {
        Self {
            inner,
            image_limits,
        }
    }

    /// Validate one image without persisting it.
    ///
    /// @param input - encoded bytes, declared media type, and optional name.
    pub fn validate_image(&self, input: &SaveImageAttachment) -> Result<(), AttachmentError> {
        if !self
            .image_limits
            .media_types
            .contains(&input.media_type)
        {
            return Err(AttachmentError::new(
                format!(
                    "Image type {} is not accepted by this deployment.",
                    input.media_type.as_str()
                ),
                "UNSUPPORTED_IMAGE_TYPE",
            ));
        }
        self.inner.validate_image(input)
    }

    /// Validate one ordered image batch before committing any member.
    ///
    /// @param inputs - encoded images in their owning message order.
    pub fn validate_image_batch(
        &self,
        inputs: &[SaveImageAttachment],
    ) -> Result<(), AttachmentError> {
        if inputs.len() > self.image_limits.max_images_per_message {
            return Err(AttachmentError::new(
                "Image batch exceeds the configured image-count limit.",
                "TOO_MANY_IMAGES",
            ));
        }
        let total: usize = inputs.iter().map(|input| input.data.len()).sum();
        if total > self.image_limits.max_message_image_bytes {
            return Err(AttachmentError::new(
                "Image batch exceeds the configured aggregate image-byte limit.",
                "IMAGES_TOO_LARGE",
            ));
        }
        for input in inputs {
            self.validate_image(input)?;
        }
        Ok(())
    }

    /// Validate and durably commit one image.
    ///
    /// @param input - encoded bytes, declared media type, and optional name.
    /// @returns the durable content-addressed image reference.
    pub fn save_image(
        &self,
        input: SaveImageAttachment,
    ) -> Result<ImageAttachmentRef, AttachmentError> {
        self.validate_image(&input)?;
        self.inner.save_image(input)
    }

    /// Validate and durably commit one ordered image batch.
    ///
    /// @param inputs - encoded images in owning-message order.
    /// @returns durable references in the same order after every member succeeds.
    pub fn save_images(
        &self,
        inputs: Vec<SaveImageAttachment>,
    ) -> Result<Vec<ImageAttachmentRef>, AttachmentError> {
        self.validate_image_batch(&inputs)?;
        inputs
            .into_iter()
            .map(|input| self.inner.save_image(input))
            .collect()
    }

    /// Read one image and verify that bytes still match the recorded reference.
    ///
    /// @param r#ref - durable reference from the session log.
    /// @returns the verified bytes and attachment reference.
    pub fn read_image(
        &self,
        r#ref: &ImageAttachmentRef,
    ) -> Result<StoredImageAttachment, AttachmentError> {
        self.inner.read_image(r#ref)
    }
}

impl Service for AttachmentStore {
    const KEY: &'static str = "attachments";
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_cordis::Context;
    use std::sync::Arc;

    struct Reject;

    impl AttachmentBackend for Reject {
        fn validate_image(&self, _: &SaveImageAttachment) -> Result<(), AttachmentError> {
            Ok(())
        }
        fn save_image(&self, _: SaveImageAttachment) -> Result<ImageAttachmentRef, AttachmentError> {
            Err(AttachmentError::new("no", "INVALID_IMAGE"))
        }
        fn read_image(
            &self,
            _: &ImageAttachmentRef,
        ) -> Result<StoredImageAttachment, AttachmentError> {
            Err(AttachmentError::new("missing", "ATTACHMENT_NOT_FOUND"))
        }
    }

    #[test]
    fn seam_key_is_stable() {
        let ctx = Context::new();
        ctx.provide(Arc::new(AttachmentStore::new(
            Box::new(Reject),
            ImageAttachmentLimits {
                max_image_bytes: 1,
                max_images_per_message: 1,
                max_message_image_bytes: 1,
                max_image_pixels: 1,
                max_image_dimension: 1,
                media_types: vec![ImageMediaType::Png],
            },
        )))
        .unwrap();
        assert!(ctx.has_service("attachments"));
        ctx.dispose();
        assert!(!ctx.has_service("attachments"));
    }
}
