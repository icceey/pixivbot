//! Pixiv API 客户端实现

use crate::auth;
use crate::error::{Error, Result};
use crate::models::*;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, USER_AGENT};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const APP_API_HOST: &str = "https://app-api.pixiv.net";
const USER_AGENT_VALUE: &str = "PixivIOSApp/7.13.3 (iOS 14.6; iPhone13,2)";

/// Token 信息，包含 access_token 和过期时间
#[derive(Debug, Clone)]
struct TokenInfo {
    access_token: String,
    /// Token 过期的时间点
    expires_at: Instant,
}

impl TokenInfo {
    /// 检查 token 是否已过期（提前 60 秒刷新，避免边界情况）
    fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at - Duration::from_secs(60)
    }
}

/// Pixiv API 客户端
pub struct PixivClient {
    client: reqwest::Client,
    token_info: Arc<RwLock<Option<TokenInfo>>>,
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
            token_info: Arc::new(RwLock::new(None)),
            refresh_token,
        })
    }

    /// 使用 refresh_token 进行认证
    pub async fn login(&self) -> Result<()> {
        let auth_response =
            auth::auth_with_refresh_token(&self.client, &self.refresh_token).await?;

        // 计算过期时间点
        let expires_at = Instant::now() + Duration::from_secs(auth_response.expires_in);

        let mut token_info = self.token_info.write().await;
        *token_info = Some(TokenInfo {
            access_token: auth_response.access_token,
            expires_at,
        });

        tracing::info!(
            "Token refreshed, expires in {} seconds",
            auth_response.expires_in
        );

        Ok(())
    }

    /// 确保 token 有效，如果过期则自动刷新
    async fn ensure_token_valid(&self) -> Result<()> {
        let needs_refresh = {
            let token_info = self.token_info.read().await;
            match &*token_info {
                Some(info) => info.is_expired(),
                None => true, // 从未登录过
            }
        };

        if needs_refresh {
            self.login().await?;
        }

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
        let token_info = self.token_info.read().await;
        if let Some(ref info) = *token_info {
            let auth_value = format!("Bearer {}", info.access_token);
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&auth_value)
                    .map_err(|e| Error::Other(format!("Invalid auth header: {}", e)))?,
            );
        } else {
            return Err(Error::Auth(
                "Not authenticated, call login() first".to_string(),
            ));
        }

        Ok(headers)
    }

    /// GET 请求
    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<T> {
        // 确保 token 有效，必要时自动刷新
        self.ensure_token_valid().await?;

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
            return Err(Error::Api {
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
    #[allow(dead_code)]
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

    /// 获取用户详情
    ///
    /// # 参数
    /// - `user_id`: 用户 ID
    pub async fn user_detail(&self, user_id: u64) -> Result<UserDetail> {
        let params = vec![
            ("user_id", user_id.to_string()),
            ("filter", "for_ios".to_string()),
        ];
        self.get("/v1/user/detail", &params).await
    }
}
