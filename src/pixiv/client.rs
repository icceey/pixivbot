use chrono::{DateTime, Utc};
use governor::{Quota, RateLimiter};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use std::num::NonZeroU32;
use std::sync::Arc;
use tokio::sync::RwLock;
use md5;

use crate::config::PixivConfig;
use crate::error::{AppError, Result};
use crate::pixiv::model::{AuthResponse, Illust, UserIllustsResponse};

const PIXIV_APP_API: &str = "https://app-api.pixiv.net";
const PIXIV_OAUTH: &str = "https://oauth.secure.pixiv.net/auth/token";
const CLIENT_ID: &str = "MOBrBDS8blbauCxk";
const CLIENT_SECRET: &str = "lsACyCDKFloLs1b7Ak05K1L4g3uNl4J";
const HASH_SECRET: &str = "28c1fdd170a5204386cb1313c7077b34f83e4aaf4aa829ce78c231e05b0bae9c";

#[derive(Clone)]
pub struct PixivClient {
    client: Client,
    config: PixivConfig,
    auth_state: Arc<RwLock<AuthState>>,
    rate_limiter: Arc<RateLimiter<governor::state::direct::NotKeyed, governor::state::InMemoryState, governor::clock::QuantaClock>>,
}

#[derive(Clone)]
struct AuthState {
    access_token: Option<String>,
    token_expires_at: Option<DateTime<Utc>>,
}

impl PixivClient {
    pub fn new(config: PixivConfig) -> Self {
        let client = Client::builder()
            .user_agent("PixivBot/0.1.0")
            .build()
            .expect("Failed to create HTTP client");
        
        // Rate limit: 30 requests per minute
        let quota = Quota::per_minute(NonZeroU32::new(30).unwrap());
        let rate_limiter = RateLimiter::direct(quota);
        
        Self {
            client,
            config,
            auth_state: Arc::new(RwLock::new(AuthState {
                access_token: None,
                token_expires_at: None,
            })),
            rate_limiter: Arc::new(rate_limiter),
        }
    }
    
    async fn ensure_auth(&self) -> Result<String> {
        let mut auth = self.auth_state.write().await;
        
        // Check if token is still valid
        let now = Utc::now();
        if !self.config.refresh_token.is_empty() {
            if let (Some(token), Some(expires_at)) = (auth.access_token.clone(), auth.token_expires_at) {
                if expires_at > now {
                    return Ok(token);
                }
            }
            
            // Refresh token
            let refresh_token = &self.config.refresh_token;
            let auth_response = self.refresh_token(refresh_token).await?;
            auth.access_token = Some(auth_response.access_token.clone());
            auth.token_expires_at = Some(now + chrono::Duration::seconds(auth_response.expires_in as i64));
            
            return Ok(auth_response.access_token);
        }
        
        Err(AppError::Custom("Pixiv refresh token not configured".to_string()))
    }
    
    async fn refresh_token(&self, refresh_token: &str) -> Result<AuthResponse> {
        let mut params = serde_json::Map::new();
        params.insert("client_id".to_string(), Value::String(CLIENT_ID.to_string()));
        params.insert("client_secret".to_string(), Value::String(CLIENT_SECRET.to_string()));
        params.insert("grant_type".to_string(), Value::String("refresh_token".to_string()));
        params.insert("refresh_token".to_string(), Value::String(refresh_token.to_string()));
        params.insert("get_secure_url".to_string(), Value::Bool(true));
        
        // Calculate time and hash for headers
        let time_now = chrono::Utc::now().timestamp().to_string();
        let hash_input = format!("{}{}", time_now, HASH_SECRET);
        let hash = md5::compute(hash_input);
        let hash_hex = format!("{:x}", hash);
        
        let response = self.client
            .post(PIXIV_OAUTH)
            .header("X-Client-Time", time_now)
            .header("X-Client-Hash", hash_hex)
            .header("Content-Type", "application/json")
            .json(&params)
            .send()
            .await?;
        
        if response.status() != StatusCode::OK {
            return Err(AppError::Pixiv(format!(
                "Failed to refresh token: {}",
                response.status()
            )));
        }
        
        let auth_response: AuthResponse = response.json().await?;
        Ok(auth_response)
    }
    
    pub async fn fetch_user_illusts(&self, user_id: u64) -> Result<Vec<Illust>> {
        // Wait for rate limiter
        self.rate_limiter.until_ready().await;
        
        let token = self.ensure_auth().await?;
        
        let url = format!("{}/v1/user/illusts", PIXIV_APP_API);
        
        let response = self.client
            .get(&url)
            .header("Authorization", format!("Bearer {}", token))
            .query(&[
                ("user_id", user_id.to_string()), 
                ("filter", "for_ios".to_string()), 
                ("type", "illust".to_string())
            ])
            .send()
            .await?;
        
        if response.status() == StatusCode::UNAUTHORIZED {
            // Try to refresh token
            let token = self.ensure_auth().await?;
            let response = self.client
                .get(&url)
                .header("Authorization", format!("Bearer {}", token))
                .query(&[
                    ("user_id", user_id.to_string()), 
                    ("filter", "for_ios".to_string()), 
                    ("type", "illust".to_string())
                ])
                .send()
                .await?;
                
            if response.status() != StatusCode::OK {
                return Err(AppError::Pixiv(format!(
                    "Failed to fetch user illusts: {}",
                    response.status()
                )));
            }
            
            let user_illusts: UserIllustsResponse = response.json().await?;
            return Ok(user_illusts.illusts);
        }
        
        if response.status() != StatusCode::OK {
            return Err(AppError::Pixiv(format!(
                "Failed to fetch user illusts: {}",
                response.status()
            )));
        }
        
        let user_illusts: UserIllustsResponse = response.json().await?;
        Ok(user_illusts.illusts)
    }
    
    pub async fn download_image(&self, url: &str) -> Result<Vec<u8>> {
        // Wait for rate limiter
        self.rate_limiter.until_ready().await;
        
        let response = self.client
            .get(url)
            .header("Referer", PIXIV_APP_API)
            .send()
            .await?;
        
        if response.status() != StatusCode::OK {
            return Err(AppError::Pixiv(format!(
                "Failed to download image: {}",
                response.status()
            )));
        }
        
        let bytes = response.bytes().await?;
        Ok(bytes.to_vec())
    }
}