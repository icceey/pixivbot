use crate::error::{Error, Result};
use crate::uploader::PixiUploader;
use serde::{Deserialize, Serialize};

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cid: Option<String>,
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
pub(crate) const MAX_PAGE_CONTENT_BYTES: usize = 60_000;

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
        self.create_gallery_page_from_urls(title, image_urls, true)
            .await
    }

    /// Create a complete gallery or return an error if any page cannot be created.
    pub async fn create_gallery_page_strict(
        &self,
        title: &str,
        image_urls: &[String],
    ) -> Result<String> {
        self.create_gallery_page_from_urls(title, image_urls, false)
            .await
    }

    async fn create_gallery_page_from_urls(
        &self,
        title: &str,
        image_urls: &[String],
        allow_partial: bool,
    ) -> Result<String> {
        let pairs: Vec<TelegraphImageUrlPair> = image_urls
            .iter()
            .map(|url| TelegraphImageUrlPair {
                preview_url: url.clone(),
                public_url: url.clone(),
                cid: None,
            })
            .collect();
        Ok(self
            .create_gallery_page_with_url_pairs_inner(title, &pairs, None, None, allow_partial)
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
        self.create_gallery_page_with_url_pairs_inner(
            title,
            image_urls,
            preview_gateway_url,
            public_gateway_url,
            true,
        )
        .await
    }

    async fn create_gallery_page_with_url_pairs_inner(
        &self,
        title: &str,
        image_urls: &[TelegraphImageUrlPair],
        preview_gateway_url: Option<&str>,
        public_gateway_url: Option<&str>,
        allow_partial: bool,
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
                    if allow_partial {
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
