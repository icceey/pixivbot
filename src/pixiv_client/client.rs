//! Pixiv API 客户端实现

use crate::pixiv_client::auth;
use crate::pixiv_client::error::{Error, Result};
use crate::pixiv_client::models::*;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, USER_AGENT};
use std::sync::Arc;
use tokio::sync::RwLock;

const APP_API_HOST: &str = "https://app-api.pixiv.net";
const USER_AGENT_VALUE: &str = "PixivIOSApp/7.13.3 (iOS 14.6; iPhone13,2)";

/// Pixiv API 客户端
pub struct PixivClient {
    client: reqwest::Client,
    access_token: Arc<RwLock<Option<String>>>,
    refresh_token: String,
}

impl PixivClient {
    /// 创建新的客户端
    pub fn new(refresh_token: String) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        Ok(Self {
            client,
            access_token: Arc::new(RwLock::new(None)),
            refresh_token,
        })
    }

    /// 使用 refresh_token 进行认证
    pub async fn login(&self) -> Result<()> {
        let auth_response = auth::auth_with_refresh_token(&self.client, &self.refresh_token).await?;
        
        let mut token = self.access_token.write().await;
        *token = Some(auth_response.access_token);
        
        Ok(())
    }

    /// 构建请求头
    async fn build_headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        
        // 设置 User-Agent
        headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        headers.insert("App-OS", HeaderValue::from_static("ios"));
        headers.insert("App-OS-Version", HeaderValue::from_static("14.6"));
        
        // 设置 Authorization
        let token = self.access_token.read().await;
        if let Some(ref access_token) = *token {
            let auth_value = format!("Bearer {}", access_token);
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&auth_value)
                    .map_err(|e| Error::Other(format!("Invalid auth header: {}", e)))?,
            );
        } else {
            return Err(Error::AuthError("Not authenticated, call login() first".to_string()));
        }
        
        Ok(headers)
    }

    /// GET 请求
    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<T> {
        let url = format!("{}{}", APP_API_HOST, path);
        let headers = self.build_headers().await?;

        let response = self
            .client
            .get(&url)
            .headers(headers)
            .query(params)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await?;

        if !status.is_success() {
            return Err(Error::ApiError {
                message: text,
                status: status.as_u16(),
            });
        }

        let result: T = serde_json::from_str(&text)?;
        Ok(result)
    }

    /// 获取用户作品列表
    /// 
    /// # 参数
    /// - `user_id`: 画师 ID
    /// - `illust_type`: 作品类型 ("illust", "manga" 或 None 表示全部)
    /// - `offset`: 分页偏移量
    pub async fn user_illusts(
        &self,
        user_id: u64,
        illust_type: Option<&str>,
        offset: Option<u32>,
    ) -> Result<UserIllusts> {
        let mut params = vec![
            ("user_id", user_id.to_string()),
            ("filter", "for_ios".to_string()),
        ];

        if let Some(t) = illust_type {
            params.push(("type", t.to_string()));
        }

        if let Some(o) = offset {
            params.push(("offset", o.to_string()));
        }

        self.get("/v1/user/illusts", &params).await
    }

    /// 获取作品详情
    pub async fn illust_detail(&self, illust_id: u64) -> Result<IllustDetail> {
        let params = vec![("illust_id", illust_id.to_string())];
        self.get("/v1/illust/detail", &params).await
    }

    /// 获取排行榜
    /// 
    /// # 参数
    /// - `mode`: 排行榜模式 (day, week, month, day_male, day_female, etc.)
    /// - `date`: 日期 (YYYY-MM-DD 格式，None 表示最新)
    /// - `offset`: 分页偏移量
    pub async fn illust_ranking(
        &self,
        mode: &str,
        date: Option<&str>,
        offset: Option<u32>,
    ) -> Result<Ranking> {
        let mut params = vec![
            ("mode", mode.to_string()),
            ("filter", "for_ios".to_string()),
        ];

        if let Some(d) = date {
            params.push(("date", d.to_string()));
        }

        if let Some(o) = offset {
            params.push(("offset", o.to_string()));
        }

        self.get("/v1/illust/ranking", &params).await
    }
}
