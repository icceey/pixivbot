use crate::error::{Error, Result};
use async_trait::async_trait;
use s3::command::Command;
use s3::creds::Credentials;
use s3::request::{tokio_backend::ReqwestRequest, Request};
use s3::{Bucket, Region};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// A Telegraph content node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attrs: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelegraphImageUrlPair {
    pub preview_url: String,
    pub public_url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelegraphRewritePage {
    pub path: String,
    pub title: String,
    pub content: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelegraphRewriteData {
    pub pages: Vec<TelegraphRewritePage>,
    pub preview_gateway_url: String,
    pub public_gateway_url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TelegraphGalleryPageResult {
    pub first_page_url: String,
    pub rewrite_data: Option<TelegraphRewriteData>,
}

impl Node {
    pub fn img(src: &str) -> Self {
        let mut attrs = serde_json::Map::new();
        attrs.insert("src".into(), serde_json::json!(src));
        Self {
            tag: "img".into(),
            attrs: Some(attrs),
            children: None,
        }
    }

    pub fn link(href: &str, text: &str) -> Self {
        let mut attrs = serde_json::Map::new();
        attrs.insert("href".into(), serde_json::json!(href));
        Self {
            tag: "a".into(),
            attrs: Some(attrs),
            children: Some(vec![serde_json::json!(text)]),
        }
    }

    pub fn paragraph(text: &str) -> Self {
        Self {
            tag: "p".into(),
            attrs: None,
            children: Some(vec![serde_json::json!(text)]),
        }
    }
}

/// Estimate serialized content size in bytes.
pub fn estimate_content_size(nodes: &[Node]) -> usize {
    serde_json::to_vec(nodes).map(|v| v.len()).unwrap_or(0)
}

pub fn node_attr_str<'a>(node: &'a Node, key: &str) -> Option<&'a str> {
    node.attrs.as_ref()?.get(key)?.as_str()
}

fn set_node_attr_string(node: &mut Node, key: &str, value: String) {
    if let Some(attrs) = node.attrs.as_mut() {
        attrs.insert(key.to_string(), serde_json::Value::String(value));
    }
}

pub fn rewrite_ipfs_gateway_nodes(
    nodes: &[Node],
    preview_gateway_url: &str,
    public_gateway_url: &str,
) -> Vec<Node> {
    let preview_prefix = format!("{}/", preview_gateway_url.trim_end_matches('/'));
    let public_gateway = public_gateway_url.trim_end_matches('/');
    nodes
        .iter()
        .cloned()
        .map(|mut node| {
            if node.tag == "img" {
                if let Some(cid) = node_attr_str(&node, "src")
                    .and_then(|src| src.strip_prefix(&preview_prefix))
                    .map(str::to_string)
                {
                    set_node_attr_string(&mut node, "src", format!("{public_gateway}/{cid}"));
                }
            }
            node
        })
        .collect()
}

/// Maximum content size per Telegraph page (64KB minus overhead).
const MAX_PAGE_CONTENT_BYTES: usize = 60_000;

/// Budget reserved for a continuation link ("Next Page →") so that adding it
/// after splitting doesn't overflow `MAX_PAGE_CONTENT_BYTES`.
pub const CONTINUATION_LINK_BUDGET: usize = 512;

/// Split image URLs into chunks that fit within the content size limit.
/// The effective per-page limit is `max_bytes - CONTINUATION_LINK_BUDGET`
/// for non-last pages so that a continuation link can be added safely.
pub fn split_for_pages(urls: &[String], max_bytes: usize) -> Vec<Vec<String>> {
    if urls.is_empty() {
        return vec![];
    }
    let max_page_size = max_bytes.saturating_sub(CONTINUATION_LINK_BUDGET);
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut current_size = 0;
    for url in urls {
        let node = Node::img(url);
        let node_size = serde_json::to_vec(&node).map(|v| v.len()).unwrap_or(100);
        if current_size + node_size > max_page_size && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_size = 0;
        }
        current_size += node_size;
        current.push(url.clone());
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn split_url_pairs_for_pages(
    urls: &[TelegraphImageUrlPair],
    max_bytes: usize,
) -> Vec<Vec<TelegraphImageUrlPair>> {
    if urls.is_empty() {
        return vec![];
    }
    let max_page_size = max_bytes.saturating_sub(CONTINUATION_LINK_BUDGET);
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut current_size = 0;
    for pair in urls {
        let preview_size = serde_json::to_vec(&Node::img(&pair.preview_url))
            .map(|v| v.len())
            .unwrap_or(100);
        let public_size = serde_json::to_vec(&Node::img(&pair.public_url))
            .map(|v| v.len())
            .unwrap_or(100);
        let node_size = preview_size.max(public_size);
        if current_size + node_size > max_page_size && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_size = 0;
        }
        current_size += node_size;
        current.push(pair.clone());
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[derive(Debug, Deserialize)]
struct TelegraphResponse<T> {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    result: Option<T>,
}

#[derive(Debug, Deserialize, Default)]
struct PageResult {
    url: String,
}

#[derive(Debug, Deserialize, Default)]
struct AccountResult {
    access_token: String,
}

/// Response from pixi.mg upload API.
#[derive(Debug, Deserialize)]
struct PixiResponse {
    #[serde(default)]
    success: bool,
    /// Present in single-file upload response.
    #[serde(default)]
    direct_url: Option<String>,
    /// Present in multi-file upload response.
    #[serde(default)]
    images: Option<Vec<PixiImage>>,
}

#[derive(Debug, Deserialize)]
struct PixiImage {
    direct_url: String,
}

/// Detect content type from image magic bytes.
fn detect_content_type(data: &[u8]) -> String {
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

#[derive(Debug, Clone, Deserialize)]
pub struct CatboxUploaderConfig {
    #[serde(default = "default_catbox_api_url")]
    pub api_url: String,
    #[serde(default)]
    pub userhash: Option<String>,
}

impl Default for CatboxUploaderConfig {
    fn default() -> Self {
        Self {
            api_url: default_catbox_api_url(),
            userhash: None,
        }
    }
}

fn default_catbox_api_url() -> String {
    "https://catbox.moe/user/api.php".to_string()
}

#[derive(Debug, Clone, Deserialize, Default)]
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
}

fn default_s3_path_style() -> bool {
    true
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
        })
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct IpfS3UploaderConfig {
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
    pub gateway_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_gateway_url: Option<String>,
    #[serde(default = "default_ipfs_preview_rewrite_delay_sec")]
    pub preview_rewrite_delay_sec: u64,
    #[serde(default)]
    pub key_prefix: String,
    #[serde(default = "default_s3_path_style")]
    pub path_style: bool,
    #[serde(default)]
    pub warm_public_gateway_after_upload: bool,
    #[serde(default)]
    pub zip_extract_enabled: bool,
}

fn default_ipfs_preview_rewrite_delay_sec() -> u64 {
    600
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpfS3PreviewRewriteConfig {
    pub preview_gateway_url: String,
    pub public_gateway_url: String,
    pub delay_sec: u64,
}

impl IpfS3UploaderConfig {
    fn required(&self) -> Result<ResolvedIpfS3UploaderConfig> {
        Ok(ResolvedIpfS3UploaderConfig {
            endpoint_url: validate_http_url(
                "image_upload.ipfs3.endpoint_url",
                &required_config("image_upload.ipfs3.endpoint_url", &self.endpoint_url)?,
            )?,
            bucket: required_config("image_upload.ipfs3.bucket", &self.bucket)?,
            region: required_config("image_upload.ipfs3.region", &self.region)?,
            access_key_id: required_config(
                "image_upload.ipfs3.access_key_id",
                &self.access_key_id,
            )?,
            secret_access_key: required_config(
                "image_upload.ipfs3.secret_access_key",
                &self.secret_access_key,
            )?,
            gateway_url: validate_http_url(
                "image_upload.ipfs3.gateway_url",
                &required_config("image_upload.ipfs3.gateway_url", &self.gateway_url)?,
            )?
            .trim_end_matches('/')
            .to_string(),
            preview_gateway_url: self
                .preview_gateway_url
                .as_ref()
                .map(|url| validate_http_url("image_upload.ipfs3.preview_gateway_url", url))
                .transpose()?
                .map(|url| url.trim_end_matches('/').to_string()),
            preview_rewrite_delay_sec: self.preview_rewrite_delay_sec,
            key_prefix: self.key_prefix.trim_matches('/').to_string(),
            path_style: self.path_style,
            warm_public_gateway_after_upload: self.warm_public_gateway_after_upload,
            zip_extract_enabled: self.zip_extract_enabled,
        })
    }
}

#[derive(Debug, Clone)]
struct ResolvedIpfS3UploaderConfig {
    endpoint_url: String,
    bucket: String,
    region: String,
    access_key_id: String,
    secret_access_key: String,
    gateway_url: String,
    preview_gateway_url: Option<String>,
    preview_rewrite_delay_sec: u64,
    key_prefix: String,
    path_style: bool,
    warm_public_gateway_after_upload: bool,
    zip_extract_enabled: bool,
}

fn required_config(name: &str, value: &Option<String>) -> Result<String> {
    value
        .as_ref()
        .filter(|v| !v.trim().is_empty())
        .cloned()
        .ok_or_else(|| Error::Other(format!("{name} is required")))
}

fn validate_http_url(name: &str, value: &str) -> Result<String> {
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
}

pub struct ImageUploadInput<'a> {
    pub filename: &'a str,
    pub bytes: &'a [u8],
}

pub struct ZipArchiveUploadInput<'a> {
    pub filename: &'a str,
    pub bytes: &'a [u8],
    pub entry_names: &'a [String],
}

#[async_trait]
pub trait ImageUploader: Send + Sync {
    fn supports_zip_archive_upload(&self) -> bool {
        false
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

pub struct PixiUploader {
    http: reqwest::Client,
    upload_url: String,
}

impl PixiUploader {
    pub fn new() -> Self {
        Self::new_with_url("https://pixi.mg/api".to_string())
    }

    pub fn new_with_url(upload_url: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("failed to build pixi uploader http client"),
            upload_url,
        }
    }

    pub async fn upload_images_batch(&self, images: &[&[u8]]) -> Result<Vec<String>> {
        if images.is_empty() {
            return Ok(Vec::new());
        }
        if images.len() > 5 {
            return Err(Error::Other("pixi.mg max 5 files per upload".into()));
        }

        let mut form = reqwest::multipart::Form::new();
        for image_data in images {
            let content_type = detect_content_type(image_data);
            let ext = extension_for_content_type(&content_type);
            let part = reqwest::multipart::Part::bytes(image_data.to_vec())
                .file_name(format!("image.{ext}"))
                .mime_str(&content_type)
                .map_err(|e| Error::Other(format!("mime error: {e}")))?;
            form = form.part("files[]", part);
        }

        let resp = self
            .http
            .post(&self.upload_url)
            .multipart(form)
            .send()
            .await?;

        let status = resp.status();

        // Detect 429 rate limiting
        if status.as_u16() == 429 {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            return Err(Error::RateLimited {
                retry_after_secs: retry_after,
            });
        }

        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Api {
                message: format!("pixi.mg upload returned {}: {}", status, body),
                status: status.as_u16(),
            });
        }

        let result: PixiResponse = resp.json().await?;
        if result.success {
            // Multi-file response has `images` array; single-file has `direct_url`
            if let Some(images) = result.images {
                return Ok(images.into_iter().map(|i| i.direct_url).collect());
            }
            if let Some(url) = result.direct_url {
                return Ok(vec![url]);
            }
        }
        Err(Error::Parse("pixi.mg upload returned no urls".into()))
    }

    pub async fn upload_images_with_retry(
        &self,
        images: &[&[u8]],
        max_retries: u32,
    ) -> Result<Vec<String>> {
        let mut attempt = 0u32;
        loop {
            match self.upload_images_batch(images).await {
                Ok(urls) => return Ok(urls),
                Err(Error::RateLimited { retry_after_secs }) => {
                    if attempt >= max_retries {
                        return Err(Error::RateLimited { retry_after_secs });
                    }
                    let computed = 40 * 2u64.pow(attempt);
                    let wait = retry_after_secs.unwrap_or(0).max(computed);
                    tracing::warn!(
                        "pixi.mg returned 429, waiting {}s before retry (attempt {}/{})",
                        wait,
                        attempt + 1,
                        max_retries
                    );
                    tokio::time::sleep(tokio::time::Duration::from_secs(wait)).await;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Default for PixiUploader {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ImageUploader for PixiUploader {
    async fn upload_images(&self, images: &[ImageUploadInput<'_>]) -> Result<Vec<String>> {
        let mut all_urls = Vec::new();
        for chunk in images.chunks(5) {
            let refs: Vec<&[u8]> = chunk.iter().map(|i| i.bytes).collect();
            let urls = self.upload_images_with_retry(&refs, 3).await?;
            all_urls.extend(urls);
        }
        Ok(all_urls)
    }
}

pub struct CatboxUploader {
    http: reqwest::Client,
    api_url: String,
    userhash: Option<String>,
}

impl CatboxUploader {
    pub fn from_config(config: &CatboxUploaderConfig) -> Result<Self> {
        if config.api_url.trim().is_empty() {
            return Err(Error::Other(
                "image_upload.catbox.api_url is required".into(),
            ));
        }
        let api_url = validate_http_url("image_upload.catbox.api_url", &config.api_url)?;
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()?,
            api_url,
            userhash: config.userhash.clone().filter(|v| !v.is_empty()),
        })
    }
}

#[async_trait]
impl ImageUploader for CatboxUploader {
    async fn upload_images(&self, images: &[ImageUploadInput<'_>]) -> Result<Vec<String>> {
        let mut urls = Vec::with_capacity(images.len());
        for image in images {
            let content_type = detect_content_type(image.bytes);
            let ext = extension_for_upload(image.filename, image.bytes);
            let mut form = reqwest::multipart::Form::new().text("reqtype", "fileupload");
            if let Some(userhash) = &self.userhash {
                form = form.text("userhash", userhash.clone());
            }
            let part = reqwest::multipart::Part::bytes(image.bytes.to_vec())
                .file_name(safe_upload_filename(image.filename, ext))
                .mime_str(&content_type)
                .map_err(|e| Error::Other(format!("mime error: {e}")))?;
            form = form.part("fileToUpload", part);

            let resp = self.http.post(&self.api_url).multipart(form).send().await?;
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                return Err(Error::Api {
                    message: format!("catbox upload returned {}: {}", status, body),
                    status: status.as_u16(),
                });
            }
            let url = body.trim().to_string();
            validate_http_url("catbox upload response", &url).map_err(|_| {
                Error::Parse(format!("catbox upload returned non-url response: {url}"))
            })?;
            urls.push(url);
        }
        Ok(urls)
    }
}

pub struct S3Uploader {
    bucket: Box<Bucket>,
    config: ResolvedS3UploaderConfig,
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
            .map_err(|e| Error::Other(format!("failed to build S3 bucket client: {e}")))?;
        if config.path_style {
            bucket = bucket.with_path_style();
        }
        Ok(Self { bucket, config })
    }

    fn object_key(&self, index: usize, input: &ImageUploadInput<'_>) -> String {
        let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
        let hash = short_hash_hex(input.bytes);
        let ext = extension_for_upload(input.filename, input.bytes);
        let filename = format!("{timestamp}-{index:04}-{hash}.{ext}");
        if self.config.key_prefix.is_empty() {
            filename
        } else {
            format!("{}/{}", self.config.key_prefix, filename)
        }
    }

    fn public_url(&self, key: &str) -> String {
        public_url_for_key(&self.config.public_base_url, key)
    }
}

#[async_trait]
impl ImageUploader for S3Uploader {
    async fn upload_images(&self, images: &[ImageUploadInput<'_>]) -> Result<Vec<String>> {
        let mut urls = Vec::with_capacity(images.len());
        for (index, image) in images.iter().enumerate() {
            let key = self.object_key(index + 1, image);
            let content_type = detect_content_type(image.bytes);
            let response = self
                .bucket
                .put_object_with_content_type(&key, image.bytes, &content_type)
                .await
                .map_err(|e| Error::Other(format!("S3 put_object failed for key {key}: {e}")))?;
            let status = response.status_code();
            if !(200..300).contains(&status) {
                return Err(Error::Api {
                    message: format!("S3 put_object returned {status} for key {key}"),
                    status,
                });
            }
            urls.push(self.public_url(&key));
        }
        Ok(urls)
    }
}

pub struct IpfS3Uploader {
    bucket: Box<Bucket>,
    config: ResolvedIpfS3UploaderConfig,
    http: reqwest::Client,
}

const ZIP_CENTRAL_HEADER_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
const ZIP_CENTRAL_FIXED_HEADER_LEN: usize = 46;
const ZIP_CENTRAL_FLAGS_OFFSET: usize = 8;
const ZIP_CENTRAL_METHOD_OFFSET: usize = 10;
#[cfg(test)]
const ZIP_CENTRAL_CRC32_OFFSET: usize = 16;
const ZIP_CENTRAL_NAME_LEN_OFFSET: usize = 28;
const ZIP_CENTRAL_EXTRA_LEN_OFFSET: usize = 30;
const ZIP_CENTRAL_COMMENT_LEN_OFFSET: usize = 32;
const ZIP_LOCAL_HEADER_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];
const ZIP_LOCAL_FIXED_HEADER_LEN: usize = 30;
const ZIP_LOCAL_FLAGS_OFFSET: usize = 6;
const ZIP_LOCAL_METHOD_OFFSET: usize = 8;
const ZIP_LOCAL_CRC32_OFFSET: usize = 14;
const ZIP_LOCAL_COMPRESSED_SIZE_OFFSET: usize = 18;
const ZIP_LOCAL_UNCOMPRESSED_SIZE_OFFSET: usize = 22;
const ZIP_LOCAL_NAME_LEN_OFFSET: usize = 26;
const ZIP_LOCAL_EXTRA_LEN_OFFSET: usize = 28;
const ZIP_DATA_DESCRIPTOR_SIGNATURE: [u8; 4] = [0x50, 0x4b, 0x07, 0x08];
const ZIP_FLAG_ENCRYPTED: u16 = 1;
const ZIP_FLAG_DATA_DESCRIPTOR: u16 = 1 << 3;
const ZIP_FLAG_UTF8: u16 = 1 << 11;
const ZIP_STREAM_RELEVANT_FLAGS: u16 =
    ZIP_FLAG_ENCRYPTED | ZIP_FLAG_DATA_DESCRIPTOR | ZIP_FLAG_UTF8;

struct IpfS3ZipCentralDirectoryEntry {
    flags: u16,
    method: u16,
    raw_name: Vec<u8>,
    header_start: u64,
}

struct IpfS3ZipArchiveEntry {
    central: IpfS3ZipCentralDirectoryEntry,
    crc32: u32,
    compressed_size: u64,
    uncompressed_size: u64,
}

struct IpfS3ZipLocalHeader<'a> {
    flags: u16,
    method: u16,
    crc32: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    raw_name: &'a [u8],
    extra: &'a [u8],
    data_start: usize,
}

fn ipfs3_zip_u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    let value: [u8; 2] = bytes.get(offset..end)?.try_into().ok()?;
    Some(u16::from_le_bytes(value))
}

fn ipfs3_zip_u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let value: [u8; 4] = bytes.get(offset..end)?.try_into().ok()?;
    Some(u32::from_le_bytes(value))
}

fn ipfs3_zip_path_is_safe(value: &str, allow_empty: bool) -> bool {
    if value.is_empty() {
        return allow_empty;
    }
    let bytes = value.as_bytes();
    let has_windows_drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if value.contains('\\') || value.starts_with('/') || has_windows_drive {
        return false;
    }
    if value
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return false;
    }
    !value.trim_matches('/').is_empty()
}

fn ipfs3_zip_central_directory_entries(
    archive_bytes: &[u8],
    central_directory_start: u64,
    archive_len: usize,
) -> Option<Vec<IpfS3ZipCentralDirectoryEntry>> {
    let Ok(mut offset) = usize::try_from(central_directory_start) else {
        return None;
    };
    let mut entries = Vec::new();
    let mut raw_names = std::collections::HashSet::new();

    loop {
        let signature_end = offset.checked_add(ZIP_CENTRAL_HEADER_SIGNATURE.len())?;
        let signature = archive_bytes.get(offset..signature_end)?;
        if signature != ZIP_CENTRAL_HEADER_SIGNATURE {
            break;
        }
        let fixed_end = offset.checked_add(ZIP_CENTRAL_FIXED_HEADER_LEN)?;
        let header = archive_bytes.get(offset..fixed_end)?;

        let name_len = ipfs3_zip_u16_at(header, ZIP_CENTRAL_NAME_LEN_OFFSET)?;
        let extra_len = ipfs3_zip_u16_at(header, ZIP_CENTRAL_EXTRA_LEN_OFFSET)?;
        let comment_len = ipfs3_zip_u16_at(header, ZIP_CENTRAL_COMMENT_LEN_OFFSET)?;
        let name_len = usize::from(name_len);
        let name_end = fixed_end.checked_add(name_len)?;
        let variable_len = name_len
            .checked_add(usize::from(extra_len))
            .and_then(|len| len.checked_add(usize::from(comment_len)))?;
        let record_end = fixed_end.checked_add(variable_len)?;
        archive_bytes.get(offset..record_end)?;
        let raw_name = archive_bytes.get(fixed_end..name_end)?;
        let raw_name = raw_name.to_vec();
        if !raw_names.insert(raw_name.clone()) {
            return None;
        }
        entries.push(IpfS3ZipCentralDirectoryEntry {
            flags: ipfs3_zip_u16_at(header, ZIP_CENTRAL_FLAGS_OFFSET)?,
            method: ipfs3_zip_u16_at(header, ZIP_CENTRAL_METHOD_OFFSET)?,
            raw_name,
            header_start: u64::from(ipfs3_zip_u32_at(header, 42)?),
        });
        offset = record_end;
    }

    (entries.len() == archive_len).then_some(entries)
}

#[cfg(test)]
fn ipfs3_zip_central_directory_is_complete_and_unique(
    archive_bytes: &[u8],
    central_directory_start: u64,
    archive_len: usize,
) -> bool {
    ipfs3_zip_central_directory_entries(archive_bytes, central_directory_start, archive_len)
        .is_some()
}

fn ipfs3_zip_local_header(bytes: &[u8], header_start: u64) -> Option<IpfS3ZipLocalHeader<'_>> {
    let start = usize::try_from(header_start).ok()?;
    let fixed_end = start.checked_add(ZIP_LOCAL_FIXED_HEADER_LEN)?;
    let header = bytes.get(start..fixed_end)?;
    if header[..ZIP_LOCAL_HEADER_SIGNATURE.len()] != ZIP_LOCAL_HEADER_SIGNATURE {
        return None;
    }
    let flags = ipfs3_zip_u16_at(header, ZIP_LOCAL_FLAGS_OFFSET)?;
    let method = ipfs3_zip_u16_at(header, ZIP_LOCAL_METHOD_OFFSET)?;
    let crc32 = ipfs3_zip_u32_at(header, ZIP_LOCAL_CRC32_OFFSET)?;
    let compressed_size = ipfs3_zip_u32_at(header, ZIP_LOCAL_COMPRESSED_SIZE_OFFSET)?;
    let uncompressed_size = ipfs3_zip_u32_at(header, ZIP_LOCAL_UNCOMPRESSED_SIZE_OFFSET)?;
    let name_len = usize::from(ipfs3_zip_u16_at(header, ZIP_LOCAL_NAME_LEN_OFFSET)?);
    let extra_len = usize::from(ipfs3_zip_u16_at(header, ZIP_LOCAL_EXTRA_LEN_OFFSET)?);
    let name_end = fixed_end.checked_add(name_len)?;
    let data_start = name_end.checked_add(extra_len)?;
    Some(IpfS3ZipLocalHeader {
        flags,
        method,
        crc32,
        compressed_size,
        uncompressed_size,
        raw_name: bytes.get(fixed_end..name_end)?,
        extra: bytes.get(name_end..data_start)?,
        data_start,
    })
}

fn ipfs3_zip_data_descriptor_record_end(
    bytes: &[u8],
    descriptor_start: usize,
    entry: &IpfS3ZipArchiveEntry,
) -> Option<usize> {
    let signature_end = descriptor_start.checked_add(ZIP_DATA_DESCRIPTOR_SIGNATURE.len())?;
    let payload_start = match bytes.get(descriptor_start..signature_end) {
        Some(signature) if signature == ZIP_DATA_DESCRIPTOR_SIGNATURE => signature_end,
        _ => descriptor_start,
    };
    let payload_end = payload_start.checked_add(12)?;
    let payload = bytes.get(payload_start..payload_end)?;
    if ipfs3_zip_u32_at(payload, 0)? != entry.crc32
        || u64::from(ipfs3_zip_u32_at(payload, 4)?) != entry.compressed_size
        || u64::from(ipfs3_zip_u32_at(payload, 8)?) != entry.uncompressed_size
    {
        return None;
    }
    Some(payload_end)
}

fn ipfs3_zip_deflate_stream_is_compatible(compressed_data: &[u8]) -> bool {
    use flate2::{Decompress, FlushDecompress, Status};

    let Ok(expected_total_in) = u64::try_from(compressed_data.len()) else {
        return false;
    };
    let mut decompressor = Decompress::new(false);
    let mut output = [0; 8 * 1024];

    loop {
        let total_in_before = decompressor.total_in();
        let total_out_before = decompressor.total_out();
        let Ok(input_start) = usize::try_from(total_in_before) else {
            return false;
        };
        let Some(input) = compressed_data.get(input_start..) else {
            return false;
        };
        let Ok(status) = decompressor.decompress(input, &mut output, FlushDecompress::None) else {
            return false;
        };

        if status == Status::StreamEnd {
            return decompressor.total_in() == expected_total_in;
        }
        if decompressor.total_in() == total_in_before
            && decompressor.total_out() == total_out_before
        {
            return false;
        }
    }
}

fn ipfs3_zip_local_extra_is_compatible(extra: &[u8]) -> bool {
    let mut offset = 0usize;
    while offset < extra.len() {
        let Some(header_end) = offset.checked_add(4) else {
            return false;
        };
        let Some(header) = extra.get(offset..header_end) else {
            return false;
        };
        let Some(field_id) = ipfs3_zip_u16_at(header, 0) else {
            return false;
        };
        let Some(field_len) = ipfs3_zip_u16_at(header, 2) else {
            return false;
        };
        let Some(field_end) = header_end.checked_add(usize::from(field_len)) else {
            return false;
        };
        let Some(field_data) = extra.get(header_end..field_end) else {
            return false;
        };
        if field_id == 0x7075
            || (field_id == 0x6375
                && (field_data.is_empty() || (field_data[0] == 1 && field_data.len() < 5)))
        {
            return false;
        }
        offset = field_end;
    }
    true
}

fn ipfs3_zip_archive_is_compatible(
    archive_bytes: &[u8],
    requested_entry_names: &[String],
    extraction_prefix: &str,
) -> bool {
    let mut requested = std::collections::HashSet::new();
    if requested_entry_names
        .iter()
        .any(|name| !requested.insert(name.as_str()))
    {
        return false;
    }
    if !ipfs3_zip_path_is_safe(extraction_prefix, true) {
        return false;
    }

    let cursor = std::io::Cursor::new(archive_bytes);
    let Ok(mut archive) = zip::ZipArchive::new(cursor) else {
        return false;
    };
    let Some(central_entries) = ipfs3_zip_central_directory_entries(
        archive_bytes,
        archive.central_directory_start(),
        archive.len(),
    ) else {
        return false;
    };
    let mut entries = Vec::with_capacity(archive.len());
    for (index, central) in central_entries.into_iter().enumerate() {
        let Ok(file) = archive.by_index_raw(index) else {
            return false;
        };
        if file.name_raw() != central.raw_name {
            return false;
        }
        entries.push(IpfS3ZipArchiveEntry {
            central,
            crc32: file.crc32(),
            compressed_size: file.compressed_size(),
            uncompressed_size: file.size(),
        });
    }
    entries.sort_unstable_by_key(|entry| entry.central.header_start);

    let Ok(central_directory_start) = usize::try_from(archive.central_directory_start()) else {
        return false;
    };
    if entries
        .first()
        .is_none_or(|entry| entry.central.header_start != 0)
    {
        return false;
    }
    for (index, entry) in entries.iter().enumerate() {
        let Ok(raw_name) = std::str::from_utf8(&entry.central.raw_name) else {
            return false;
        };
        if !ipfs3_zip_path_is_safe(raw_name, false) || entry.central.flags & ZIP_FLAG_ENCRYPTED != 0
        {
            return false;
        }

        if entry.central.method != 0 && entry.central.method != 8 {
            return false;
        }
        let Some(local) = ipfs3_zip_local_header(archive_bytes, entry.central.header_start) else {
            return false;
        };
        let Ok(local_name) = std::str::from_utf8(local.raw_name) else {
            return false;
        };
        let non_ascii_local_name = local.raw_name.iter().any(|byte| !byte.is_ascii());
        let local_uses_descriptor = local.flags & ZIP_FLAG_DATA_DESCRIPTOR != 0;
        if !ipfs3_zip_path_is_safe(local_name, false)
            || local.raw_name != entry.central.raw_name
            || local.flags & ZIP_FLAG_ENCRYPTED != 0
            || local.flags & ZIP_STREAM_RELEVANT_FLAGS
                != entry.central.flags & ZIP_STREAM_RELEVANT_FLAGS
            || local.method != entry.central.method
            || !ipfs3_zip_local_extra_is_compatible(local.extra)
            || (non_ascii_local_name && local.flags & ZIP_FLAG_UTF8 == 0)
            || (local_uses_descriptor && local.method != 8)
        {
            return false;
        }

        let Ok(compressed_size) = usize::try_from(entry.compressed_size) else {
            return false;
        };
        let Some(data_end) = local.data_start.checked_add(compressed_size) else {
            return false;
        };
        if entry.central.method == 8 {
            let Some(compressed_data) = archive_bytes.get(local.data_start..data_end) else {
                return false;
            };
            if !ipfs3_zip_deflate_stream_is_compatible(compressed_data) {
                return false;
            }
        }
        let local_record_end = if local_uses_descriptor {
            let Some(record_end) =
                ipfs3_zip_data_descriptor_record_end(archive_bytes, data_end, entry)
            else {
                return false;
            };
            record_end
        } else {
            if local.crc32 != entry.crc32
                || u64::from(local.compressed_size) != entry.compressed_size
                || u64::from(local.uncompressed_size) != entry.uncompressed_size
            {
                return false;
            }
            data_end
        };
        let expected_next_start = match entries.get(index + 1) {
            Some(next) => match usize::try_from(next.central.header_start) {
                Ok(start) => start,
                Err(_) => return false,
            },
            None => central_directory_start,
        };
        if local_record_end != expected_next_start {
            return false;
        }
    }
    true
}

impl IpfS3Uploader {
    pub fn from_config(config: &IpfS3UploaderConfig) -> Result<Self> {
        let config = config.required()?;
        let credentials = Credentials::new(
            Some(&config.access_key_id),
            Some(&config.secret_access_key),
            None,
            None,
            None,
        )
        .map_err(|e| Error::Other(format!("invalid ipfS3 credentials: {e}")))?;
        let region = Region::Custom {
            region: config.region.clone(),
            endpoint: config.endpoint_url.clone(),
        };
        let mut bucket = Bucket::new(&config.bucket, region, credentials)
            .map_err(|e| Error::Other(format!("failed to build ipfS3 bucket client: {e}")))?;
        if config.path_style {
            bucket = bucket.with_path_style();
        }
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        Ok(Self {
            bucket,
            config,
            http,
        })
    }

    fn object_key(&self, index: usize, input: &ImageUploadInput<'_>) -> String {
        let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
        let hash = short_hash_hex(input.bytes);
        let ext = extension_for_upload(input.filename, input.bytes);
        let filename = format!("{timestamp}-{index:04}-{hash}.{ext}");
        if self.config.key_prefix.is_empty() {
            filename
        } else {
            format!("{}/{}", self.config.key_prefix, filename)
        }
    }

    fn warm_public_gateway(&self, cid: &str) {
        if !self.config.warm_public_gateway_after_upload {
            return;
        }

        let url = format!("{}/{}", self.config.gateway_url, cid);
        let http = self.http.clone();
        tokio::spawn(async move {
            match http.head(&url).send().await {
                Ok(resp) if !resp.status().is_success() => {
                    tracing::debug!("IPFS gateway warmup returned {} for {}", resp.status(), url);
                }
                Ok(_) => {}
                Err(e) => tracing::debug!("IPFS gateway warmup failed for {}: {}", url, e),
            }
        });
    }

    fn url_pair_for_cid(&self, cid: &str) -> TelegraphImageUrlPair {
        let public_url = format!("{}/{cid}", self.config.gateway_url);
        let preview_url = self
            .config
            .preview_gateway_url
            .as_ref()
            .map(|gateway| format!("{gateway}/{cid}"))
            .unwrap_or_else(|| public_url.clone());
        TelegraphImageUrlPair {
            preview_url,
            public_url,
        }
    }

    pub async fn upload_images_with_url_pairs(
        &self,
        images: &[ImageUploadInput<'_>],
    ) -> Result<Vec<TelegraphImageUrlPair>> {
        let mut urls = Vec::with_capacity(images.len());
        for (index, image) in images.iter().enumerate() {
            let key = self.object_key(index + 1, image);
            let content_type = detect_content_type(image.bytes);
            let response = self
                .bucket
                .put_object_with_content_type(&key, image.bytes, &content_type)
                .await
                .map_err(|e| Error::Other(format!("ipfS3 put_object failed for key {key}: {e}")))?;
            let status = response.status_code();
            if !(200..300).contains(&status) {
                return Err(Error::Api {
                    message: format!("ipfS3 put_object returned {status} for key {key}"),
                    status,
                });
            }
            let cid = response
                .as_str()
                .map_err(|e| {
                    Error::Other(format!(
                        "ipfS3 put_object for key {key} returned non-UTF-8 ETag: {e}"
                    ))
                })?
                .trim_matches('"')
                .trim();
            if cid.is_empty() {
                return Err(Error::Other(format!(
                    "ipfS3 put_object for key {key} returned no ETag (CID); \
                     cannot build public URL"
                )));
            }
            self.warm_public_gateway(cid);
            urls.push(self.url_pair_for_cid(cid));
        }
        Ok(urls)
    }

    fn archive_object_key(&self, input: &ZipArchiveUploadInput<'_>) -> String {
        let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
        let hash = short_hash_hex(input.bytes);
        let filename = format!("{timestamp}-archive-{hash}.zip");
        if self.config.key_prefix.is_empty() {
            filename
        } else {
            format!("{}/{}", self.config.key_prefix, filename)
        }
    }

    pub async fn upload_zip_archive_with_url_pairs(
        &self,
        archive: ZipArchiveUploadInput<'_>,
    ) -> Result<Option<Vec<TelegraphImageUrlPair>>> {
        if !self.config.zip_extract_enabled {
            return Ok(None);
        }
        if archive.entry_names.is_empty() {
            return Ok(Some(Vec::new()));
        }

        let key = self.archive_object_key(&archive);
        let archive_stem = key.strip_suffix(".zip").ok_or_else(|| {
            Error::Other(format!("ipfS3 ZIP object key {key} does not end in .zip"))
        })?;
        let extraction_prefix = format!("{archive_stem}/");
        if !ipfs3_zip_archive_is_compatible(archive.bytes, archive.entry_names, &extraction_prefix)
        {
            return Ok(None);
        }
        let mut upload_bucket = self.bucket.clone();
        upload_bucket.add_query("decompress-zip", &extraction_prefix);

        let command = Command::PutObject {
            content: archive.bytes,
            content_type: "application/zip",
            custom_headers: None,
            multipart: None,
        };
        let request = ReqwestRequest::new(upload_bucket.as_ref(), &key, command)
            .await
            .map_err(|error| {
                Error::Other(format!(
                    "failed to build ipfS3 ZIP put_object request for key {key}: {error}"
                ))
            })?;
        let response = request.response_data(false).await.map_err(|error| {
            Error::Other(format!(
                "ipfS3 ZIP put_object failed for key {key}: {error}"
            ))
        })?;
        let status = response.status_code();
        if !(200..300).contains(&status) {
            return Err(Error::Api {
                message: format!("ipfS3 ZIP put_object returned {status} for key {key}"),
                status,
            });
        }

        let extract_result = parse_ipfs3_zip_extract_result(response.bytes()).map_err(|error| {
            Error::Other(format!(
                "ipfS3 ZIP put_object for key {key} returned {error}"
            ))
        })?;
        let Some(cids) =
            ipfs3_zip_entry_cids(&extraction_prefix, archive.entry_names, extract_result)?
        else {
            return Ok(None);
        };
        let pairs = cids
            .into_iter()
            .map(|cid| {
                self.warm_public_gateway(&cid);
                self.url_pair_for_cid(&cid)
            })
            .collect();
        Ok(Some(pairs))
    }
}

#[async_trait]
impl ImageUploader for IpfS3Uploader {
    fn supports_zip_archive_upload(&self) -> bool {
        self.config.zip_extract_enabled
    }

    async fn upload_images(&self, images: &[ImageUploadInput<'_>]) -> Result<Vec<String>> {
        Ok(IpfS3Uploader::upload_images_with_url_pairs(self, images)
            .await?
            .into_iter()
            .map(|pair| pair.preview_url)
            .collect())
    }

    async fn upload_images_with_url_pairs(
        &self,
        images: &[ImageUploadInput<'_>],
    ) -> Result<Vec<TelegraphImageUrlPair>> {
        IpfS3Uploader::upload_images_with_url_pairs(self, images).await
    }

    async fn upload_zip_archive_with_url_pairs(
        &self,
        archive: ZipArchiveUploadInput<'_>,
    ) -> Result<Option<Vec<TelegraphImageUrlPair>>> {
        IpfS3Uploader::upload_zip_archive_with_url_pairs(self, archive).await
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename = "DecompressZipResult")]
struct IpfS3ZipExtractResult {
    #[serde(rename = "ArchiveKey")]
    _archive_key: String,
    #[serde(rename = "ArchiveETag")]
    _archive_etag: String,
    #[serde(rename = "ArchiveSize")]
    _archive_size: u64,
    #[serde(rename = "ExtractedCount")]
    extracted_count: usize,
    #[serde(rename = "FailedCount")]
    failed_count: usize,
    #[serde(rename = "Entries", default)]
    entries: IpfS3ZipExtractEntries,
    #[serde(rename = "Failures", default)]
    failures: IpfS3ZipExtractFailures,
}

#[derive(Debug, Default, Deserialize)]
struct IpfS3ZipExtractEntries {
    #[serde(rename = "Entry", default)]
    entries: Vec<IpfS3ZipExtractEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename = "Entry")]
struct IpfS3ZipExtractEntry {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "ETag")]
    etag: String,
    #[serde(rename = "Size")]
    _size: u64,
}

#[derive(Debug, Default, Deserialize)]
struct IpfS3ZipExtractFailures {
    #[serde(rename = "Failure", default)]
    failures: Vec<IpfS3ZipExtractFailure>,
}

#[derive(Debug, Deserialize)]
#[serde(rename = "Failure")]
struct IpfS3ZipExtractFailure {
    #[serde(rename = "EntryName")]
    entry_name: String,
    #[serde(rename = "Code")]
    _code: String,
    #[serde(rename = "Message")]
    _message: String,
}

fn validate_ipfs3_zip_extract_result_xml(body: &[u8]) -> Result<()> {
    const ROOT_ELEMENT: &[u8] = b"DecompressZipResult";

    let mut reader = quick_xml::Reader::from_reader(body);
    let mut buffer = Vec::new();
    let root_is_empty = loop {
        buffer.clear();
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Start(element)) => {
                if element.name().as_ref() != ROOT_ELEMENT {
                    return Err(Error::Other(
                        "invalid DecompressZipResult XML: expected DecompressZipResult root element"
                            .into(),
                    ));
                }
                break false;
            }
            Ok(quick_xml::events::Event::Empty(element)) => {
                if element.name().as_ref() != ROOT_ELEMENT {
                    return Err(Error::Other(
                        "invalid DecompressZipResult XML: expected DecompressZipResult root element"
                            .into(),
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
                if text.iter().all(u8::is_ascii_whitespace) => {}
            Ok(quick_xml::events::Event::Eof) | Ok(_) => {
                return Err(Error::Other(
                    "invalid DecompressZipResult XML: expected DecompressZipResult root element"
                        .into(),
                ));
            }
            Err(error) => {
                return Err(Error::Other(format!(
                    "invalid DecompressZipResult XML: {error}"
                )));
            }
        }
    };

    if !root_is_empty {
        buffer.clear();
        reader
            .read_to_end(quick_xml::name::QName(ROOT_ELEMENT))
            .map_err(|error| Error::Other(format!("invalid DecompressZipResult XML: {error}")))?;
    }

    loop {
        buffer.clear();
        match reader.read_event_into(&mut buffer) {
            Ok(quick_xml::events::Event::Eof) => return Ok(()),
            Ok(quick_xml::events::Event::Comment(_) | quick_xml::events::Event::PI(_)) => {}
            Ok(quick_xml::events::Event::Text(text))
                if text.iter().all(u8::is_ascii_whitespace) => {}
            Ok(quick_xml::events::Event::Start(_) | quick_xml::events::Event::Empty(_)) => {
                return Err(Error::Other(
                    "invalid DecompressZipResult XML: trailing XML content after root element"
                        .into(),
                ));
            }
            Ok(_) => {
                return Err(Error::Other(
                    "invalid DecompressZipResult XML: trailing XML content after root element"
                        .into(),
                ));
            }
            Err(error) => {
                return Err(Error::Other(format!(
                    "invalid DecompressZipResult XML: {error}"
                )));
            }
        }
    }
}

fn parse_ipfs3_zip_extract_result(body: &[u8]) -> Result<IpfS3ZipExtractResult> {
    if body.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Err(Error::Other(
            "invalid DecompressZipResult XML: empty response body".into(),
        ));
    }

    validate_ipfs3_zip_extract_result_xml(body)?;
    quick_xml::de::from_reader(std::io::Cursor::new(body))
        .map_err(|e| Error::Other(format!("invalid DecompressZipResult XML: {e}")))
}

fn ipfs3_zip_entry_cids(
    extraction_prefix: &str,
    entry_names: &[String],
    result: IpfS3ZipExtractResult,
) -> Result<Option<Vec<String>>> {
    if result.extracted_count != result.entries.entries.len() {
        return Err(Error::Other(format!(
            "ipfS3 ZIP extraction ExtractedCount {} does not match {} entries",
            result.extracted_count,
            result.entries.entries.len()
        )));
    }
    if result.failed_count != result.failures.failures.len() {
        return Err(Error::Other(format!(
            "ipfS3 ZIP extraction FailedCount {} does not match {} failures",
            result.failed_count,
            result.failures.failures.len()
        )));
    }
    let requested_names = entry_names
        .iter()
        .map(String::as_str)
        .collect::<std::collections::HashSet<_>>();
    if requested_names.len() != entry_names.len() {
        return Ok(None);
    }

    let mut entry_cids = std::collections::HashMap::new();
    for entry in result.entries.entries {
        let cid = entry.etag.trim().trim_matches('"').trim();
        if cid.is_empty() {
            let key_kind = if entry
                .key
                .strip_prefix(extraction_prefix)
                .is_some_and(|name| requested_names.contains(name))
            {
                "requested extraction key"
            } else {
                "extraction key"
            };
            return Err(Error::Other(format!(
                "ipfS3 ZIP extraction {key_kind} {} returned an empty CID",
                entry.key,
            )));
        }
        entry_cids.insert(entry.key, cid.to_string());
    }
    let mut cids = Vec::with_capacity(entry_names.len());

    for entry_name in entry_names {
        let key = format!("{extraction_prefix}{entry_name}");
        let Some(cid) = entry_cids.get(&key) else {
            return Ok(None);
        };
        cids.push(cid.clone());
    }

    if result
        .failures
        .failures
        .iter()
        .any(|failure| requested_names.contains(failure.entry_name.as_str()))
    {
        return Ok(None);
    }

    Ok(Some(cids))
}

fn extension_for_content_type(content_type: &str) -> &'static str {
    match content_type {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

fn extension_for_upload(filename: &str, bytes: &[u8]) -> &'static str {
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

fn safe_upload_filename(filename: &str, ext: &str) -> String {
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

fn short_hash_hex(bytes: &[u8]) -> String {
    let mut hash: u32 = 0x811c9dc5;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")
}

fn public_url_for_key(public_base_url: &str, key: &str) -> String {
    let base = public_base_url.trim_end_matches('/');
    let encoded_key = key
        .split('/')
        .map(urlencoding::encode)
        .collect::<Vec<_>>()
        .join("/");
    format!("{base}/{encoded_key}")
}

pub struct TelegraphClient {
    http: reqwest::Client,
    pixi: PixiUploader,
    /// Telegraph API base URL for page creation.
    api_url: String,
    /// Telegraph access token for page creation.
    telegraph_token: String,
}

impl TelegraphClient {
    /// Create a new client.
    /// `telegraph_token` is used for creating Telegraph pages.
    /// Image uploads to pixi.mg are anonymous (no auth needed).
    pub fn new(telegraph_token: String) -> Self {
        Self::new_with_urls(
            telegraph_token,
            "https://pixi.mg/api".to_string(),
            "https://api.telegra.ph".to_string(),
        )
    }

    /// Constructor with configurable endpoint URLs (for testing).
    pub fn new_with_urls(telegraph_token: String, upload_url: String, api_url: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("failed to build telegraph http client"),
            pixi: PixiUploader::new_with_url(upload_url),
            api_url,
            telegraph_token,
        }
    }

    /// Create a Telegraph account and return a client using the new access token.
    pub async fn create_account(
        short_name: &str,
        author_name: Option<&str>,
        author_url: Option<&str>,
    ) -> Result<Self> {
        Self::create_account_with_urls(
            short_name,
            author_name,
            author_url,
            "https://pixi.mg/api".to_string(),
            "https://api.telegra.ph".to_string(),
        )
        .await
    }

    /// Create a Telegraph account with configurable endpoint URLs (for testing).
    pub async fn create_account_with_urls(
        short_name: &str,
        author_name: Option<&str>,
        author_url: Option<&str>,
        upload_url: String,
        api_url: String,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("failed to build telegraph http client");

        let mut form = vec![("short_name", short_name)];
        if let Some(author_name) = author_name {
            form.push(("author_name", author_name));
        }
        if let Some(author_url) = author_url {
            form.push(("author_url", author_url));
        }

        let resp = http
            .post(format!("{}/createAccount", api_url))
            .form(&form)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("createAccount returned {}", status),
                status: status.as_u16(),
            });
        }

        let telegraph_resp: TelegraphResponse<AccountResult> = resp.json().await?;
        if telegraph_resp.ok {
            if let Some(result) = telegraph_resp.result {
                return Ok(Self::new_with_urls(
                    result.access_token,
                    upload_url,
                    api_url,
                ));
            }
        }
        Err(Error::Api {
            message: telegraph_resp
                .error
                .unwrap_or_else(|| "unknown error".into()),
            status: 0,
        })
    }

    /// Upload an image to pixi.mg. Returns the direct image URL.
    /// Pixi.mg supports JPEG, PNG, GIF, WebP. No auth required. Permanent URLs.
    pub async fn upload_image(&self, image_data: &[u8], _filename: &str) -> Result<String> {
        let urls = self.upload_images_batch(&[image_data]).await?;
        urls.into_iter()
            .next()
            .ok_or_else(|| Error::Parse("pixi.mg upload returned no url".into()))
    }

    /// Upload up to 5 images in a single request to pixi.mg.
    /// Returns direct image URLs in the same order as input.
    pub async fn upload_images_batch(&self, images: &[&[u8]]) -> Result<Vec<String>> {
        self.pixi.upload_images_batch(images).await
    }

    /// Upload images with automatic 429 backoff. Retries up to `max_retries` times
    /// on HTTP 429, waiting exponentially longer each time (40s, 80s, 160s).
    /// Returns the uploaded URLs (all images in one batch).
    pub async fn upload_images_with_retry(
        &self,
        images: &[&[u8]],
        max_retries: u32,
    ) -> Result<Vec<String>> {
        self.pixi
            .upload_images_with_retry(images, max_retries)
            .await
    }

    /// Create a Telegraph page. Returns the page URL.
    pub async fn create_page(&self, title: &str, content: &[Node]) -> Result<String> {
        let content_json = serde_json::to_value(content)?;
        let content_str = content_json.to_string();
        let form = vec![
            ("access_token", self.telegraph_token.as_str()),
            ("title", title),
            ("content", content_str.as_str()),
            ("return_content", "false"),
        ];

        let resp = self
            .http
            .post(format!("{}/createPage", self.api_url))
            .form(&form)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("createPage returned {}", status),
                status: status.as_u16(),
            });
        }

        let telegraph_resp: TelegraphResponse<PageResult> = resp.json().await?;
        if telegraph_resp.ok {
            if let Some(result) = telegraph_resp.result {
                return Ok(result.url);
            }
        }
        Err(Error::Api {
            message: telegraph_resp
                .error
                .unwrap_or_else(|| "unknown error".into()),
            status: 0,
        })
    }

    pub async fn edit_page(&self, path: &str, title: &str, content: &[Node]) -> Result<String> {
        let content_json = serde_json::to_value(content)?;
        let content_str = content_json.to_string();
        let form = vec![
            ("access_token", self.telegraph_token.as_str()),
            ("path", path),
            ("title", title),
            ("content", content_str.as_str()),
            ("return_content", "false"),
        ];

        let resp = self
            .http
            .post(format!("{}/editPage", self.api_url))
            .form(&form)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("editPage returned {}", status),
                status: status.as_u16(),
            });
        }

        let telegraph_resp: TelegraphResponse<PageResult> = resp.json().await?;
        if telegraph_resp.ok {
            if let Some(result) = telegraph_resp.result {
                return Ok(result.url);
            }
        }
        Err(Error::Api {
            message: telegraph_resp
                .error
                .unwrap_or_else(|| "unknown error".into()),
            status: 0,
        })
    }

    /// Create a gallery page from image URLs. Splits into multiple pages if needed.
    /// Returns the first page URL (with "Next Page" links to subsequent pages).
    ///
    /// If a page creation fails partway through, returns the last successfully
    /// created page URL (if any) instead of erroring, so the user can still
    /// access the partial gallery.
    pub async fn create_gallery_page(&self, title: &str, image_urls: &[String]) -> Result<String> {
        let pairs: Vec<TelegraphImageUrlPair> = image_urls
            .iter()
            .map(|url| TelegraphImageUrlPair {
                preview_url: url.clone(),
                public_url: url.clone(),
            })
            .collect();
        Ok(self
            .create_gallery_page_with_url_pairs(title, &pairs, None, None)
            .await?
            .first_page_url)
    }

    pub async fn create_gallery_page_with_url_pairs(
        &self,
        title: &str,
        image_urls: &[TelegraphImageUrlPair],
        preview_gateway_url: Option<&str>,
        public_gateway_url: Option<&str>,
    ) -> Result<TelegraphGalleryPageResult> {
        if image_urls.is_empty() {
            return Err(Error::Other("no images to upload".into()));
        }

        let chunks = split_url_pairs_for_pages(image_urls, MAX_PAGE_CONTENT_BYTES);
        let should_rewrite =
            preview_gateway_url
                .zip(public_gateway_url)
                .is_some_and(|(preview, public)| {
                    preview.trim_end_matches('/') != public.trim_end_matches('/')
                });
        let mut rewrite_pages = Vec::new();

        if chunks.len() == 1 {
            let nodes: Vec<Node> = chunks[0]
                .iter()
                .map(|pair| Node::img(&pair.preview_url))
                .collect();
            let url = self.create_page(title, &nodes).await?;
            if should_rewrite {
                if let Some(path) = telegraph_path_from_url(&url) {
                    rewrite_pages.push(TelegraphRewritePage {
                        path,
                        title: title.to_string(),
                        content: nodes,
                    });
                }
            }
            return Ok(TelegraphGalleryPageResult {
                first_page_url: url,
                rewrite_data: build_rewrite_data(
                    rewrite_pages,
                    preview_gateway_url,
                    public_gateway_url,
                ),
            });
        }

        // Multi-page: create in reverse order, linking to the next page.
        // The first page (created last in the loop) gets the original title.
        let total_pages = chunks.len();
        let mut next_url: Option<String> = None;
        for (idx, chunk) in chunks.iter().rev().enumerate() {
            let mut nodes: Vec<Node> = Vec::new();
            for pair in chunk {
                nodes.push(Node::img(&pair.preview_url));
            }
            if let Some(ref next) = next_url {
                nodes.push(Node::link(next, "Next Page →"));
            }
            let page_title = if idx == total_pages - 1 {
                title.to_string()
            } else {
                format!("{} (continued)", title)
            };
            match self.create_page(&page_title, &nodes).await {
                Ok(url) => {
                    if should_rewrite {
                        if let Some(path) = telegraph_path_from_url(&url) {
                            rewrite_pages.push(TelegraphRewritePage {
                                path,
                                title: page_title,
                                content: nodes,
                            });
                        }
                    }
                    next_url = Some(url)
                }
                Err(e) => {
                    // If we have at least one page created, return it instead of failing
                    if let Some(ref url) = next_url {
                        tracing::warn!(
                            "Telegraph page creation failed at index {}, returning last successful page: {}",
                            idx,
                            e
                        );
                        return Ok(TelegraphGalleryPageResult {
                            first_page_url: url.clone(),
                            rewrite_data: build_rewrite_data(
                                rewrite_pages,
                                preview_gateway_url,
                                public_gateway_url,
                            ),
                        });
                    }
                    return Err(e);
                }
            }
        }

        Ok(TelegraphGalleryPageResult {
            first_page_url: next_url.unwrap_or_else(|| image_urls[0].preview_url.clone()),
            rewrite_data: build_rewrite_data(
                rewrite_pages,
                preview_gateway_url,
                public_gateway_url,
            ),
        })
    }
}

fn telegraph_path_from_url(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()?
        .path_segments()?
        .next_back()
        .map(str::to_string)
}

fn build_rewrite_data(
    pages: Vec<TelegraphRewritePage>,
    preview_gateway_url: Option<&str>,
    public_gateway_url: Option<&str>,
) -> Option<TelegraphRewriteData> {
    let (preview, public) = preview_gateway_url.zip(public_gateway_url)?;
    if pages.is_empty() || preview.trim_end_matches('/') == public.trim_end_matches('/') {
        return None;
    }
    Some(TelegraphRewriteData {
        pages,
        preview_gateway_url: preview.trim_end_matches('/').to_string(),
        public_gateway_url: public.trim_end_matches('/').to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_image_node() {
        let node = Node::img("https://i.pixi.mg/i/abc.jpg");
        assert_eq!(node.tag, "img");
        assert_eq!(node.attrs.unwrap()["src"], "https://i.pixi.mg/i/abc.jpg");
    }

    #[test]
    fn test_build_link_node() {
        let node = Node::link("https://example.com", "Next Page");
        assert_eq!(node.tag, "a");
        assert_eq!(node.attrs.unwrap()["href"], "https://example.com");
        assert_eq!(node.children.unwrap()[0], serde_json::json!("Next Page"));
    }

    #[test]
    fn test_content_size_estimate() {
        let nodes = vec![
            Node::img("https://i.pixi.mg/i/abc.jpg"),
            Node::img("https://i.pixi.mg/i/def.jpg"),
        ];
        let size = estimate_content_size(&nodes);
        assert!(size > 0);
    }

    #[test]
    fn test_split_image_urls_for_pages() {
        let urls: Vec<String> = (0..50)
            .map(|i| format!("https://i.pixi.mg/i/{}.jpg", i))
            .collect();
        let chunks = split_for_pages(&urls, 1024);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| !c.is_empty()));
    }

    #[test]
    fn test_split_for_pages_reserves_next_link_budget() {
        // Each image node JSON: {"tag":"img","attrs":{"src":"<URL>"}} = 32 + URL_len bytes.
        // For URL_len=478: node = 510 bytes. 2 nodes + array overhead = 1023 bytes.
        // Max=1024: 3rd node would be 1533 > 1024, so chunk 1 has 2 nodes = 1023 bytes.
        // Adding a link (~92 bytes) = 1115 > 1024 — should overflow WITHOUT the budget fix.
        let url = format!("https://img.example/{}", "x".repeat(478 - 22)); // 22 = "https://img.example/".len()
        let urls = vec![url; 6];
        let max_bytes = 1024usize;
        let pages = split_for_pages(&urls, max_bytes);
        assert!(
            pages.len() > 1,
            "expected multiple pages, got {}",
            pages.len()
        );
        for (idx, page) in pages.iter().enumerate() {
            let mut nodes: Vec<Node> = page.iter().map(|u| Node::img(u)).collect();
            if idx + 1 < pages.len() {
                nodes.push(Node::link("https://telegra.ph/next", "Next Page \u{2192}"));
            }
            let size = estimate_content_size(&nodes);
            assert!(
                size <= max_bytes,
                "page {} size {} exceeds max {} (without budget reservation)",
                idx,
                size,
                max_bytes
            );
        }
    }

    #[test]
    fn split_url_pairs_for_pages_accounts_for_longer_public_urls() {
        let pairs: Vec<TelegraphImageUrlPair> = (0..6)
            .map(|i| TelegraphImageUrlPair {
                preview_url: format!("https://p.example/ipfs/cid-{i}"),
                public_url: format!("https://public.example/ipfs/{}-cid-{i}", "x".repeat(210)),
            })
            .collect();
        let preview_urls: Vec<String> = pairs.iter().map(|pair| pair.preview_url.clone()).collect();

        assert_eq!(split_for_pages(&preview_urls, 1024).len(), 1);

        let pages = split_url_pairs_for_pages(&pairs, 1024);
        assert!(
            pages.len() > 1,
            "expected public URL size to force multiple pages"
        );
        for (idx, page) in pages.iter().enumerate() {
            let mut nodes: Vec<Node> = page
                .iter()
                .map(|pair| Node::img(&pair.public_url))
                .collect();
            if idx + 1 < pages.len() {
                nodes.push(Node::link("https://telegra.ph/next", "Next Page →"));
            }
            assert!(
                estimate_content_size(&nodes) <= 1024,
                "rewritten page {idx} should fit Telegraph content budget"
            );
        }
    }

    #[test]
    fn rewrite_ipfs_gateway_nodes_rewrites_only_preview_image_sources() {
        let nodes = vec![
            Node::img("https://preview.example/ipfs/cid-one"),
            Node::link("https://preview.example/ipfs/not-an-image", "Next"),
            Node::img("https://public.example/ipfs/cid-two"),
        ];

        let rewritten = rewrite_ipfs_gateway_nodes(
            &nodes,
            "https://preview.example/ipfs/",
            "https://public.example/ipfs/",
        );

        assert_eq!(
            node_attr_str(&rewritten[0], "src"),
            Some("https://public.example/ipfs/cid-one")
        );
        assert_eq!(
            node_attr_str(&rewritten[1], "href"),
            Some("https://preview.example/ipfs/not-an-image")
        );
        assert_eq!(
            node_attr_str(&rewritten[2], "src"),
            Some("https://public.example/ipfs/cid-two")
        );
    }

    #[tokio::test]
    async fn test_create_account_returns_client_with_access_token() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/createAccount"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {
                    "access_token": "auto-created-token"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = TelegraphClient::create_account_with_urls(
            "PixivBot",
            Some("PixivBot"),
            None,
            "https://pixi.example/api".to_string(),
            server.uri(),
        )
        .await
        .expect("createAccount should build a Telegraph client");

        assert_eq!(client.telegraph_token, "auto-created-token");
    }

    #[test]
    fn image_upload_config_defaults_to_pixi() {
        let cfg = ImageUploadConfig::default();
        assert_eq!(cfg.provider, ImageUploadProvider::Pixi);
        assert!(cfg.s3.is_none());
        assert_eq!(cfg.catbox.api_url, "https://catbox.moe/user/api.php");
    }

    #[test]
    fn s3_config_requires_fields_for_provider() {
        let cfg = ImageUploadConfig {
            provider: ImageUploadProvider::S3,
            s3: Some(S3UploaderConfig::default()),
            ..Default::default()
        };
        let err = cfg.s3.unwrap().required().unwrap_err();
        assert!(err.to_string().contains("image_upload.s3.endpoint_url"));
    }

    #[test]
    fn s3_config_rejects_invalid_public_base_url() {
        let mut cfg = complete_s3_config("http://localhost:9000", "not a url");
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("image_upload.s3.public_base_url"));

        cfg.public_base_url = Some("ftp://cdn.example.com".to_string());
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must use http or https"));
    }

    #[test]
    fn s3_config_rejects_public_base_url_with_secret_or_non_path_parts() {
        let mut cfg = complete_s3_config(
            "http://localhost:9000",
            "https://user:pass@cdn.example.com/base",
        );
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must not contain userinfo"));

        cfg.public_base_url = Some("https://cdn.example.com/base?token=secret".to_string());
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must not contain query"));

        cfg.public_base_url = Some("https://cdn.example.com/base#frag".to_string());
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must not contain fragment"));
    }

    #[test]
    fn s3_config_rejects_unsafe_endpoint_url() {
        let mut cfg = complete_s3_config("ftp://localhost:9000", "https://cdn.example.com");
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must use http or https"));

        cfg.endpoint_url = Some("https://user:secret@s3.example.com".to_string());
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must not contain userinfo"));

        cfg.endpoint_url = Some("https://s3.example.com?token=secret".to_string());
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must not contain query"));
    }

    #[test]
    fn ipfs3_config_requires_fields_for_provider() {
        let cfg = IpfS3UploaderConfig::default();
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("image_upload.ipfs3.endpoint_url"));
    }

    #[test]
    fn ipfs3_config_rejects_invalid_gateway_url() {
        let mut cfg = complete_ipfs3_config("http://localhost:9000", "not a url");
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("image_upload.ipfs3.gateway_url"));

        cfg.gateway_url = Some("ftp://ipfs.io/ipfs".to_string());
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must use http or https"));
    }

    #[test]
    fn ipfs3_config_rejects_gateway_url_with_secret_or_non_path_parts() {
        let mut cfg =
            complete_ipfs3_config("http://localhost:9000", "https://user:pass@ipfs.io/ipfs");
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must not contain userinfo"));

        cfg.gateway_url = Some("https://ipfs.io/ipfs?token=secret".to_string());
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must not contain query"));

        cfg.gateway_url = Some("https://ipfs.io/ipfs#frag".to_string());
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must not contain fragment"));
    }

    #[test]
    fn ipfs3_config_rejects_unsafe_endpoint_url() {
        let mut cfg = complete_ipfs3_config("ftp://localhost:9000", "https://ipfs.io/ipfs");
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must use http or https"));

        cfg.endpoint_url = Some("https://user:secret@ipfs3.example.com".to_string());
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must not contain userinfo"));

        cfg.endpoint_url = Some("https://ipfs3.example.com?token=secret".to_string());
        let err = cfg.required().unwrap_err();
        assert!(err.to_string().contains("must not contain query"));
    }

    #[test]
    fn ipfs3_config_trims_gateway_url_trailing_slash() {
        let cfg = complete_ipfs3_config("http://localhost:9000", "https://ipfs.io/ipfs/");
        let resolved = cfg.required().unwrap();
        assert_eq!(resolved.gateway_url, "https://ipfs.io/ipfs");
    }

    #[test]
    fn ipfs3_preview_rewrite_config_normalizes_and_skips_same_gateway() {
        let mut cfg = ImageUploadConfig {
            provider: ImageUploadProvider::IpfS3,
            ipfs3: Some(complete_ipfs3_config(
                "http://localhost:9000",
                "https://public.example/ipfs/",
            )),
            ..Default::default()
        };
        cfg.ipfs3.as_mut().unwrap().preview_gateway_url =
            Some("https://preview.example/ipfs/".to_string());
        cfg.ipfs3.as_mut().unwrap().preview_rewrite_delay_sec = 42;

        let rewrite = cfg.ipfs3_preview_rewrite_config().unwrap();
        assert_eq!(rewrite.preview_gateway_url, "https://preview.example/ipfs");
        assert_eq!(rewrite.public_gateway_url, "https://public.example/ipfs");
        assert_eq!(rewrite.delay_sec, 42);

        cfg.ipfs3.as_mut().unwrap().preview_gateway_url =
            Some("https://public.example/ipfs/".to_string());
        assert!(cfg.ipfs3_preview_rewrite_config().is_none());

        cfg.provider = ImageUploadProvider::Pixi;
        cfg.ipfs3.as_mut().unwrap().preview_gateway_url =
            Some("https://preview.example/ipfs/".to_string());
        assert!(cfg.ipfs3_preview_rewrite_config().is_none());
    }

    #[test]
    fn catbox_config_rejects_unsafe_api_url() {
        let err = CatboxUploader::from_config(&CatboxUploaderConfig {
            api_url: "ftp://catbox.moe/user/api.php".to_string(),
            userhash: None,
        })
        .err()
        .unwrap();
        assert!(err.to_string().contains("must use http or https"));

        let err = CatboxUploader::from_config(&CatboxUploaderConfig {
            api_url: "https://user:secret@catbox.moe/user/api.php".to_string(),
            userhash: None,
        })
        .err()
        .unwrap();
        assert!(err.to_string().contains("must not contain userinfo"));

        let err = CatboxUploader::from_config(&CatboxUploaderConfig {
            api_url: "https://catbox.moe/user/api.php?token=secret".to_string(),
            userhash: None,
        })
        .err()
        .unwrap();
        assert!(err.to_string().contains("must not contain query"));
    }

    #[test]
    fn public_url_encodes_key_segments_and_trims_base() {
        let url = public_url_for_key("https://cdn.example.com/base/", "eh/hello world/01#.jpg");
        assert_eq!(
            url,
            "https://cdn.example.com/base/eh/hello%20world/01%23.jpg"
        );
    }

    #[test]
    fn extension_prefers_detected_content_type() {
        assert_eq!(extension_for_upload("x.bin", b"\xFF\xD8\xFF\x00"), "jpg");
        assert_eq!(extension_for_upload("x.webp", b"not image"), "webp");
        assert_eq!(extension_for_upload("x.unknown", b"not image"), "bin");
    }

    #[test]
    fn safe_upload_filename_sanitizes_path_and_extension() {
        assert_eq!(
            safe_upload_filename("dir/my image!.png", "jpg"),
            "my_image_.jpg"
        );
        assert_eq!(safe_upload_filename("", "bin"), "image.bin");
    }

    fn complete_s3_config(endpoint: &str, public_base_url: &str) -> S3UploaderConfig {
        S3UploaderConfig {
            endpoint_url: Some(endpoint.to_string()),
            bucket: Some("bucket".to_string()),
            region: Some("auto".to_string()),
            access_key_id: Some("key".to_string()),
            secret_access_key: Some("secret".to_string()),
            public_base_url: Some(public_base_url.to_string()),
            key_prefix: "eh".to_string(),
            path_style: true,
        }
    }

    fn complete_ipfs3_config(endpoint: &str, gateway_url: &str) -> IpfS3UploaderConfig {
        IpfS3UploaderConfig {
            endpoint_url: Some(endpoint.to_string()),
            bucket: Some("bucket".to_string()),
            region: Some("auto".to_string()),
            access_key_id: Some("key".to_string()),
            secret_access_key: Some("secret".to_string()),
            gateway_url: Some(gateway_url.to_string()),
            preview_gateway_url: None,
            preview_rewrite_delay_sec: default_ipfs_preview_rewrite_delay_sec(),
            key_prefix: "eh".to_string(),
            path_style: true,
            warm_public_gateway_after_upload: false,
            zip_extract_enabled: false,
        }
    }

    #[derive(Debug)]
    struct MultipartContains(&'static str);

    impl wiremock::Match for MultipartContains {
        fn matches(&self, request: &wiremock::Request) -> bool {
            String::from_utf8_lossy(&request.body).contains(self.0)
        }
    }

    #[tokio::test]
    async fn s3_uploader_puts_object_and_returns_public_url() {
        use wiremock::matchers::{body_bytes, method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/bucket/eh/\d{14}-0001-[0-9a-f]{8}\.png$"))
            .and(body_bytes(vec![
                0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n',
            ]))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let uploader = S3Uploader::from_config(&complete_s3_config(
            &server.uri(),
            "https://cdn.example.com/root/",
        ))
        .unwrap();
        let urls = uploader
            .upload_images(&[ImageUploadInput {
                filename: "image.png",
                bytes: b"\x89PNG\r\n\x1a\n",
            }])
            .await
            .unwrap();

        assert_eq!(urls.len(), 1);
        assert!(urls[0].starts_with("https://cdn.example.com/root/eh/"));
        assert!(urls[0].ends_with(".png"));
    }

    #[tokio::test]
    async fn s3_uploader_returns_error_on_failed_put() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/bucket/eh/.*\.jpg$"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let uploader = S3Uploader::from_config(&complete_s3_config(
            &server.uri(),
            "https://cdn.example.com",
        ))
        .unwrap();
        let err = uploader
            .upload_images(&[ImageUploadInput {
                filename: "image.jpg",
                bytes: b"\xFF\xD8\xFF\x00",
            }])
            .await
            .unwrap_err();

        assert!(err.to_string().contains("S3 put_object returned 500"));
    }

    #[tokio::test]
    async fn ipfs3_uploader_puts_object_and_returns_gateway_url_from_etag() {
        use wiremock::matchers::{body_bytes, method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let cid = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/bucket/eh/\d{14}-0001-[0-9a-f]{8}\.png$"))
            .and(body_bytes(vec![
                0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n',
            ]))
            .respond_with(ResponseTemplate::new(200).insert_header("etag", format!("\"{cid}\"")))
            .expect(1)
            .mount(&server)
            .await;

        let uploader = IpfS3Uploader::from_config(&complete_ipfs3_config(
            &server.uri(),
            "https://ipfs.io/ipfs",
        ))
        .unwrap();
        let urls = uploader
            .upload_images(&[ImageUploadInput {
                filename: "image.png",
                bytes: b"\x89PNG\r\n\x1a\n",
            }])
            .await
            .unwrap();

        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0], format!("https://ipfs.io/ipfs/{cid}"));
    }

    #[tokio::test]
    async fn ipfs3_uploader_returns_preview_and_public_url_pairs() {
        use wiremock::matchers::{body_bytes, method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let cid = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/bucket/eh/\d{14}-0001-[0-9a-f]{8}\.png$"))
            .and(body_bytes(vec![
                0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n',
            ]))
            .respond_with(ResponseTemplate::new(200).insert_header("etag", format!("\"{cid}\"")))
            .expect(1)
            .mount(&server)
            .await;

        let mut cfg = complete_ipfs3_config(&server.uri(), "https://public.example/ipfs/");
        cfg.preview_gateway_url = Some("https://preview.example/ipfs/".to_string());
        let uploader = IpfS3Uploader::from_config(&cfg).unwrap();
        let pairs = uploader
            .upload_images_with_url_pairs(&[ImageUploadInput {
                filename: "image.png",
                bytes: b"\x89PNG\r\n\x1a\n",
            }])
            .await
            .unwrap();

        assert_eq!(
            pairs,
            vec![TelegraphImageUrlPair {
                preview_url: format!("https://preview.example/ipfs/{cid}"),
                public_url: format!("https://public.example/ipfs/{cid}"),
            }]
        );
    }

    #[tokio::test]
    async fn ipfs3_uploader_warms_public_gateway_after_upload_without_blocking_result() {
        use wiremock::matchers::{method, path, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let cid = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/bucket/eh/\d{14}-0001-[0-9a-f]{8}\.png$"))
            .respond_with(ResponseTemplate::new(200).insert_header("etag", format!("\"{cid}\"")))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .and(path(format!("/ipfs/{cid}")))
            .respond_with(ResponseTemplate::new(503).set_body_bytes(vec![b'x'; 1024 * 1024]))
            .expect(1)
            .mount(&server)
            .await;

        let mut config = complete_ipfs3_config(&server.uri(), &format!("{}/ipfs", server.uri()));
        config.warm_public_gateway_after_upload = true;
        let uploader = IpfS3Uploader::from_config(&config).unwrap();
        let urls = uploader
            .upload_images(&[ImageUploadInput {
                filename: "image.png",
                bytes: b"\x89PNG\r\n\x1a\n",
            }])
            .await
            .unwrap();

        assert_eq!(urls, vec![format!("{}/ipfs/{cid}", server.uri())]);
        for _ in 0..20 {
            let received = server.received_requests().await.unwrap();
            if received.iter().any(|request| {
                request.method.as_str() == "HEAD" && request.url.path() == format!("/ipfs/{cid}")
            }) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("expected non-blocking IPFS gateway warmup HEAD request");
    }

    #[tokio::test]
    async fn ipfs3_uploader_returns_error_when_etag_missing() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/bucket/eh/.*\.png$"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let uploader = IpfS3Uploader::from_config(&complete_ipfs3_config(
            &server.uri(),
            "https://ipfs.io/ipfs",
        ))
        .unwrap();
        let err = uploader
            .upload_images(&[ImageUploadInput {
                filename: "image.png",
                bytes: b"\x89PNG\r\n\x1a\n",
            }])
            .await
            .unwrap_err();

        assert!(err.to_string().contains("no ETag (CID)"));
    }

    #[tokio::test]
    async fn ipfs3_uploader_returns_error_on_failed_put() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/bucket/eh/.*\.jpg$"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let uploader = IpfS3Uploader::from_config(&complete_ipfs3_config(
            &server.uri(),
            "https://ipfs.io/ipfs",
        ))
        .unwrap();
        let err = uploader
            .upload_images(&[ImageUploadInput {
                filename: "image.jpg",
                bytes: b"\xFF\xD8\xFF\x00",
            }])
            .await
            .unwrap_err();

        assert!(err.to_string().contains("ipfS3 put_object returned 500"));
    }

    #[tokio::test]
    async fn catbox_uploader_posts_file_and_returns_url() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/user/api.php"))
            .and(MultipartContains("name=\"reqtype\""))
            .and(MultipartContains("fileupload"))
            .and(MultipartContains("name=\"userhash\""))
            .and(MultipartContains("userhash"))
            .and(MultipartContains("name=\"fileToUpload\""))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("https://files.catbox.moe/abc123.png"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let uploader = CatboxUploader::from_config(&CatboxUploaderConfig {
            api_url: format!("{}/user/api.php", server.uri()),
            userhash: Some("userhash".to_string()),
        })
        .unwrap();
        let urls = uploader
            .upload_images(&[ImageUploadInput {
                filename: "image.png",
                bytes: b"\x89PNG\r\n\x1a\n",
            }])
            .await
            .unwrap();

        assert_eq!(urls, vec!["https://files.catbox.moe/abc123.png"]);
    }

    #[tokio::test]
    async fn catbox_uploader_rejects_non_url_response() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/user/api.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string("error: file too large"))
            .expect(1)
            .mount(&server)
            .await;

        let uploader = CatboxUploader::from_config(&CatboxUploaderConfig {
            api_url: format!("{}/user/api.php", server.uri()),
            userhash: None,
        })
        .unwrap();
        let err = uploader
            .upload_images(&[ImageUploadInput {
                filename: "image.png",
                bytes: b"\x89PNG\r\n\x1a\n",
            }])
            .await
            .unwrap_err();

        assert!(err.to_string().contains("non-url response"));
    }

    #[tokio::test]
    async fn edit_page_posts_path_title_and_content() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/editPage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "url": "https://telegra.ph/Page-Path" }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = TelegraphClient::new_with_urls(
            "token".to_string(),
            "https://pixi.example/api".to_string(),
            server.uri(),
        );
        let url = client
            .edit_page(
                "Page-Path",
                "Title",
                &[Node::img("https://img.example/a.jpg")],
            )
            .await
            .unwrap();

        assert_eq!(url, "https://telegra.ph/Page-Path");
        let requests = server.received_requests().await.unwrap();
        let body = String::from_utf8_lossy(&requests[0].body);
        assert!(body.contains("access_token=token"));
        assert!(body.contains("path=Page-Path"));
        assert!(body.contains("title=Title"));
        assert!(body.contains("return_content=false"));
        assert!(body.contains("content="));
    }

    #[tokio::test]
    async fn create_gallery_page_with_url_pairs_returns_rewrite_metadata() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/createPage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "url": "https://telegra.ph/Gallery-Path" }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = TelegraphClient::new_with_urls(
            "token".to_string(),
            "https://pixi.example/api".to_string(),
            server.uri(),
        );
        let result = client
            .create_gallery_page_with_url_pairs(
                "Gallery",
                &[TelegraphImageUrlPair {
                    preview_url: "https://preview.example/ipfs/cid-one".to_string(),
                    public_url: "https://public.example/ipfs/cid-one".to_string(),
                }],
                Some("https://preview.example/ipfs/"),
                Some("https://public.example/ipfs/"),
            )
            .await
            .unwrap();

        assert_eq!(result.first_page_url, "https://telegra.ph/Gallery-Path");
        let rewrite = result.rewrite_data.unwrap();
        assert_eq!(rewrite.preview_gateway_url, "https://preview.example/ipfs");
        assert_eq!(rewrite.public_gateway_url, "https://public.example/ipfs");
        assert_eq!(rewrite.pages.len(), 1);
        assert_eq!(rewrite.pages[0].path, "Gallery-Path");
        assert_eq!(rewrite.pages[0].title, "Gallery");
        assert_eq!(
            node_attr_str(&rewrite.pages[0].content[0], "src"),
            Some("https://preview.example/ipfs/cid-one")
        );
        let public_nodes = rewrite_ipfs_gateway_nodes(
            &rewrite.pages[0].content,
            &rewrite.preview_gateway_url,
            &rewrite.public_gateway_url,
        );
        assert_eq!(
            node_attr_str(&public_nodes[0], "src"),
            Some("https://public.example/ipfs/cid-one")
        );
    }

    #[test]
    fn ipfs3_zip_extract_disabled_by_default() {
        let config = IpfS3UploaderConfig::default();
        assert!(!config.zip_extract_enabled);
    }

    const ZIP_FIXTURE_DATA: &[u8] = b"hello";
    const ZIP_FIXTURE_DEFLATED_DATA: &[u8] = &[0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00];

    #[derive(Clone, Copy)]
    struct ZipEntryFixture<'a> {
        local_name: &'a [u8],
        central_name: &'a [u8],
        local_extra: &'a [u8],
        central_extra: &'a [u8],
        data: &'a [u8],
        local_method: u16,
        central_method: u16,
        local_flags: u16,
        central_flags: u16,
        data_descriptor_has_signature: bool,
    }

    impl<'a> ZipEntryFixture<'a> {
        fn stored(name: &'a [u8]) -> Self {
            Self {
                local_name: name,
                central_name: name,
                local_extra: &[],
                central_extra: &[],
                data: ZIP_FIXTURE_DATA,
                local_method: 0,
                central_method: 0,
                local_flags: 0,
                central_flags: 0,
                data_descriptor_has_signature: true,
            }
        }

        fn deflated(name: &'a [u8]) -> Self {
            Self {
                local_name: name,
                central_name: name,
                local_extra: &[],
                central_extra: &[],
                data: ZIP_FIXTURE_DATA,
                local_method: 8,
                central_method: 8,
                local_flags: 0,
                central_flags: 0,
                data_descriptor_has_signature: true,
            }
        }
    }

    fn push_zip_u16(output: &mut Vec<u8>, value: u16) {
        output.extend_from_slice(&value.to_le_bytes());
    }

    fn push_zip_u32(output: &mut Vec<u8>, value: u32) {
        output.extend_from_slice(&value.to_le_bytes());
    }

    fn zip_fixture_crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
            }
        }
        !crc
    }

    fn zip_fixture_encoded_data<'a>(entry: ZipEntryFixture<'a>) -> &'a [u8] {
        if entry.local_method == 8 && entry.data == ZIP_FIXTURE_DATA {
            ZIP_FIXTURE_DEFLATED_DATA
        } else {
            entry.data
        }
    }

    fn zip_fixture(entries: &[ZipEntryFixture<'_>]) -> Vec<u8> {
        let mut output = Vec::new();
        let mut local_offsets = Vec::with_capacity(entries.len());

        for entry in entries {
            let encoded = zip_fixture_encoded_data(*entry);
            let crc = zip_fixture_crc32(entry.data);
            let uses_descriptor = entry.local_flags & (1 << 3) != 0;
            local_offsets.push(output.len() as u32);

            push_zip_u32(&mut output, 0x0403_4b50);
            push_zip_u16(&mut output, 20);
            push_zip_u16(&mut output, entry.local_flags);
            push_zip_u16(&mut output, entry.local_method);
            push_zip_u16(&mut output, 0);
            push_zip_u16(&mut output, 0);
            push_zip_u32(&mut output, if uses_descriptor { 0 } else { crc });
            push_zip_u32(
                &mut output,
                if uses_descriptor {
                    0
                } else {
                    encoded.len() as u32
                },
            );
            push_zip_u32(
                &mut output,
                if uses_descriptor {
                    0
                } else {
                    entry.data.len() as u32
                },
            );
            push_zip_u16(&mut output, entry.local_name.len() as u16);
            push_zip_u16(&mut output, entry.local_extra.len() as u16);
            output.extend_from_slice(entry.local_name);
            output.extend_from_slice(entry.local_extra);
            output.extend_from_slice(encoded);

            if uses_descriptor {
                if entry.data_descriptor_has_signature {
                    push_zip_u32(&mut output, 0x0807_4b50);
                }
                push_zip_u32(&mut output, crc);
                push_zip_u32(&mut output, encoded.len() as u32);
                push_zip_u32(&mut output, entry.data.len() as u32);
            }
        }

        let central_offset = output.len() as u32;
        for (entry, local_offset) in entries.iter().zip(local_offsets) {
            let encoded = zip_fixture_encoded_data(*entry);
            push_zip_u32(&mut output, 0x0201_4b50);
            push_zip_u16(&mut output, 20);
            push_zip_u16(&mut output, 20);
            push_zip_u16(&mut output, entry.central_flags);
            push_zip_u16(&mut output, entry.central_method);
            push_zip_u16(&mut output, 0);
            push_zip_u16(&mut output, 0);
            push_zip_u32(&mut output, zip_fixture_crc32(entry.data));
            push_zip_u32(&mut output, encoded.len() as u32);
            push_zip_u32(&mut output, entry.data.len() as u32);
            push_zip_u16(&mut output, entry.central_name.len() as u16);
            push_zip_u16(&mut output, entry.central_extra.len() as u16);
            push_zip_u16(&mut output, 0);
            push_zip_u16(&mut output, 0);
            push_zip_u16(&mut output, 0);
            push_zip_u32(&mut output, 0);
            push_zip_u32(&mut output, local_offset);
            output.extend_from_slice(entry.central_name);
            output.extend_from_slice(entry.central_extra);
        }

        let central_size = output.len() as u32 - central_offset;
        push_zip_u32(&mut output, 0x0605_4b50);
        push_zip_u16(&mut output, 0);
        push_zip_u16(&mut output, 0);
        push_zip_u16(&mut output, entries.len() as u16);
        push_zip_u16(&mut output, entries.len() as u16);
        push_zip_u32(&mut output, central_size);
        push_zip_u32(&mut output, central_offset);
        push_zip_u16(&mut output, 0);
        output
    }

    fn zip_fixture_central_start(bytes: &[u8]) -> usize {
        let eocd_start = bytes.len() - 22;
        usize::try_from(u32::from_le_bytes(
            bytes[eocd_start + 16..eocd_start + 20].try_into().unwrap(),
        ))
        .unwrap()
    }

    fn zip_fixture_first_local_data_start(bytes: &[u8]) -> usize {
        let name_len = usize::from(u16::from_le_bytes(bytes[26..28].try_into().unwrap()));
        let extra_len = usize::from(u16::from_le_bytes(bytes[28..30].try_into().unwrap()));
        ZIP_LOCAL_FIXED_HEADER_LEN + name_len + extra_len
    }

    fn zip_fixture_write_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn zip_fixture_insert_bytes(bytes: &mut Vec<u8>, offset: usize, inserted: &[u8]) {
        let central_start = zip_fixture_central_start(bytes);
        assert!(offset <= central_start);
        let eocd_start = bytes.len() - 22;
        let mut central_offset = central_start;
        while bytes[central_offset..central_offset + 4] == ZIP_CENTRAL_HEADER_SIGNATURE {
            let name_len = usize::from(u16::from_le_bytes(
                bytes[central_offset + ZIP_CENTRAL_NAME_LEN_OFFSET
                    ..central_offset + ZIP_CENTRAL_NAME_LEN_OFFSET + 2]
                    .try_into()
                    .unwrap(),
            ));
            let extra_len = usize::from(u16::from_le_bytes(
                bytes[central_offset + ZIP_CENTRAL_EXTRA_LEN_OFFSET
                    ..central_offset + ZIP_CENTRAL_EXTRA_LEN_OFFSET + 2]
                    .try_into()
                    .unwrap(),
            ));
            let comment_len = usize::from(u16::from_le_bytes(
                bytes[central_offset + ZIP_CENTRAL_COMMENT_LEN_OFFSET
                    ..central_offset + ZIP_CENTRAL_COMMENT_LEN_OFFSET + 2]
                    .try_into()
                    .unwrap(),
            ));
            let local_offset = usize::try_from(u32::from_le_bytes(
                bytes[central_offset + 42..central_offset + 46]
                    .try_into()
                    .unwrap(),
            ))
            .unwrap();
            if local_offset >= offset {
                let adjusted = u32::try_from(local_offset + inserted.len()).unwrap();
                bytes[central_offset + 42..central_offset + 46]
                    .copy_from_slice(&adjusted.to_le_bytes());
            }
            central_offset += ZIP_CENTRAL_FIXED_HEADER_LEN + name_len + extra_len + comment_len;
        }
        assert_eq!(central_offset, eocd_start);
        let adjusted_central_start = u32::try_from(central_start + inserted.len()).unwrap();
        bytes[eocd_start + 16..eocd_start + 20]
            .copy_from_slice(&adjusted_central_start.to_le_bytes());
        bytes.splice(offset..offset, inserted.iter().copied());
    }

    fn zip_fixture_append_deflate_junk(bytes: &mut Vec<u8>, junk: &[u8]) {
        let data_start = zip_fixture_first_local_data_start(bytes);
        let central_start = zip_fixture_central_start(bytes);
        let compressed_size = usize::try_from(u32::from_le_bytes(
            bytes[central_start + 20..central_start + 24]
                .try_into()
                .unwrap(),
        ))
        .unwrap();
        let data_end = data_start + compressed_size;
        let uses_descriptor =
            u16::from_le_bytes(bytes[6..8].try_into().unwrap()) & ZIP_FLAG_DATA_DESCRIPTOR != 0;

        zip_fixture_insert_bytes(bytes, data_end, junk);

        let compressed_size = u32::try_from(compressed_size + junk.len()).unwrap();
        let central_start = zip_fixture_central_start(bytes);
        zip_fixture_write_u32(bytes, central_start + 20, compressed_size);
        if uses_descriptor {
            let descriptor_start = data_end + junk.len();
            let payload_start =
                if bytes[descriptor_start..descriptor_start + 4] == ZIP_DATA_DESCRIPTOR_SIGNATURE {
                    descriptor_start + 4
                } else {
                    descriptor_start
                };
            zip_fixture_write_u32(bytes, payload_start + 4, compressed_size);
        } else {
            zip_fixture_write_u32(bytes, ZIP_LOCAL_COMPRESSED_SIZE_OFFSET, compressed_size);
        }
    }

    fn zip_fixture_local_record(entry: ZipEntryFixture<'_>) -> Vec<u8> {
        let bytes = zip_fixture(&[entry]);
        bytes[..zip_fixture_central_start(&bytes)].to_vec()
    }

    fn duplicate_physical_name_zip_fixture() -> Vec<u8> {
        zip_fixture(&[
            ZipEntryFixture {
                local_name: b"../page.jpg",
                central_name: b"page.jpg",
                ..ZipEntryFixture::stored(b"page.jpg")
            },
            ZipEntryFixture::stored(b"page.jpg"),
        ])
    }

    #[test]
    fn ipfs3_zip_preflight_accepts_stored_deflate_and_safe_prefixes() {
        let bytes = zip_fixture(&[
            ZipEntryFixture::stored(b"page001.jpg"),
            ZipEntryFixture::deflated(b"dir/page002.png"),
            ZipEntryFixture::stored(b"metadata/"),
        ]);
        let requested = vec!["page001.jpg".to_string(), "dir/page002.png".to_string()];

        assert!(ipfs3_zip_archive_is_compatible(
            &bytes,
            &requested,
            "eh/archive/"
        ));
    }

    #[test]
    fn ipfs3_zip_preflight_accepts_deflate_with_data_descriptor() {
        let signed_descriptor = zip_fixture(&[ZipEntryFixture {
            local_flags: 1 << 3,
            central_flags: 1 << 3,
            ..ZipEntryFixture::deflated(b"page.jpg")
        }]);
        let unsigned_descriptor = zip_fixture(&[ZipEntryFixture {
            local_flags: 1 << 3,
            central_flags: 1 << 3,
            data_descriptor_has_signature: false,
            ..ZipEntryFixture::deflated(b"page.jpg")
        }]);

        for bytes in [signed_descriptor, unsigned_descriptor] {
            assert!(ipfs3_zip_archive_is_compatible(
                &bytes,
                &["page.jpg".to_string()],
                "eh/archive/"
            ));
        }
    }

    #[test]
    fn ipfs3_zip_preflight_rejects_trailing_junk_after_deflate_stream() {
        let requested = ["page.jpg".to_string()];
        let entries = [
            ("no descriptor", ZipEntryFixture::deflated(b"page.jpg")),
            (
                "signed descriptor",
                ZipEntryFixture {
                    local_flags: ZIP_FLAG_DATA_DESCRIPTOR,
                    central_flags: ZIP_FLAG_DATA_DESCRIPTOR,
                    ..ZipEntryFixture::deflated(b"page.jpg")
                },
            ),
            (
                "unsigned descriptor",
                ZipEntryFixture {
                    local_flags: ZIP_FLAG_DATA_DESCRIPTOR,
                    central_flags: ZIP_FLAG_DATA_DESCRIPTOR,
                    data_descriptor_has_signature: false,
                    ..ZipEntryFixture::deflated(b"page.jpg")
                },
            ),
        ];

        for (case, entry) in entries {
            let mut bytes = zip_fixture(&[entry]);
            zip_fixture_append_deflate_junk(&mut bytes, b"junk");

            assert!(
                !ipfs3_zip_archive_is_compatible(&bytes, &requested, "eh/archive/"),
                "{case}"
            );
        }
    }

    #[test]
    fn ipfs3_zip_preflight_preserves_unsigned_descriptor_signature_ambiguity() {
        let mut bytes = zip_fixture(&[ZipEntryFixture {
            local_flags: ZIP_FLAG_DATA_DESCRIPTOR,
            central_flags: ZIP_FLAG_DATA_DESCRIPTOR,
            data_descriptor_has_signature: false,
            ..ZipEntryFixture::deflated(b"page.jpg")
        }]);
        let ambiguous_crc32 = u32::from_le_bytes(ZIP_DATA_DESCRIPTOR_SIGNATURE);
        let descriptor_start =
            zip_fixture_first_local_data_start(&bytes) + ZIP_FIXTURE_DEFLATED_DATA.len();
        let central_crc32_offset = zip_fixture_central_start(&bytes) + ZIP_CENTRAL_CRC32_OFFSET;
        zip_fixture_write_u32(&mut bytes, central_crc32_offset, ambiguous_crc32);
        zip_fixture_write_u32(&mut bytes, descriptor_start, ambiguous_crc32);

        assert!(!ipfs3_zip_archive_is_compatible(
            &bytes,
            &["page.jpg".to_string()],
            "eh/archive/"
        ));
    }

    #[test]
    fn ipfs3_zip_preflight_rejects_local_stream_metadata_mismatches() {
        let requested = ["page.jpg".to_string()];

        let mut local_compressed_size = zip_fixture(&[ZipEntryFixture::stored(b"page.jpg")]);
        zip_fixture_write_u32(&mut local_compressed_size, 18, 4);

        let mut local_crc32 = zip_fixture(&[ZipEntryFixture::stored(b"page.jpg")]);
        zip_fixture_write_u32(&mut local_crc32, 14, 0);

        let mut local_uncompressed_size = zip_fixture(&[ZipEntryFixture::stored(b"page.jpg")]);
        zip_fixture_write_u32(&mut local_uncompressed_size, 22, 4);

        let mut descriptor_crc32 = zip_fixture(&[ZipEntryFixture {
            local_flags: ZIP_FLAG_DATA_DESCRIPTOR,
            central_flags: ZIP_FLAG_DATA_DESCRIPTOR,
            ..ZipEntryFixture::deflated(b"page.jpg")
        }]);
        let descriptor_start =
            zip_fixture_first_local_data_start(&descriptor_crc32) + ZIP_FIXTURE_DEFLATED_DATA.len();
        zip_fixture_write_u32(&mut descriptor_crc32, descriptor_start + 4, 0);

        let mut descriptor_compressed_size = zip_fixture(&[ZipEntryFixture {
            local_flags: ZIP_FLAG_DATA_DESCRIPTOR,
            central_flags: ZIP_FLAG_DATA_DESCRIPTOR,
            ..ZipEntryFixture::deflated(b"page.jpg")
        }]);
        let descriptor_start = zip_fixture_first_local_data_start(&descriptor_compressed_size)
            + ZIP_FIXTURE_DEFLATED_DATA.len();
        zip_fixture_write_u32(&mut descriptor_compressed_size, descriptor_start + 8, 0);

        for (case, bytes) in [
            ("local compressed size", local_compressed_size),
            ("local CRC32", local_crc32),
            ("local uncompressed size", local_uncompressed_size),
            ("descriptor CRC32", descriptor_crc32),
            ("descriptor compressed size", descriptor_compressed_size),
        ] {
            assert!(
                !ipfs3_zip_archive_is_compatible(&bytes, &requested, "eh/archive/"),
                "{case}"
            );
        }
    }

    #[test]
    fn ipfs3_zip_preflight_rejects_incompatible_entries_and_prefixes() {
        let unsafe_names: &[&[u8]] = &[
            b"",
            b"/",
            b"/absolute.jpg",
            b"C:/drive.jpg",
            b"dir\\page.jpg",
            b"a/./page.jpg",
            b"a/../page.jpg",
            b"\xff.jpg",
        ];
        for name in unsafe_names {
            let bytes = zip_fixture(&[ZipEntryFixture::stored(name)]);
            assert!(!ipfs3_zip_archive_is_compatible(
                &bytes,
                &["page.jpg".to_string()],
                "eh/archive/"
            ));
        }

        let requested = vec!["page.jpg".to_string()];
        let encrypted = zip_fixture(&[ZipEntryFixture {
            central_flags: 1,
            ..ZipEntryFixture::stored(b"page.jpg")
        }]);
        let unsupported = zip_fixture(&[ZipEntryFixture {
            local_method: 12,
            central_method: 12,
            ..ZipEntryFixture::stored(b"page.jpg")
        }]);
        let mismatched = zip_fixture(&[ZipEntryFixture {
            local_method: 8,
            central_method: 0,
            ..ZipEntryFixture::stored(b"page.jpg")
        }]);
        let stored_descriptor = zip_fixture(&[ZipEntryFixture {
            local_flags: 1 << 3,
            central_flags: 1 << 3,
            ..ZipEntryFixture::stored(b"page.jpg")
        }]);
        let descriptor_flag_mismatch = zip_fixture(&[ZipEntryFixture {
            local_flags: 1 << 3,
            ..ZipEntryFixture::deflated(b"page.jpg")
        }]);
        let unsafe_unrequested = zip_fixture(&[
            ZipEntryFixture::stored(b"page.jpg"),
            ZipEntryFixture::stored(b"../notes.txt"),
        ]);

        for bytes in [
            encrypted,
            unsupported,
            mismatched,
            stored_descriptor,
            descriptor_flag_mismatch,
            unsafe_unrequested,
        ] {
            assert!(!ipfs3_zip_archive_is_compatible(
                &bytes,
                &requested,
                "eh/archive/"
            ));
        }

        let valid = zip_fixture(&[ZipEntryFixture::stored(b"page.jpg")]);
        for prefix in [
            "/absolute/",
            "C:/drive/",
            "dir\\prefix/",
            "a/./",
            "a/../",
            "/",
        ] {
            assert!(!ipfs3_zip_archive_is_compatible(&valid, &requested, prefix));
        }
        assert!(!ipfs3_zip_archive_is_compatible(
            &valid,
            &["page.jpg".to_string(), "page.jpg".to_string()],
            "eh/archive/"
        ));
    }

    #[test]
    fn ipfs3_zip_preflight_rejects_incompatible_local_names_and_flags() {
        let requested = vec!["page.jpg".to_string()];
        let entries = [
            ZipEntryFixture {
                local_name: b"\xff.jpg",
                ..ZipEntryFixture::stored(b"page.jpg")
            },
            ZipEntryFixture {
                local_name: b"../page.jpg",
                ..ZipEntryFixture::stored(b"page.jpg")
            },
            ZipEntryFixture {
                local_name: b"dir\\page.jpg",
                ..ZipEntryFixture::stored(b"page.jpg")
            },
            ZipEntryFixture {
                local_name: b"other.jpg",
                ..ZipEntryFixture::stored(b"page.jpg")
            },
            ZipEntryFixture {
                local_flags: 1,
                ..ZipEntryFixture::stored(b"page.jpg")
            },
        ];

        for entry in entries {
            let bytes = zip_fixture(&[entry]);
            assert!(!ipfs3_zip_archive_is_compatible(
                &bytes,
                &requested,
                "eh/archive/"
            ));
        }
    }

    #[test]
    fn ipfs3_zip_preflight_rejects_non_utf8_flagged_names_and_unicode_path_extra() {
        let requested = vec!["page.jpg".to_string()];
        let non_ascii_without_utf8_flag =
            zip_fixture(&[ZipEntryFixture::stored(b"\xe2\x98\x83.jpg")]);
        let unicode_path_extra = zip_fixture(&[ZipEntryFixture {
            local_extra: b"\x75\x70\x10\x00\x01\x00\x00\x00\x00../page.jpg",
            ..ZipEntryFixture::stored(b"page.jpg")
        }]);
        let malformed_extra = zip_fixture(&[ZipEntryFixture {
            local_extra: b"\x01\x00\x02",
            ..ZipEntryFixture::stored(b"page.jpg")
        }]);

        for (case, bytes) in [
            (
                "non-ASCII name without UTF-8 flag",
                non_ascii_without_utf8_flag,
            ),
            ("Unicode Path extra", unicode_path_extra),
            ("malformed local extra", malformed_extra),
        ] {
            assert!(
                !ipfs3_zip_archive_is_compatible(&bytes, &requested, "eh/archive/"),
                "{case}"
            );
        }
    }

    #[test]
    fn ipfs3_zip_preflight_validates_unicode_comment_local_extra() {
        let requested = vec!["page.jpg".to_string()];

        for (case, local_extra) in [
            (
                "empty Unicode Comment extra",
                b"\x75\x63\x00\x00".as_slice(),
            ),
            (
                "truncated V1 Unicode Comment extra",
                b"\x75\x63\x01\x00\x01".as_slice(),
            ),
        ] {
            let bytes = zip_fixture(&[ZipEntryFixture {
                local_extra,
                ..ZipEntryFixture::stored(b"page.jpg")
            }]);
            assert!(
                !ipfs3_zip_archive_is_compatible(&bytes, &requested, "eh/archive/"),
                "{case}"
            );
        }

        for (case, local_extra) in [
            (
                "unknown Unicode Comment version",
                b"\x75\x63\x01\x00\x02".as_slice(),
            ),
            (
                "complete V1 Unicode Comment extra",
                b"\x75\x63\x05\x00\x01\x00\x00\x00\x00".as_slice(),
            ),
        ] {
            let bytes = zip_fixture(&[ZipEntryFixture {
                local_extra,
                ..ZipEntryFixture::stored(b"page.jpg")
            }]);
            assert!(
                ipfs3_zip_archive_is_compatible(&bytes, &requested, "eh/archive/"),
                "{case}"
            );
        }
    }

    #[test]
    fn ipfs3_zip_preflight_rejects_central_unicode_path_name_override() {
        let mut unicode_path_extra = b"\x75\x70\x10\x00\x01".to_vec();
        unicode_path_extra.extend_from_slice(&zip_fixture_crc32(b"page.jpg").to_le_bytes());
        unicode_path_extra.extend_from_slice(b"renamed.jpg");
        let bytes = zip_fixture(&[ZipEntryFixture {
            central_extra: &unicode_path_extra,
            ..ZipEntryFixture::stored(b"page.jpg")
        }]);
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes.as_slice())).unwrap();
        let central_directory_start = archive.central_directory_start();
        let archive_len = archive.len();
        let file = archive.by_index_raw(0).unwrap();

        assert_eq!(file.name_raw(), b"renamed.jpg");
        assert_eq!(file.name(), "renamed.jpg");
        let central_entries =
            ipfs3_zip_central_directory_entries(&bytes, central_directory_start, archive_len)
                .unwrap();
        assert_eq!(central_entries[0].raw_name, b"page.jpg");
        assert!(!ipfs3_zip_archive_is_compatible(
            &bytes,
            &["page.jpg".to_string()],
            "eh/archive/"
        ));
    }

    #[test]
    fn ipfs3_zip_preflight_rejects_non_contiguous_or_aliased_local_records() {
        let requested = vec!["one.jpg".to_string()];

        let mut prefixed = zip_fixture(&[ZipEntryFixture::stored(b"one.jpg")]);
        zip_fixture_insert_bytes(&mut prefixed, 0, b"prefix");

        let mut gapped = zip_fixture(&[
            ZipEntryFixture::stored(b"one.jpg"),
            ZipEntryFixture::stored(b"two.jpg"),
        ]);
        let first_record_end =
            ZIP_LOCAL_FIXED_HEADER_LEN + b"one.jpg".len() + ZIP_FIXTURE_DATA.len();
        zip_fixture_insert_bytes(&mut gapped, first_record_end, b"gap");

        let mut orphaned = zip_fixture(&[ZipEntryFixture::stored(b"one.jpg")]);
        let orphan_start = zip_fixture_central_start(&orphaned);
        let orphan = zip_fixture_local_record(ZipEntryFixture::stored(b"orphan.jpg"));
        zip_fixture_insert_bytes(&mut orphaned, orphan_start, &orphan);

        let mut aliased = zip_fixture(&[
            ZipEntryFixture::stored(b"one.jpg"),
            ZipEntryFixture::stored(b"two.jpg"),
        ]);
        let second_central =
            zip_fixture_central_start(&aliased) + ZIP_CENTRAL_FIXED_HEADER_LEN + b"one.jpg".len();
        aliased[second_central + 42..second_central + 46].copy_from_slice(&0u32.to_le_bytes());

        for (case, bytes) in [
            ("prefix", prefixed),
            ("gap", gapped),
            ("orphan", orphaned),
            ("aliased", aliased),
        ] {
            assert!(
                !ipfs3_zip_archive_is_compatible(&bytes, &requested, "eh/archive/"),
                "{case}"
            );
        }
    }

    #[test]
    fn ipfs3_zip_preflight_rejects_duplicate_physical_central_names() {
        let bytes = duplicate_physical_name_zip_fixture();

        assert!(!ipfs3_zip_archive_is_compatible(
            &bytes,
            &["page.jpg".to_string()],
            "eh/archive/"
        ));
    }

    #[test]
    fn ipfs3_zip_central_scan_rejects_early_stop_before_archive_len() {
        let mut bytes = zip_fixture(&[
            ZipEntryFixture::stored(b"one.jpg"),
            ZipEntryFixture::stored(b"two.jpg"),
        ]);
        let archive = zip::ZipArchive::new(std::io::Cursor::new(bytes.as_slice())).unwrap();
        let central_start = archive.central_directory_start();
        let archive_len = archive.len();
        drop(archive);
        let second_record_start = usize::try_from(central_start).unwrap()
            + ZIP_CENTRAL_FIXED_HEADER_LEN
            + b"one.jpg".len();
        bytes[second_record_start..second_record_start + 4].copy_from_slice(b"STOP");

        assert!(!ipfs3_zip_central_directory_is_complete_and_unique(
            &bytes,
            central_start,
            archive_len,
        ));
    }

    #[tokio::test]
    async fn ipfs3_zip_archive_upload_preflight_fallback_sends_no_put() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let mut config = complete_ipfs3_config(&server.uri(), "https://public.example/ipfs/");
        config.zip_extract_enabled = true;
        let uploader = IpfS3Uploader::from_config(&config).unwrap();
        let zip_bytes = zip_fixture(&[ZipEntryFixture::stored(b"dir\\page.jpg")]);
        let entries = vec!["dir\\page.jpg".to_string()];

        let result = uploader
            .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
                filename: "gallery.zip",
                bytes: &zip_bytes,
                entry_names: &entries,
            })
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn ipfs3_zip_archive_upload_truncated_unicode_comment_sends_no_put() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let mut config = complete_ipfs3_config(&server.uri(), "https://public.example/ipfs/");
        config.zip_extract_enabled = true;
        let uploader = IpfS3Uploader::from_config(&config).unwrap();
        let zip_bytes = zip_fixture(&[ZipEntryFixture {
            local_extra: b"\x75\x63\x01\x00\x01",
            ..ZipEntryFixture::stored(b"page.jpg")
        }]);
        let entries = vec!["page.jpg".to_string()];

        let result = uploader
            .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
                filename: "gallery.zip",
                bytes: &zip_bytes,
                entry_names: &entries,
            })
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn ipfs3_zip_archive_upload_duplicate_physical_name_sends_no_put() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let mut config = complete_ipfs3_config(&server.uri(), "https://public.example/ipfs/");
        config.zip_extract_enabled = true;
        let uploader = IpfS3Uploader::from_config(&config).unwrap();
        let zip_bytes = duplicate_physical_name_zip_fixture();
        let entries = vec!["page.jpg".to_string()];

        let result = uploader
            .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
                filename: "gallery.zip",
                bytes: &zip_bytes,
                entry_names: &entries,
            })
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn ipfs3_zip_archive_upload_orphan_local_record_sends_no_put() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let mut config = complete_ipfs3_config(&server.uri(), "https://public.example/ipfs/");
        config.zip_extract_enabled = true;
        let uploader = IpfS3Uploader::from_config(&config).unwrap();
        let mut zip_bytes = zip_fixture(&[ZipEntryFixture::stored(b"page.jpg")]);
        let orphan_start = zip_fixture_central_start(&zip_bytes);
        let orphan = zip_fixture_local_record(ZipEntryFixture::stored(b"orphan.jpg"));
        zip_fixture_insert_bytes(&mut zip_bytes, orphan_start, &orphan);
        let entries = vec!["page.jpg".to_string()];

        let result = uploader
            .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
                filename: "gallery.zip",
                bytes: &zip_bytes,
                entry_names: &entries,
            })
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn ipfs3_zip_archive_upload_deflate_trailing_junk_sends_no_put() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let mut config = complete_ipfs3_config(&server.uri(), "https://public.example/ipfs/");
        config.zip_extract_enabled = true;
        let uploader = IpfS3Uploader::from_config(&config).unwrap();
        let mut zip_bytes = zip_fixture(&[ZipEntryFixture {
            local_flags: ZIP_FLAG_DATA_DESCRIPTOR,
            central_flags: ZIP_FLAG_DATA_DESCRIPTOR,
            ..ZipEntryFixture::deflated(b"page.jpg")
        }]);
        zip_fixture_append_deflate_junk(&mut zip_bytes, b"junk");
        let entries = vec!["page.jpg".to_string()];

        let result = uploader
            .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
                filename: "gallery.zip",
                bytes: &zip_bytes,
                entry_names: &entries,
            })
            .await
            .unwrap();

        assert!(result.is_none());
    }

    fn ipfs3_zip_extract_result_xml(
        entries: &[(&str, &str)],
        failures: &[(&str, &str, &str)],
        extracted_count: usize,
        failed_count: usize,
    ) -> Vec<u8> {
        let entries = entries
            .iter()
            .map(|(key, etag)| {
                format!("<Entry><Key>{key}</Key><ETag>{etag}</ETag><Size>123</Size></Entry>")
            })
            .collect::<String>();
        let failures = failures
            .iter()
            .map(|(entry_name, code, message)| {
                format!(
                    "<Failure><EntryName>{entry_name}</EntryName><Code>{code}</Code><Message>{message}</Message></Failure>"
                )
            })
            .collect::<String>();

        format!(
            "<DecompressZipResult><ArchiveKey>archive.zip</ArchiveKey><ArchiveETag>archive-cid</ArchiveETag><ArchiveSize>456</ArchiveSize><ExtractedCount>{extracted_count}</ExtractedCount><FailedCount>{failed_count}</FailedCount><Entries>{entries}</Entries><Failures>{failures}</Failures></DecompressZipResult>"
        )
        .into_bytes()
    }

    struct IpfS3ZipExtractResponder;

    impl wiremock::Respond for IpfS3ZipExtractResponder {
        fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
            let archive_key = request
                .url
                .path()
                .strip_prefix("/bucket/")
                .expect("ZIP extraction request must target the configured bucket");
            let archive_stem = archive_key
                .strip_suffix(".zip")
                .expect("ZIP extraction request must target a ZIP object");
            let entries = [
                (format!("{archive_stem}/notes.txt"), "bafyExtra"),
                (format!("{archive_stem}/dir/page002.png"), "bafySecond"),
                (format!("{archive_stem}/page001.jpg"), "bafyFirst"),
            ];
            let entries = entries
                .iter()
                .map(|(key, cid)| (key.as_str(), *cid))
                .collect::<Vec<_>>();

            wiremock::ResponseTemplate::new(200)
                .insert_header("etag", "\"bafyHttpArchiveCidMustNotAppear\"")
                .set_body_bytes(ipfs3_zip_extract_result_xml(&entries, &[], 3, 0))
        }
    }

    #[test]
    fn ipfs3_zip_extract_result_maps_exact_keys_in_requested_order_and_last_key_wins() {
        let xml = ipfs3_zip_extract_result_xml(
            &[
                ("extract/page-001.jpg", "cid-first"),
                ("extract/page-002.jpg", "cid-two"),
                ("extract/page-001.jpg", "cid-last"),
                ("extract/unrequested.jpg", "cid-extra"),
            ],
            &[],
            4,
            0,
        );
        let xml = [
            b"<?xml version=\"1.0\"?>\n ".as_slice(),
            xml.as_slice(),
            b"\n\t",
        ]
        .concat();
        let result = parse_ipfs3_zip_extract_result(&xml).unwrap();

        let cids = ipfs3_zip_entry_cids(
            "extract/",
            &["page-002.jpg".to_string(), "page-001.jpg".to_string()],
            result,
        )
        .unwrap()
        .unwrap();

        assert_eq!(cids, ["cid-two", "cid-last"]);
    }

    #[test]
    fn ipfs3_zip_extract_result_rejects_empty_and_malformed_xml() {
        let err = parse_ipfs3_zip_extract_result(b" \n\t ").unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid DecompressZipResult XML: empty response body"
        );

        let err = parse_ipfs3_zip_extract_result(b"<DecompressZipResult>").unwrap_err();
        assert!(err
            .to_string()
            .starts_with("invalid DecompressZipResult XML: "));
    }

    #[test]
    fn ipfs3_zip_extract_result_rejects_wrong_root_element() {
        let xml = String::from_utf8(ipfs3_zip_extract_result_xml(&[], &[], 0, 0))
            .unwrap()
            .replace("DecompressZipResult", "NotDecompressZipResult");

        let err = parse_ipfs3_zip_extract_result(xml.as_bytes()).unwrap_err();

        assert!(err
            .to_string()
            .contains("expected DecompressZipResult root element"));
    }

    #[test]
    fn ipfs3_zip_extract_result_rejects_trailing_document_content() {
        let mut xml = ipfs3_zip_extract_result_xml(&[], &[], 0, 0);
        xml.extend_from_slice(b"<Unexpected/>");

        let err = parse_ipfs3_zip_extract_result(&xml).unwrap_err();
        assert!(err.to_string().contains("trailing XML content"));

        let mut xml = ipfs3_zip_extract_result_xml(&[], &[], 0, 0);
        xml.extend_from_slice(b"<broken");

        let err = parse_ipfs3_zip_extract_result(&xml).unwrap_err();
        assert!(err
            .to_string()
            .starts_with("invalid DecompressZipResult XML: "));
    }

    #[test]
    fn ipfs3_zip_extract_result_rejects_declared_count_inconsistencies() {
        let result = parse_ipfs3_zip_extract_result(&ipfs3_zip_extract_result_xml(
            &[("extract/page.jpg", "cid-page")],
            &[],
            2,
            0,
        ))
        .unwrap();
        let err = ipfs3_zip_entry_cids("extract/", &["page.jpg".to_string()], result).unwrap_err();
        assert!(err
            .to_string()
            .contains("ExtractedCount 2 does not match 1 entries"));

        let result = parse_ipfs3_zip_extract_result(&ipfs3_zip_extract_result_xml(
            &[("extract/page.jpg", "cid-page")],
            &[("page.jpg", "ExtractFailed", "bad archive")],
            1,
            0,
        ))
        .unwrap();
        let err = ipfs3_zip_entry_cids("extract/", &["page.jpg".to_string()], result).unwrap_err();
        assert!(err
            .to_string()
            .contains("FailedCount 0 does not match 1 failures"));
    }

    #[test]
    fn ipfs3_zip_extract_result_falls_back_when_requested_entry_failed() {
        let result = parse_ipfs3_zip_extract_result(&ipfs3_zip_extract_result_xml(
            &[("extract/page-002.jpg", "cid-page-two")],
            &[("page-001.jpg", "ExtractFailed", "bad archive")],
            1,
            1,
        ))
        .unwrap();

        let cids = ipfs3_zip_entry_cids(
            "extract/",
            &["page-001.jpg".to_string(), "page-002.jpg".to_string()],
            result,
        )
        .unwrap();

        assert!(cids.is_none());
    }

    #[test]
    fn ipfs3_zip_extract_result_ignores_unrequested_failure_when_requested_entries_succeed() {
        let result = parse_ipfs3_zip_extract_result(&ipfs3_zip_extract_result_xml(
            &[
                ("extract/page-001.jpg", "cid-page-one"),
                ("extract/page-002.jpg", "cid-page-two"),
            ],
            &[("unrequested.jpg", "ExtractFailed", "bad archive")],
            2,
            1,
        ))
        .unwrap();

        let cids = ipfs3_zip_entry_cids(
            "extract/",
            &["page-001.jpg".to_string(), "page-002.jpg".to_string()],
            result,
        )
        .unwrap()
        .unwrap();

        assert_eq!(cids, ["cid-page-one", "cid-page-two"]);
    }

    #[test]
    fn ipfs3_zip_extract_result_falls_back_when_requested_key_is_missing() {
        let result = parse_ipfs3_zip_extract_result(&ipfs3_zip_extract_result_xml(
            &[("extract/page.jpg", "cid-page")],
            &[],
            1,
            0,
        ))
        .unwrap();

        let cids = ipfs3_zip_entry_cids("extract/", &["missing.jpg".to_string()], result).unwrap();

        assert!(cids.is_none());
    }

    #[test]
    fn ipfs3_zip_extract_result_falls_back_when_requested_names_are_duplicate() {
        let result = parse_ipfs3_zip_extract_result(&ipfs3_zip_extract_result_xml(
            &[("extract/page.jpg", "cid-page")],
            &[],
            1,
            0,
        ))
        .unwrap();

        let cids = ipfs3_zip_entry_cids(
            "extract/",
            &["page.jpg".to_string(), "page.jpg".to_string()],
            result,
        )
        .unwrap();

        assert!(cids.is_none());
    }

    #[test]
    fn ipfs3_zip_extract_result_rejects_empty_requested_entry_cid() {
        let result = parse_ipfs3_zip_extract_result(&ipfs3_zip_extract_result_xml(
            &[("extract/page.jpg", " \"\" ")],
            &[],
            1,
            0,
        ))
        .unwrap();

        let err = ipfs3_zip_entry_cids("extract/", &["page.jpg".to_string()], result).unwrap_err();
        assert!(err
            .to_string()
            .contains("requested extraction key extract/page.jpg returned an empty CID"));
    }

    #[test]
    fn ipfs3_zip_extract_result_rejects_empty_unrequested_entry_cid() {
        let result = parse_ipfs3_zip_extract_result(&ipfs3_zip_extract_result_xml(
            &[
                ("extract/page.jpg", "cid-page"),
                ("extract/notes.txt", " \"\" "),
            ],
            &[],
            2,
            0,
        ))
        .unwrap();

        let err = ipfs3_zip_entry_cids("extract/", &["page.jpg".to_string()], result).unwrap_err();
        assert!(err
            .to_string()
            .contains("extraction key extract/notes.txt returned an empty CID"));
    }

    struct DefaultZipCapabilityUploader;

    #[async_trait::async_trait]
    impl ImageUploader for DefaultZipCapabilityUploader {
        async fn upload_images(&self, _images: &[ImageUploadInput<'_>]) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn default_zip_archive_upload_capability_returns_none() {
        let uploader = DefaultZipCapabilityUploader;
        let entries = vec!["page001.jpg".to_string()];
        let zip_bytes = zip_fixture(&[ZipEntryFixture::stored(b"page001.jpg")]);
        let result = uploader
            .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
                filename: "gallery.zip",
                bytes: &zip_bytes,
                entry_names: &entries,
            })
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn ipfs3_zip_archive_upload_disabled_returns_none_without_put() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200).set_body_string("unexpected-cid"))
            .expect(0)
            .mount(&server)
            .await;

        let uploader = IpfS3Uploader::from_config(&IpfS3UploaderConfig {
            endpoint_url: Some(server.uri()),
            bucket: Some("bucket".into()),
            region: Some("auto".into()),
            access_key_id: Some("ak".into()),
            secret_access_key: Some("sk".into()),
            gateway_url: Some("https://public.example/ipfs".into()),
            zip_extract_enabled: false,
            path_style: true,
            ..Default::default()
        })
        .unwrap();
        let entries = vec!["page001.jpg".to_string()];
        let zip_bytes = zip_fixture(&[ZipEntryFixture::stored(b"page001.jpg")]);

        let result = uploader
            .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
                filename: "gallery.zip",
                bytes: &zip_bytes,
                entry_names: &entries,
            })
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn ipfs3_zip_archive_upload_signs_query_and_uses_ordered_entry_cids() {
        use wiremock::matchers::{body_bytes, method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let zip_bytes = zip_fixture(&[
            ZipEntryFixture::stored(b"page001.jpg"),
            ZipEntryFixture::deflated(b"dir/page002.png"),
            ZipEntryFixture::stored(b"notes.txt"),
        ]);
        Mock::given(method("PUT"))
            .and(path_regex(r"^/bucket/eh/\d{14}-archive-[0-9a-f]{8}\.zip$"))
            .and(body_bytes(zip_bytes.clone()))
            .respond_with(IpfS3ZipExtractResponder)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/bucket/eh/\d{14}-0001-[0-9a-f]{8}\.png$"))
            .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"bafyNormalImage\""))
            .expect(1)
            .mount(&server)
            .await;

        let mut config = complete_ipfs3_config(&server.uri(), "https://public.example/ipfs/");
        config.preview_gateway_url = Some("https://preview.example/ipfs/".to_string());
        config.zip_extract_enabled = true;
        let uploader = IpfS3Uploader::from_config(&config).unwrap();
        let entries = vec!["page001.jpg".to_string(), "dir/page002.png".to_string()];

        let pairs = uploader
            .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
                filename: "gallery.zip",
                bytes: &zip_bytes,
                entry_names: &entries,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            pairs,
            vec![
                TelegraphImageUrlPair {
                    preview_url: "https://preview.example/ipfs/bafyFirst".to_string(),
                    public_url: "https://public.example/ipfs/bafyFirst".to_string(),
                },
                TelegraphImageUrlPair {
                    preview_url: "https://preview.example/ipfs/bafySecond".to_string(),
                    public_url: "https://public.example/ipfs/bafySecond".to_string(),
                },
            ]
        );

        let normal_pairs = uploader
            .upload_images_with_url_pairs(&[ImageUploadInput {
                filename: "normal.png",
                bytes: b"\x89PNG\r\n\x1a\n",
            }])
            .await
            .unwrap();
        assert_eq!(
            normal_pairs,
            vec![TelegraphImageUrlPair {
                preview_url: "https://preview.example/ipfs/bafyNormalImage".to_string(),
                public_url: "https://public.example/ipfs/bafyNormalImage".to_string(),
            }]
        );

        let requests = server.received_requests().await.unwrap();
        let zip_request = requests
            .iter()
            .find(|request| request.url.path().ends_with(".zip"))
            .unwrap();
        let archive_stem = zip_request
            .url
            .path()
            .strip_prefix("/bucket/")
            .unwrap()
            .strip_suffix(".zip")
            .unwrap();
        assert_eq!(
            zip_request
                .url
                .query_pairs()
                .find(|(name, _)| name == "decompress-zip")
                .map(|(_, value)| value.into_owned()),
            Some(format!("{archive_stem}/"))
        );
        assert!(!zip_request
            .url
            .query_pairs()
            .any(|(name, _)| name == "decompress-zip-result"));
        assert_eq!(
            zip_request
                .headers
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap(),
            "application/zip"
        );
        assert!(zip_request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("AWS4-HMAC-SHA256 ")));

        let normal_request = requests
            .iter()
            .find(|request| request.url.path().ends_with(".png"))
            .unwrap();
        assert!(normal_request.url.query().is_none());
        assert!(!normal_request
            .url
            .query_pairs()
            .any(|(name, _)| name == "decompress-zip"));
        for url in pairs
            .iter()
            .chain(normal_pairs.iter())
            .flat_map(|pair| [&pair.preview_url, &pair.public_url])
        {
            assert!(!url.contains("bafyHttpArchiveCidMustNotAppear"));
            assert!(!url.contains("page001.jpg"));
            assert!(!url.contains("dir/page002.png"));
        }
    }

    #[tokio::test]
    async fn ipfs3_zip_archive_upload_rejects_malformed_xml() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let zip_bytes = zip_fixture(&[ZipEntryFixture::stored(b"page001.jpg")]);
        Mock::given(method("PUT"))
            .and(path_regex(r"^/bucket/eh/\d{14}-archive-[0-9a-f]{8}\.zip$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"bafyHttpArchiveCidMustNotAppear\"")
                    .set_body_string("<DecompressZipResult>"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let mut config = complete_ipfs3_config(&server.uri(), "https://public.example/ipfs/");
        config.zip_extract_enabled = true;
        let uploader = IpfS3Uploader::from_config(&config).unwrap();
        let entries = vec!["page001.jpg".to_string()];

        let err = uploader
            .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
                filename: "gallery.zip",
                bytes: &zip_bytes,
                entry_names: &entries,
            })
            .await
            .unwrap_err();

        assert!(err.to_string().contains("invalid DecompressZipResult XML"));
    }

    #[tokio::test]
    async fn ipfs3_zip_archive_upload_rejects_non_success_status() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let zip_bytes = zip_fixture(&[ZipEntryFixture::stored(b"page001.jpg")]);
        Mock::given(method("PUT"))
            .and(path_regex(r"^/bucket/eh/\d{14}-archive-[0-9a-f]{8}\.zip$"))
            .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
            .expect(1)
            .mount(&server)
            .await;

        let mut config = complete_ipfs3_config(&server.uri(), "https://public.example/ipfs/");
        config.zip_extract_enabled = true;
        let uploader = IpfS3Uploader::from_config(&config).unwrap();
        let entries = vec!["page001.jpg".to_string()];

        let err = uploader
            .upload_zip_archive_with_url_pairs(ZipArchiveUploadInput {
                filename: "gallery.zip",
                bytes: &zip_bytes,
                entry_names: &entries,
            })
            .await
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("ipfS3 ZIP put_object returned 503"));
    }
}
