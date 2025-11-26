//! Pixiv OAuth 认证模块
//!
//! 参考 [pixivpy](https://github.com/upbit/pixivpy) 的 pixivpy3/api.py auth() 方法实现

use crate::pixiv_client::error::{Error, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

const CLIENT_ID: &str = "MOBrBDS8blbauoSck0ZfDbtuzpyT";
const CLIENT_SECRET: &str = "lsACyCD94FhDUtGTXi3QzcFE2uU1hqtDaKeqrdwj";
const HASH_SECRET: &str = "28c1fdd170a5204386cb1313c7077b34f83e4aaf4aa829ce78c231e05b0bae2c";
const AUTH_URL: &str = "https://oauth.secure.pixiv.net/auth/token";

/// 认证响应
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub user: AuthUser,
    pub expires_in: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthUser {
    pub profile_image_urls: serde_json::Value,
    pub id: String,
    pub name: String,
    pub account: String,
    pub mail_address: String,
    pub is_premium: bool,
    pub x_restrict: u32,
    pub is_mail_authorized: bool,
}

/// 使用 refresh_token 获取 access_token
pub async fn auth_with_refresh_token(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<AuthResponse> {
    // 生成时间戳和哈希
    let local_time = Utc::now().format("%Y-%m-%dT%H:%M:%S+00:00").to_string();
    let hash_input = format!("{}{}", local_time, HASH_SECRET);
    let hash = format!("{:x}", md5::compute(hash_input.as_bytes()));

    // 构造请求
    let params = [
        ("get_secure_url", "1"),
        ("client_id", CLIENT_ID),
        ("client_secret", CLIENT_SECRET),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ];

    let response = client
        .post(AUTH_URL)
        .header("X-Client-Time", &local_time)
        .header("X-Client-Hash", &hash)
        .header("User-Agent", "PixivIOSApp/7.13.3 (iOS 14.6; iPhone13,2)")
        .header("App-OS", "ios")
        .header("App-OS-Version", "14.6")
        .form(&params)
        .send()
        .await?;

    let status = response.status();
    let text = response.text().await?;

    if !status.is_success() {
        return Err(Error::Auth(format!(
            "Authentication failed ({}): {}",
            status, text
        )));
    }

    // 解析响应
    let auth_response: AuthResponse = serde_json::from_str(&text)?;
    Ok(auth_response)
}
