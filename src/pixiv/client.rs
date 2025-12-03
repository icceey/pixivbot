use crate::config::PixivConfig;
use crate::pixiv_client::{self, Illust};
use anyhow::Result;
use tracing::info;

pub struct PixivClient {
    client: pixiv_client::PixivClient,
}

impl PixivClient {
    pub fn new(config: PixivConfig) -> Result<Self> {
        let client = pixiv_client::PixivClient::new(config.refresh_token)?;

        Ok(Self { client })
    }

    /// Login using refresh token
    pub async fn login(&mut self) -> Result<()> {
        self.client.login().await?;

        info!("✅ Pixiv authentication successful");
        Ok(())
    }

    /// Get latest illusts from an author
    pub async fn get_user_illusts(&self, user_id: u64, limit: usize) -> Result<Vec<Illust>> {
        let response = self
            .client
            .user_illusts(user_id, Some("illust"), None)
            .await?;

        let illusts: Vec<_> = response.illusts.into_iter().take(limit).collect();

        info!(
            "Successfully fetched {} illusts for author {}",
            illusts.len(),
            user_id
        );
        Ok(illusts)
    }

    /// Get ranking illusts
    pub async fn get_ranking(
        &self,
        mode: &str,
        date: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Illust>> {
        let response = self.client.illust_ranking(mode, date, None).await?;

        let illusts: Vec<_> = response.illusts.into_iter().take(limit).collect();
        info!("Fetched {} ranking illusts", illusts.len());

        Ok(illusts)
    }

    /// Get illust detail by ID
    pub async fn get_illust_detail(&self, illust_id: u64) -> Result<Illust> {
        let response = self.client.illust_detail(illust_id).await?;

        Ok(response.illust)
    }

    /// 获取用户详情
    pub async fn get_user_detail(&self, user_id: u64) -> Result<pixiv_client::User> {
        let response = self.client.user_detail(user_id).await?;

        info!(
            "Successfully fetched user detail: {} ({})",
            response.user.name, response.user.id
        );
        Ok(response.user)
    }
}
