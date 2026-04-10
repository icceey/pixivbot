use crate::engine_type::BooruEngineType;
use crate::error::{Error, Result};
use crate::models::*;
use std::time::Duration;

const DEFAULT_USER_AGENT: &str = "pixivbot/1.0 (booru_client)";

pub struct BooruClient {
    client: reqwest::Client,
    base_url: String,
    engine_type: BooruEngineType,
    api_key: Option<String>,
    username: Option<String>,
}

impl BooruClient {
    pub fn new(base_url: &str, engine_type: BooruEngineType) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            engine_type,
            api_key: None,
            username: None,
        })
    }

    pub fn with_auth(mut self, username: &str, api_key: &str) -> Self {
        self.username = Some(username.to_string());
        self.api_key = Some(api_key.to_string());
        self
    }

    pub fn engine_type(&self) -> BooruEngineType {
        self.engine_type
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn get_posts(&self, tags: &str, limit: u32, page: u32) -> Result<Vec<BooruPost>> {
        match self.engine_type {
            BooruEngineType::Moebooru => self.get_posts_moebooru(tags, limit, page).await,
            BooruEngineType::Danbooru => self.get_posts_danbooru(tags, limit, page).await,
            BooruEngineType::Gelbooru => self.get_posts_gelbooru(tags, limit, page).await,
        }
    }

    pub async fn get_pool(&self, pool_id: u64) -> Result<BooruPoolInfo> {
        match self.engine_type {
            BooruEngineType::Moebooru => self.get_pool_moebooru(pool_id).await,
            BooruEngineType::Danbooru => self.get_pool_danbooru(pool_id).await,
            BooruEngineType::Gelbooru => Err(Error::Api {
                message: "Gelbooru does not support pool queries".to_string(),
                status: 0,
            }),
        }
    }

    pub async fn get_pool_posts(
        &self,
        pool_id: u64,
        limit: u32,
        page: u32,
    ) -> Result<Vec<BooruPost>> {
        match self.engine_type {
            BooruEngineType::Moebooru => self.get_pool_posts_moebooru(pool_id, limit, page).await,
            BooruEngineType::Danbooru => {
                let tags = format!("pool:{pool_id}");
                self.get_posts_danbooru(&tags, limit, page).await
            }
            BooruEngineType::Gelbooru => {
                let tags = format!("pool:{pool_id}");
                self.get_posts_gelbooru(&tags, limit, page).await
            }
        }
    }

    async fn get_posts_moebooru(
        &self,
        tags: &str,
        limit: u32,
        page: u32,
    ) -> Result<Vec<BooruPost>> {
        let url = format!("{}/post.json", self.base_url);
        let raw: Vec<MoebooruRawPost> = self
            .request(
                &url,
                &[
                    ("tags", tags),
                    ("limit", &limit.to_string()),
                    ("page", &page.to_string()),
                ],
            )
            .await?;
        Ok(raw.into_iter().map(|r| r.into_booru_post()).collect())
    }

    async fn get_pool_moebooru(&self, pool_id: u64) -> Result<BooruPoolInfo> {
        let url = format!("{}/pool/show.json", self.base_url);
        let raw: MoebooruRawPool = self.request(&url, &[("id", &pool_id.to_string())]).await?;
        Ok(raw.into_pool_info())
    }

    async fn get_pool_posts_moebooru(
        &self,
        pool_id: u64,
        limit: u32,
        page: u32,
    ) -> Result<Vec<BooruPost>> {
        let url = format!("{}/pool/show.json", self.base_url);
        let raw: MoebooruRawPool = self
            .request(
                &url,
                &[("id", &pool_id.to_string()), ("page", &page.to_string())],
            )
            .await?;
        Ok(raw
            .posts
            .into_iter()
            .take(limit as usize)
            .map(|r| r.into_booru_post())
            .collect())
    }

    async fn get_posts_danbooru(
        &self,
        tags: &str,
        limit: u32,
        page: u32,
    ) -> Result<Vec<BooruPost>> {
        let url = format!("{}/posts.json", self.base_url);
        let raw: Vec<DanbooruRawPost> = self
            .request(
                &url,
                &[
                    ("tags", tags),
                    ("limit", &limit.to_string()),
                    ("page", &page.to_string()),
                ],
            )
            .await?;
        Ok(raw.into_iter().map(|r| r.into_booru_post()).collect())
    }

    async fn get_pool_danbooru(&self, pool_id: u64) -> Result<BooruPoolInfo> {
        let url = format!("{}/pools/{}.json", self.base_url, pool_id);
        let raw: DanbooruRawPool = self.request(&url, &[]).await?;
        Ok(raw.into_pool_info())
    }

    async fn get_posts_gelbooru(
        &self,
        tags: &str,
        limit: u32,
        page: u32,
    ) -> Result<Vec<BooruPost>> {
        let url = format!("{}/index.php", self.base_url);
        // Gelbooru uses 0-indexed pid
        let pid = page.saturating_sub(1);
        let resp: GelbooruPostsResponse = self
            .request(
                &url,
                &[
                    ("page", "dapi"),
                    ("s", "post"),
                    ("q", "index"),
                    ("json", "1"),
                    ("tags", tags),
                    ("limit", &limit.to_string()),
                    ("pid", &pid.to_string()),
                ],
            )
            .await?;
        Ok(resp.post.into_iter().map(|r| r.into_booru_post()).collect())
    }

    async fn request<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        params: &[(&str, &str)],
    ) -> Result<T> {
        let mut query_params: Vec<(&str, &str)> = params.to_vec();

        let username_str;
        let api_key_str;
        if let (Some(user), Some(key)) = (&self.username, &self.api_key) {
            username_str = user.clone();
            api_key_str = key.clone();
            match self.engine_type {
                BooruEngineType::Moebooru => {
                    query_params.push(("login", &username_str));
                    query_params.push(("api_key", &api_key_str));
                }
                BooruEngineType::Gelbooru => {
                    query_params.push(("user_id", &username_str));
                    query_params.push(("api_key", &api_key_str));
                }
                BooruEngineType::Danbooru => {}
            }
        }

        let mut req = self
            .client
            .get(url)
            .header(reqwest::header::USER_AGENT, DEFAULT_USER_AGENT)
            .query(&query_params);

        if let (Some(user), Some(key)) = (&self.username, &self.api_key) {
            if self.engine_type == BooruEngineType::Danbooru {
                req = req.basic_auth(user, Some(key));
            }
        }

        let response = req.send().await?;
        let status = response.status();
        let text = response.text().await?;

        if !status.is_success() {
            return Err(Error::Api {
                message: text,
                status: status.as_u16(),
            });
        }

        serde_json::from_str(&text).map_err(|e| {
            tracing::debug!("Failed to parse response from {}: {}", url, e);
            let preview = text
                .char_indices()
                .nth(500)
                .map(|(i, _)| &text[..i])
                .unwrap_or(&text);
            tracing::debug!("Response body: {}", preview);
            e.into()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_creation() {
        let client = BooruClient::new("https://yande.re", BooruEngineType::Moebooru).unwrap();
        assert_eq!(client.base_url(), "https://yande.re");
        assert_eq!(client.engine_type(), BooruEngineType::Moebooru);
    }

    #[test]
    fn test_client_strips_trailing_slash() {
        let client = BooruClient::new("https://yande.re/", BooruEngineType::Moebooru).unwrap();
        assert_eq!(client.base_url(), "https://yande.re");
    }

    #[test]
    fn test_client_with_auth() {
        let client = BooruClient::new("https://danbooru.donmai.us", BooruEngineType::Danbooru)
            .unwrap()
            .with_auth("user", "key123");
        assert!(client.username.is_some());
        assert!(client.api_key.is_some());
    }
}
