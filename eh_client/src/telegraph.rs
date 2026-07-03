use crate::error::{Error, Result};
use async_trait::async_trait;
use s3::creds::Credentials;
use s3::{Bucket, Region};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// A Telegraph content node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attrs: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<serde_json::Value>>,
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
    #[serde(default)]
    pub key_prefix: String,
    #[serde(default = "default_s3_path_style")]
    pub path_style: bool,
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
            key_prefix: self.key_prefix.trim_matches('/').to_string(),
            path_style: self.path_style,
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
    key_prefix: String,
    path_style: bool,
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

#[async_trait]
pub trait ImageUploader: Send + Sync {
    async fn upload_images(&self, images: &[ImageUploadInput<'_>]) -> Result<Vec<String>>;
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
}

#[async_trait]
impl ImageUploader for IpfS3Uploader {
    async fn upload_images(&self, images: &[ImageUploadInput<'_>]) -> Result<Vec<String>> {
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
            // rust-s3's put_object path extracts the ETag header into the
            // response body (see response_data(etag=true)), so the IPFS CID
            // lives in `response.as_str()`, not in the headers map. The value
            // retains its surrounding double quotes, which we strip.
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
            urls.push(format!("{}/{cid}", self.config.gateway_url));
        }
        Ok(urls)
    }
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

    /// Create a gallery page from image URLs. Splits into multiple pages if needed.
    /// Returns the first page URL (with "Next Page" links to subsequent pages).
    ///
    /// If a page creation fails partway through, returns the last successfully
    /// created page URL (if any) instead of erroring, so the user can still
    /// access the partial gallery.
    pub async fn create_gallery_page(&self, title: &str, image_urls: &[String]) -> Result<String> {
        if image_urls.is_empty() {
            return Err(Error::Other("no images to upload".into()));
        }

        let chunks = split_for_pages(image_urls, MAX_PAGE_CONTENT_BYTES);
        if chunks.len() == 1 {
            let nodes: Vec<Node> = chunks[0].iter().map(|url| Node::img(url)).collect();
            return self.create_page(title, &nodes).await;
        }

        // Multi-page: create in reverse order, linking to the next page.
        // The first page (created last in the loop) gets the original title.
        let total_pages = chunks.len();
        let mut next_url: Option<String> = None;
        for (idx, chunk) in chunks.iter().rev().enumerate() {
            let mut nodes: Vec<Node> = Vec::new();
            for url in chunk {
                nodes.push(Node::img(url));
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
                Ok(url) => next_url = Some(url),
                Err(e) => {
                    // If we have at least one page created, return it instead of failing
                    if let Some(ref url) = next_url {
                        tracing::warn!(
                            "Telegraph page creation failed at index {}, returning last successful page: {}",
                            idx,
                            e
                        );
                        return Ok(url.clone());
                    }
                    return Err(e);
                }
            }
        }

        Ok(next_url.unwrap_or_else(|| image_urls[0].clone()))
    }
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
            key_prefix: "eh".to_string(),
            path_style: true,
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
}
