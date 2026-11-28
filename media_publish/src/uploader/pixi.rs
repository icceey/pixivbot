use super::{detect_content_type, extension_for_content_type, ImageUploadInput, ImageUploader};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde::Deserialize;

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
