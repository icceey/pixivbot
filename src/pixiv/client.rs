use crate::config::PixivConfig;
use crate::error::AppResult;

#[derive(Clone)]
pub struct PixivClient {
    // client: pixivrs::PixivClient, // Actual client from pixivrs
    config: PixivConfig
}

impl PixivClient {
    pub fn new(config: PixivConfig) -> Self {
        Self { config }
    }

    pub async fn login(&mut self) -> AppResult<()> {
        // Placeholder for login logic
        // self.client.login(&self.config.refresh_token).await?;
        Ok(())
    }
}
