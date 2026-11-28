pub mod catbox;
pub mod ipfs3;
pub mod pixi;
pub mod s3;
pub(crate) mod s3_multipart;

#[cfg(test)]
mod tests;

pub use catbox::{CatboxUploader, CatboxUploaderConfig};
pub use ipfs3::{IpfS3PreviewRewriteConfig, IpfS3Uploader, IpfS3UploaderConfig};
pub use pixi::PixiUploader;
pub use s3::{S3Uploader, S3UploaderConfig};

use crate::error::{Error, Result, UploadStateError};
use crate::telegraph::TelegraphImageUrlPair;
use async_trait::async_trait;
use s3_multipart::{uploader_identity_fingerprint, ProviderKind};
use serde::Deserialize;
use std::sync::Arc;

/// Detect content type from image magic bytes.
pub(super) fn detect_content_type(data: &[u8]) -> String {
    if data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF {
        "image/jpeg".into()
    } else if data.len() >= 8 && &data[0..8] == b"\x89PNG\r\n\x1a\n" {
        "image/png".into()
    } else if data.len() >= 6 && (&data[0..6] == b"GIF87a" || &data[0..6] == b"GIF89a") {
        "image/gif".into()
    } else if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        "image/webp".into()
    } else {
        "application/octet-stream".into()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImageUploadProvider {
    #[default]
    Pixi,
    S3,
    Catbox,
    IpfS3,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ImageUploadConfig {
    #[serde(default)]
    pub provider: ImageUploadProvider,
    #[serde(default)]
    pub s3: Option<S3UploaderConfig>,
    #[serde(default)]
    pub ipfs3: Option<IpfS3UploaderConfig>,
    #[serde(default)]
    pub catbox: CatboxUploaderConfig,
}

impl ImageUploadConfig {
    pub async fn build_uploader(&self) -> Result<Arc<dyn ImageUploader>> {
        match self.provider {
            ImageUploadProvider::Pixi => Ok(Arc::new(PixiUploader::new())),
            ImageUploadProvider::S3 => Ok(Arc::new(S3Uploader::from_config(
                self.s3.as_ref().ok_or_else(|| {
                    Error::Other("image_upload.s3 is required when provider=s3".into())
                })?,
            )?)),
            ImageUploadProvider::IpfS3 => Ok(Arc::new(IpfS3Uploader::from_config(
                self.ipfs3.as_ref().ok_or_else(|| {
                    Error::Other("image_upload.ipfs3 is required when provider=ipfs3".into())
                })?,
            )?)),
            ImageUploadProvider::Catbox => Ok(Arc::new(CatboxUploader::from_config(&self.catbox)?)),
        }
    }

    pub fn ipfs3_preview_rewrite_config(&self) -> Option<IpfS3PreviewRewriteConfig> {
        if self.provider != ImageUploadProvider::IpfS3 {
            return None;
        }
        let ipfs3 = self.ipfs3.as_ref()?;
        let resolved = ipfs3.required().ok()?;
        let preview = resolved.preview_gateway_url?;
        if preview == resolved.gateway_url {
            return None;
        }
        Some(IpfS3PreviewRewriteConfig {
            preview_gateway_url: preview,
            public_gateway_url: resolved.gateway_url,
            delay_sec: resolved.preview_rewrite_delay_sec,
        })
    }
}

pub(super) fn required_config(name: &str, value: &Option<String>) -> Result<String> {
    value
        .as_ref()
        .filter(|v| !v.trim().is_empty())
        .cloned()
        .ok_or_else(|| Error::Other(format!("{name} is required")))
}

pub(super) fn validate_http_url(name: &str, value: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(value)
        .map_err(|e| Error::Other(format!("{name} must be a valid URL: {e}")))?;
    match parsed.scheme() {
        "http" | "https" => {}
        scheme => Err(Error::Other(format!(
            "{name} must use http or https, got {scheme}"
        )))?,
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::Other(format!("{name} must not contain userinfo")));
    }
    if parsed.query().is_some() {
        return Err(Error::Other(format!("{name} must not contain query")));
    }
    if parsed.fragment().is_some() {
        return Err(Error::Other(format!("{name} must not contain fragment")));
    }
    Ok(value.to_string())
}

pub(super) fn normalized_multipart_endpoint(endpoint: &str) -> Result<String> {
    let endpoint = reqwest::Url::parse(endpoint)
        .map_err(|_| {
            Error::Other("multipart endpoint was invalid after configuration validation".into())
        })?
        .to_string();
    Ok(endpoint.trim_end_matches('/').to_string())
}

pub(super) fn multipart_uploader_identity(
    provider: ProviderKind,
    endpoint: &str,
    region: &str,
    bucket: &str,
    path_style: bool,
) -> Result<String> {
    Ok(uploader_identity_fingerprint(
        provider,
        &normalized_multipart_endpoint(endpoint)?,
        region,
        bucket,
        path_style,
    ))
}

#[derive(Debug, Clone, Copy)]
pub struct UploadResumeContext<'a> {
    pub manifest_path: &'a std::path::Path,
    pub logical_object_id: &'a str,
}

pub struct ImageUploadInput<'a> {
    pub filename: &'a str,
    pub bytes: &'a [u8],
    pub resume_context: Option<UploadResumeContext<'a>>,
}

pub struct ZipArchiveUploadInput<'a> {
    /// An immutable, seekable archive for the duration of this upload.
    pub path: &'a std::path::Path,
    pub entry_names: &'a [String],
    pub resume_context: Option<UploadResumeContext<'a>>,
}

#[async_trait]
pub trait ImageUploader: Send + Sync {
    fn supports_zip_archive_upload(&self) -> bool {
        false
    }

    async fn abort_upload_state(
        &self,
        _uploads_dir: &std::path::Path,
    ) -> std::result::Result<(), UploadStateError> {
        Ok(())
    }

    async fn upload_images(&self, images: &[ImageUploadInput<'_>]) -> Result<Vec<String>>;

    async fn upload_images_with_url_pairs(
        &self,
        images: &[ImageUploadInput<'_>],
    ) -> Result<Vec<TelegraphImageUrlPair>> {
        Ok(self
            .upload_images(images)
            .await?
            .into_iter()
            .map(|url| TelegraphImageUrlPair {
                preview_url: url.clone(),
                public_url: url,
                cid: None,
            })
            .collect())
    }

    async fn upload_zip_archive_with_url_pairs(
        &self,
        _archive: ZipArchiveUploadInput<'_>,
    ) -> Result<Option<Vec<TelegraphImageUrlPair>>> {
        Ok(None)
    }
}

pub(super) fn extension_for_content_type(content_type: &str) -> &'static str {
    match content_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

pub(super) fn extension_for_upload(filename: &str, bytes: &[u8]) -> &'static str {
    let detected = detect_content_type(bytes);
    if detected != "application/octet-stream" {
        return extension_for_content_type(&detected);
    }
    match filename
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "jpg",
        "png" => "png",
        "gif" => "gif",
        "webp" => "webp",
        _ => "bin",
    }
}

pub(super) fn safe_upload_filename(filename: &str, ext: &str) -> String {
    let name = filename.rsplit(['/', '\\']).next().unwrap_or("image");
    let stem = name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(name);
    let stem = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let stem = if stem.is_empty() { "image" } else { &stem };
    format!("{stem}.{ext}")
}

pub(super) fn short_hash_hex(bytes: &[u8]) -> String {
    let mut hash: u32 = 0x811c9dc5;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")
}

pub(super) fn image_object_key(prefix: &str, index: usize, input: &ImageUploadInput<'_>) -> String {
    let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
    let hash = short_hash_hex(input.bytes);
    let ext = extension_for_upload(input.filename, input.bytes);
    let filename = format!("{timestamp}-{index:04}-{hash}.{ext}");
    if prefix.is_empty() {
        filename
    } else {
        format!("{prefix}/{filename}")
    }
}
