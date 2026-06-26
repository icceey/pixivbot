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

#[derive(Debug, Deserialize, Default)]
struct UploadResult {
    #[serde(default)]
    src: Option<String>,
}

pub struct TelegraphClient {
    http: reqwest::Client,
    access_token: String,
}

impl TelegraphClient {
    pub fn new(access_token: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("failed to build telegraph http client"),
            access_token,
        }
    }

    /// Upload an image to Telegraph. Returns the full URL.
    pub async fn upload_image(&self, image_data: &[u8], filename: &str) -> Result<String> {
        let part = reqwest::multipart::Part::bytes(image_data.to_vec())
            .file_name(filename.to_string())
            .mime_str("image/jpeg")
            .map_err(|e| Error::Other(format!("mime error: {e}")))?;
        let form = reqwest::multipart::Form::new().part("file", part);

        let resp = self
            .http
            .post("https://telegra.ph/upload")
            .multipart(form)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api {
                message: format!("telegraph upload returned {}", status),
                status: status.as_u16(),
            });
        }

        let results: Vec<UploadResult> = resp.json().await?;
        if let Some(first) = results.first() {
            if let Some(ref src) = first.src {
                return Ok(format!("https://telegra.ph{}", src));
            }
        }
        Err(Error::Parse("telegraph upload returned no src".into()))
    }

    /// Create a Telegraph page. Returns the page URL.
    pub async fn create_page(&self, title: &str, content: &[Node]) -> Result<String> {
        let content_json = serde_json::to_value(content)?;
        let content_str = content_json.to_string();
        let form = vec![
            ("access_token", self.access_token.as_str()),
            ("title", title),
            ("content", content_str.as_str()),
            ("return_content", "false"),
        ];

        let resp = self
            .http
            .post("https://api.telegra.ph/createPage")
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

        // Multi-page: create in reverse order, linking to the next page
        let mut next_url: Option<String> = None;
        for chunk in chunks.iter().rev() {
            let mut nodes: Vec<Node> = Vec::new();
            if let Some(ref next) = next_url {
                nodes.push(Node::link(next, "Next Page →"));
            }
            for url in chunk {
                nodes.push(Node::img(url));
            }
            let page_title = if next_url.is_some() {
                format!("{} (continued)", title)
            } else {
                title.to_string()
            };
            let url = self.create_page(&page_title, &nodes).await?;
            next_url = Some(url);
        }

        // Return the last-created URL (which is the first page due to reverse order)
        Ok(next_url.unwrap_or_else(|| image_urls[0].clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_image_node() {
        let node = Node::img("https://telegra.ph/file/abc.jpg");
        assert_eq!(node.tag, "img");
        assert_eq!(
            node.attrs.unwrap()["src"],
            "https://telegra.ph/file/abc.jpg"
        );
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
            Node::img("https://telegra.ph/file/abc.jpg"),
            Node::img("https://telegra.ph/file/def.jpg"),
        ];
        let size = estimate_content_size(&nodes);
        // Each img node is roughly {"tag":"img","attrs":{"src":"url"}} ≈ 60-80 bytes
        assert!(size > 0);
    }

    #[test]
    fn test_split_image_urls_for_pages() {
        let urls: Vec<String> = (0..50)
            .map(|i| format!("https://telegra.ph/file/{}.jpg", i))
            .collect();
        let chunks = split_for_pages(&urls, 1024); // 1KB limit for testing
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| !c.is_empty()));
    }
}
