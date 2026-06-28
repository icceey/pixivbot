use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};

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

/// Split image URLs into chunks that fit within the content size limit.
pub fn split_for_pages(urls: &[String], max_bytes: usize) -> Vec<Vec<String>> {
    if urls.is_empty() {
        return vec![];
    }
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut current_size = 0;
    for url in urls {
        let node = Node::img(url);
        let node_size = serde_json::to_vec(&node).map(|v| v.len()).unwrap_or(100);
        if current_size + node_size > max_bytes && !current.is_empty() {
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

pub struct TelegraphClient {
    http: reqwest::Client,
    /// Image upload endpoint (pixi.mg).
    upload_url: String,
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
            upload_url,
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
        if images.is_empty() {
            return Ok(Vec::new());
        }
        if images.len() > 5 {
            return Err(Error::Other("pixi.mg max 5 files per upload".into()));
        }

        let mut form = reqwest::multipart::Form::new();
        for image_data in images {
            let content_type = detect_content_type(image_data);
            let ext = match content_type.as_str() {
                "image/jpeg" => "jpg",
                "image/png" => "png",
                "image/gif" => "gif",
                "image/webp" => "webp",
                _ => "bin",
            };
            let part = reqwest::multipart::Part::bytes(image_data.to_vec())
                .file_name(format!("image.{}", ext))
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

    /// Upload images with automatic 429 backoff. Retries up to `max_retries` times
    /// on HTTP 429, waiting exponentially longer each time (40s, 80s, 160s).
    /// Returns the uploaded URLs (all images in one batch).
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
                    let wait = retry_after_secs.unwrap_or_else(|| 40 * 2u64.pow(attempt));
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
            let url = self.create_page(&page_title, &nodes).await?;
            next_url = Some(url);
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
}
