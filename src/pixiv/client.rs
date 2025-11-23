use crate::config::PixivConfig;
use crate::error::AppResult;
use crate::pixiv_client::{self, Illust};
use tracing::info;

pub struct PixivClient {
    client: pixiv_client::PixivClient,
}

impl PixivClient {
    pub fn new(config: PixivConfig) -> AppResult<Self> {
        let client = pixiv_client::PixivClient::new(config.refresh_token)?;
        
        Ok(Self { client })
    }

    /// Login using refresh token
    pub async fn login(&mut self) -> AppResult<()> {
        info!("Authenticating with Pixiv using refresh token...");
        
        self.client.login().await?;
        
        info!("✅ Pixiv authentication successful");
        Ok(())
    }

    /// Get latest illusts from an author
    pub async fn get_user_illusts(
        &self,
        user_id: u64,
        limit: usize,
    ) -> AppResult<Vec<Illust>> {
        info!("Fetching illusts for author {}", user_id);
        
        let response = self.client.user_illusts(user_id, Some("illust"), None).await?;
        
        let illusts: Vec<_> = response.illusts.into_iter().take(limit).collect();
        
        info!("Successfully fetched {} illusts for author {}", illusts.len(), user_id);
        Ok(illusts)
    }

    /// Get ranking illusts
    pub async fn get_ranking(
        &self,
        mode: &str,
        date: Option<&str>,
        limit: usize,
    ) -> AppResult<Vec<Illust>> {
        info!("Fetching {} ranking", mode);
        
        let response = self.client.illust_ranking(mode, date, None).await?;
        
        let illusts: Vec<_> = response.illusts.into_iter().take(limit).collect();
        info!("Fetched {} ranking illusts", illusts.len());
        
        Ok(illusts)
    }

    /// Get illust detail by ID
    pub async fn get_illust_detail(
        &self,
        illust_id: u64,
    ) -> AppResult<Illust> {
        info!("Fetching illust detail for {}", illust_id);
        
        let response = self.client.illust_detail(illust_id).await?;
        
        Ok(response.illust)
    }

    /// Get image download URL from illust
    pub fn get_image_url(&self, illust: &Illust) -> String {
        // Prefer original quality from meta_single_page
        if let Some(original_url) = &illust.meta_single_page.original_image_url {
            return original_url.clone();
        }
        
        // Fallback to large image
        illust.image_urls.large.clone()
    }
}
