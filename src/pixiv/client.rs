use crate::config::PixivConfig;
use crate::error::AppResult;
use pixivrs::{AppClient, AuthClient, HttpClient, Filter, RankingMode};
use pixivrs::models::app::Illust;
use tracing::{info, warn};
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct PixivClient {
    app_client: Arc<RwLock<AppClient>>,
    auth_client: AuthClient,
    config: PixivConfig,
}

impl PixivClient {
    pub fn new(config: PixivConfig) -> AppResult<Self> {
        let http_client = HttpClient::new()?;
        let app_client = AppClient::new(http_client);
        let auth_client = AuthClient::new()?;
        
        Ok(Self {
            app_client: Arc::new(RwLock::new(app_client)),
            auth_client,
            config,
        })
    }

    /// Login using refresh token
    pub async fn login(&mut self) -> AppResult<()> {
        info!("Authenticating with Pixiv using refresh token...");
        
        let auth_response = self.auth_client
            .refresh_access_token(&self.config.refresh_token)
            .await?;
        
        // Create new HTTP client with token
        let mut http_client = HttpClient::new()?;
        http_client.set_access_token(auth_response.access_token.clone());
        http_client.set_refresh_token(auth_response.refresh_token.clone());
        
        // Update app client
        let mut app_client = self.app_client.write().await;
        *app_client = AppClient::new(http_client);
        
        info!("✅ Pixiv authentication successful");
        Ok(())
    }

    /// Get latest illusts from an author using search by user ID
    /// Note: pixivrs doesn't have direct user_illusts method, so we use search
    pub async fn get_user_illusts(
        &self,
        user_id: u64,
        limit: usize,
    ) -> AppResult<Vec<Illust>> {
        use pixivrs::{Filter, SearchTarget, Sort};
        
        info!("Fetching illusts for user {}", user_id);
        
        let app_client = self.app_client.read().await;
        
        // Search for illusts by this user ID
        // Using PartialMatchForTags with user:XXXXX pattern
        let query = format!("user:{}", user_id);
        
        match app_client.search_illust(
            &query,
            SearchTarget::PartialMatchForTags,
            Sort::DateDesc,
            None,       // duration
            None,       // start_date
            None,       // end_date
            Filter::ForIOS,
            Some(0),    // ai_type: filter AI works
            Some(0),    // offset
        ).await {
            Ok(response) => {
                let illusts: Vec<_> = response.illusts.into_iter().take(limit).collect();
                info!("Fetched {} illusts for user {} via search", illusts.len(), user_id);
                Ok(illusts)
            }
            Err(e) => {
                warn!("Failed to fetch user illusts for {}: {}", user_id, e);
                Err(e.into())
            }
        }
    }

    /// Get ranking illusts
    pub async fn get_ranking(
        &self,
        mode: &str,
        date: Option<&str>,
        limit: usize,
    ) -> AppResult<Vec<Illust>> {
        let ranking_mode = match mode {
            "daily" => RankingMode::Day,
            "weekly" => RankingMode::Week,
            "monthly" => RankingMode::Month,
            "daily_r18" => RankingMode::DayR18,
            "weekly_r18" => RankingMode::WeekR18,
            "daily_male" => RankingMode::DayMale,
            "daily_female" => RankingMode::DayFemale,
            "daily_male_r18" => RankingMode::DayMaleR18,
            "daily_female_r18" => RankingMode::DayFemaleR18,
            _ => {
                warn!("Unknown ranking mode: {}, using daily", mode);
                RankingMode::Day
            }
        };
        
        info!("Fetching {:?} ranking", ranking_mode);
        
        let app_client = self.app_client.read().await;
        let response = app_client
            .illust_ranking(ranking_mode, Filter::ForIOS, date, None)
            .await?;
        
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
        
        let app_client = self.app_client.read().await;
        let response = app_client.illust_detail(illust_id).await?;
        
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
