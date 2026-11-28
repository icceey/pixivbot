use super::s3_multipart::{
    abort_upload_state as abort_multipart_upload_state, retry_put_object, sha256_hex,
    upload_multipart, CapabilityState, CompletionEvidence, CreateExtension, HeadRecovery,
    MultipartCapability, MultipartOutcome, MultipartUploadRequest, ProviderKind, MAX_PART_SIZE,
    MIN_PART_SIZE,
};
use super::{
    detect_content_type, image_object_key, multipart_uploader_identity, required_config,
    validate_http_url, ImageUploadInput, ImageUploader,
};
use crate::error::{Error, Result, UploadStateError};
use async_trait::async_trait;
use s3::creds::Credentials;
use s3::{Bucket, Region};
use serde::Deserialize;
use std::num::NonZeroU64;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct S3UploaderConfig {
    #[serde(default)]
    pub endpoint_url: Option<String>,
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub access_key_id: Option<String>,
    #[serde(default)]
    pub secret_access_key: Option<String>,
    #[serde(default)]
    pub public_base_url: Option<String>,
    #[serde(default)]
    pub key_prefix: String,
    #[serde(default = "default_s3_path_style")]
    pub path_style: bool,
    #[serde(default = "default_s3_request_timeout_sec")]
    pub request_timeout_sec: NonZeroU64,
    #[serde(default = "default_multipart_image_threshold_mb")]
    pub multipart_image_threshold_mb: u64,
    #[serde(
        default = "default_multipart_part_size_mb",
        deserialize_with = "deserialize_multipart_part_size_mb"
    )]
    pub multipart_part_size_mb: u64,
}

pub(super) fn default_s3_path_style() -> bool {
    true
}

pub(super) fn default_s3_request_timeout_sec() -> NonZeroU64 {
    NonZeroU64::new(300).unwrap()
}

pub(super) fn default_multipart_image_threshold_mb() -> u64 {
    8
}

pub(super) fn default_multipart_part_size_mb() -> u64 {
    8
}

pub(super) fn multipart_part_size_bytes(size_mb: u64) -> Result<u64> {
    size_mb
        .checked_mul(1024 * 1024)
        .filter(|size| (MIN_PART_SIZE..=MAX_PART_SIZE).contains(size))
        .ok_or_else(|| Error::Other("multipart_part_size_mb must be between 5 and 5120 MiB".into()))
}

pub(super) fn deserialize_multipart_part_size_mb<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u64, D::Error> {
    let size_mb = u64::deserialize(deserializer)?;
    multipart_part_size_bytes(size_mb).map_err(serde::de::Error::custom)?;
    Ok(size_mb)
}

pub(super) fn image_uses_multipart(byte_len: usize, threshold_mb: u64) -> bool {
    threshold_mb != 0
        && u64::try_from(byte_len).unwrap_or(u64::MAX) >= threshold_mb.saturating_mul(1024 * 1024)
}

impl Default for S3UploaderConfig {
    fn default() -> Self {
        Self {
            endpoint_url: None,
            bucket: None,
            region: None,
            access_key_id: None,
            secret_access_key: None,
            public_base_url: None,
            key_prefix: String::new(),
            path_style: default_s3_path_style(),
            request_timeout_sec: default_s3_request_timeout_sec(),
            multipart_image_threshold_mb: default_multipart_image_threshold_mb(),
            multipart_part_size_mb: default_multipart_part_size_mb(),
        }
    }
}

impl S3UploaderConfig {
    fn required(&self) -> Result<ResolvedS3UploaderConfig> {
        Ok(ResolvedS3UploaderConfig {
            endpoint_url: validate_http_url(
                "image_upload.s3.endpoint_url",
                &required_config("image_upload.s3.endpoint_url", &self.endpoint_url)?,
            )?,
            bucket: required_config("image_upload.s3.bucket", &self.bucket)?,
            region: required_config("image_upload.s3.region", &self.region)?,
            access_key_id: required_config("image_upload.s3.access_key_id", &self.access_key_id)?,
            secret_access_key: required_config(
                "image_upload.s3.secret_access_key",
                &self.secret_access_key,
            )?,
            public_base_url: validate_http_url(
                "image_upload.s3.public_base_url",
                &required_config("image_upload.s3.public_base_url", &self.public_base_url)?,
            )?
            .trim_end_matches('/')
            .to_string(),
            key_prefix: self.key_prefix.trim_matches('/').to_string(),
            path_style: self.path_style,
            request_timeout: Duration::from_secs(self.request_timeout_sec.get()),
            multipart_image_threshold_mb: self.multipart_image_threshold_mb,
            multipart_part_size: multipart_part_size_bytes(self.multipart_part_size_mb)?,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename = "CompleteMultipartUploadResult")]
pub(super) struct CompleteMultipartUploadResult {
    // Deserialize Location to preserve duplicate-element validation.
    #[serde(rename = "Location", default)]
    _location: Option<String>,
    #[serde(rename = "Bucket")]
    pub(super) bucket: String,
    #[serde(rename = "Key")]
    pub(super) key: String,
    #[serde(rename = "ETag")]
    pub(super) etag: String,
}

fn validate_complete_multipart_upload_result_xml(body: &[u8]) -> Result<()> {
    const ROOT_ELEMENT: &str = "CompleteMultipartUploadResult";

    let mut reader = quick_xml::Reader::from_reader(body);
    let mut buffer = Vec::new();
    let root_is_empty = loop {
        buffer.clear();
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(element)) => {
                if element.name().as_ref() != ROOT_ELEMENT {
                    return Err(Error::Other(
                        "invalid CompleteMultipartUploadResult XML: expected CompleteMultipartUploadResult root element".into(),
                    ));
                }
                break false;
            }
            Ok(quick_xml::events::Event::Empty(element)) => {
                if element.name().as_ref() != ROOT_ELEMENT {
                    return Err(Error::Other(
                        "invalid CompleteMultipartUploadResult XML: expected CompleteMultipartUploadResult root element".into(),
                    ));
                }
                break true;
            }
            Ok(
                quick_xml::events::Event::Decl(_)
                | quick_xml::events::Event::Comment(_)
                | quick_xml::events::Event::PI(_)
                | quick_xml::events::Event::DocType(_),
            ) => {}
            Ok(quick_xml::events::Event::Text(text))
                if text.as_bytes().iter().all(u8::is_ascii_whitespace) => {}
            Ok(quick_xml::events::Event::Eof) | Ok(_) => {
                return Err(Error::Other(
                    "invalid CompleteMultipartUploadResult XML: expected CompleteMultipartUploadResult root element".into(),
                ));
            }
            Err(error) => {
                return Err(Error::Other(format!(
                    "invalid CompleteMultipartUploadResult XML: {error}"
                )));
            }
        }
    };

    if !root_is_empty {
        buffer.clear();
        reader
            .read_to_end(quick_xml::name::QName(ROOT_ELEMENT))
            .map_err(|error| {
                Error::Other(format!(
                    "invalid CompleteMultipartUploadResult XML: {error}"
                ))
            })?;
    }

    loop {
        buffer.clear();
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Eof) => return Ok(()),
            Ok(quick_xml::events::Event::Comment(_) | quick_xml::events::Event::PI(_)) => {}
            Ok(quick_xml::events::Event::Text(text))
                if text.as_bytes().iter().all(u8::is_ascii_whitespace) => {}
            Ok(_) => {
                return Err(Error::Other(
                    "invalid CompleteMultipartUploadResult XML: trailing XML content after root element"
                        .into(),
                ));
            }
            Err(error) => {
                return Err(Error::Other(format!(
                    "invalid CompleteMultipartUploadResult XML: {error}"
                )));
            }
        }
    }
}

pub(super) fn parse_complete_multipart_upload_result(
    body: &[u8],
) -> Result<CompleteMultipartUploadResult> {
    if body.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Err(Error::Other(
            "invalid CompleteMultipartUploadResult XML: empty response body".into(),
        ));
    }
    validate_complete_multipart_upload_result_xml(body)?;
    let result: CompleteMultipartUploadResult =
        quick_xml::de::from_reader(std::io::Cursor::new(body)).map_err(|error| {
            Error::Other(format!(
                "invalid CompleteMultipartUploadResult XML: {error}"
            ))
        })?;
    if [
        result.bucket.as_str(),
        result.key.as_str(),
        result.etag.as_str(),
    ]
    .into_iter()
    .any(|value| value.trim().is_empty())
    {
        return Err(Error::Other(
            "invalid CompleteMultipartUploadResult XML: Bucket, Key, and ETag must be non-empty"
                .into(),
        ));
    }
    Ok(result)
}

pub(super) fn validate_complete_multipart_target(
    result: &CompleteMultipartUploadResult,
    bucket: &str,
    object_key: &str,
) -> Result<()> {
    if result.bucket != bucket || result.key != object_key {
        return Err(Error::Other(
            "CompleteMultipartUploadResult did not match the requested bucket and object key"
                .into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct ResolvedS3UploaderConfig {
    endpoint_url: String,
    bucket: String,
    region: String,
    access_key_id: String,
    secret_access_key: String,
    public_base_url: String,
    key_prefix: String,
    path_style: bool,
    request_timeout: Duration,
    multipart_image_threshold_mb: u64,
    multipart_part_size: u64,
}

pub struct S3Uploader {
    bucket: Box<Bucket>,
    config: ResolvedS3UploaderConfig,
    multipart_http: reqwest::Client,
    standard_multipart: MultipartCapability,
    pub(super) uploader_identity_sha256: String,
}

impl S3Uploader {
    pub fn from_config(config: &S3UploaderConfig) -> Result<Self> {
        let config = config.required()?;
        let credentials = Credentials::new(
            Some(&config.access_key_id),
            Some(&config.secret_access_key),
            None,
            None,
            None,
        )
        .map_err(|e| Error::Other(format!("invalid S3 credentials: {e}")))?;
        let region = Region::Custom {
            region: config.region.clone(),
            endpoint: config.endpoint_url.clone(),
        };
        let mut bucket = Bucket::new(&config.bucket, region, credentials)
            .and_then(|bucket| bucket.with_request_timeout(config.request_timeout))
            .map_err(|e| Error::Other(format!("failed to build S3 bucket client: {e}")))?;
        if config.path_style {
            bucket = bucket.with_path_style();
        }
        let multipart_http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()?;
        let uploader_identity_sha256 = multipart_uploader_identity(
            ProviderKind::S3,
            &config.endpoint_url,
            &config.region,
            &config.bucket,
            config.path_style,
        )?;
        Ok(Self {
            bucket,
            config,
            multipart_http,
            standard_multipart: MultipartCapability::default(),
            uploader_identity_sha256,
        })
    }

    fn public_url(&self, key: &str) -> String {
        let base = self.config.public_base_url.trim_end_matches('/');
        let encoded_key = key
            .split('/')
            .map(urlencoding::encode)
            .collect::<Vec<_>>()
            .join("/");
        format!("{base}/{encoded_key}")
    }

    async fn upload_image_single_put(
        &self,
        key: &str,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<String> {
        let response = retry_put_object(|| {
            self.bucket
                .put_object_with_content_type(key, bytes, content_type)
        })
        .await
        .map_err(|e| Error::Other(format!("S3 put_object failed for key {key}: {e}")))?;
        let status = response.status_code();
        if !(200..300).contains(&status) {
            return Err(Error::Api {
                message: format!("S3 put_object returned {status} for key {key}"),
                status,
            });
        }
        Ok(self.public_url(key))
    }

    fn public_url_from_multipart_completion(
        &self,
        object_key: &str,
        evidence: &CompletionEvidence,
    ) -> Result<String> {
        match evidence {
            CompletionEvidence::Response(response) => {
                let result = parse_complete_multipart_upload_result(response.bytes())?;
                validate_complete_multipart_target(&result, &self.config.bucket, object_key)?;
            }
            CompletionEvidence::RecoveredHead { etag: None } => {}
            CompletionEvidence::RecoveredHead { etag: Some(_) } => {
                return Err(Error::Other(
                    "S3 multipart recovery returned unexpected ETag evidence".into(),
                ));
            }
        }
        Ok(self.public_url(object_key))
    }
}

#[async_trait]
impl ImageUploader for S3Uploader {
    async fn abort_upload_state(
        &self,
        uploads_dir: &std::path::Path,
    ) -> std::result::Result<(), UploadStateError> {
        abort_multipart_upload_state(
            self.bucket.as_ref(),
            ProviderKind::S3,
            &self.uploader_identity_sha256,
            uploads_dir,
        )
        .await
    }

    async fn upload_images(&self, images: &[ImageUploadInput<'_>]) -> Result<Vec<String>> {
        let mut urls = Vec::with_capacity(images.len());
        for (index, image) in images.iter().enumerate() {
            let key = image_object_key(&self.config.key_prefix, index + 1, image);
            let content_type = detect_content_type(image.bytes);
            if !image_uses_multipart(image.bytes.len(), self.config.multipart_image_threshold_mb)
                || self.standard_multipart.state() == CapabilityState::Unsupported
            {
                urls.push(
                    self.upload_image_single_put(&key, image.bytes, &content_type)
                        .await?,
                );
                continue;
            }

            let (manifest_path, logical_object_id) = match image.resume_context {
                Some(context) => (Some(context.manifest_path), context.logical_object_id),
                None => (None, key.as_str()),
            };
            match upload_multipart(
                self.bucket.as_ref(),
                &self.multipart_http,
                MultipartUploadRequest {
                    provider: ProviderKind::S3,
                    uploader_identity_sha256: &self.uploader_identity_sha256,
                    candidate_object_key: key.clone(),
                    logical_object_id,
                    reader: Box::new(std::io::Cursor::new(image.bytes)),
                    object_len: image.bytes.len() as u64,
                    object_sha256: sha256_hex(image.bytes),
                    part_size: self.config.multipart_part_size,
                    content_type: &content_type,
                    manifest_path,
                    create_extension: CreateExtension::None,
                    head_recovery: HeadRecovery::S3ByLength,
                },
            )
            .await?
            {
                MultipartOutcome::Unsupported { .. } => {
                    self.standard_multipart.mark_unsupported();
                    urls.push(
                        self.upload_image_single_put(&key, image.bytes, &content_type)
                            .await?,
                    );
                }
                MultipartOutcome::Completed(completed) => {
                    let url = self.public_url_from_multipart_completion(
                        &completed.object_key,
                        &completed.evidence,
                    )?;
                    self.standard_multipart.mark_supported();
                    completed.remove_manifest().await?;
                    urls.push(url);
                }
            }
        }
        Ok(urls)
    }
}
