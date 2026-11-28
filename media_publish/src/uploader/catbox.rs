use super::{
    detect_content_type, extension_for_upload, safe_upload_filename, validate_http_url,
    ImageUploadInput, ImageUploader,
};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde::Deserialize;

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
