use crate::config::PixivConfig;
use anyhow::Result;
use pixiv_client::{self, Illust};
use tracing::info;

// Pixiv's publication dates use Japan Standard Time, independent of the host TZ.
pub const SEARCH_TIMEZONE: chrono::FixedOffset = chrono::FixedOffset::east_opt(9 * 3600).unwrap();

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

    /// Search an inclusive date range from oldest to newest.
    pub async fn search_illusts(
        &self,
        query: &str,
        start: chrono::NaiveDate,
        end: chrono::NaiveDate,
        offset: u32,
    ) -> Result<(Vec<Illust>, Option<u32>)> {
        let start_date = start.format("%Y-%m-%d").to_string();
        let end_date = end.format("%Y-%m-%d").to_string();
        let response = self
            .client
            .search_illusts(query, &start_date, &end_date, offset)
            .await?;
        let next_offset = response
            .next_url
            .map(|next| -> Result<u32> {
                let next = url::Url::parse(&next)?;
                let offset_value = next
                    .query_pairs()
                    .find(|(key, _)| key == "offset")
                    .ok_or_else(|| anyhow::anyhow!("Search pagination has no offset"))?
                    .1;
                let next_offset: u32 = offset_value.parse()?;
                anyhow::ensure!(next_offset > offset, "Search pagination did not advance");
                Ok(next_offset)
            })
            .transpose()?;
        Ok((response.illusts, next_offset))
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

    /// 获取 Ugoira (动图) 元数据
    pub async fn get_ugoira_metadata(
        &self,
        illust_id: u64,
    ) -> Result<pixiv_client::UgoiraMetadataInfo> {
        let response = self.client.ugoira_metadata(illust_id).await?;
        info!("Fetched ugoira metadata for illust {}", illust_id);
        Ok(response.ugoira_metadata)
    }
}
