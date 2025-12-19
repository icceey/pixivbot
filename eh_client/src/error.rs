//! E-Hentai 客户端错误类型

use std::fmt;

/// E-Hentai 客户端错误类型
#[derive(Debug)]
pub enum Error {
    /// HTTP 请求错误
    Http(reqwest::Error),
    /// JSON 解析错误
    Json(serde_json::Error),
    /// API 错误
    Api { message: String, status: u16 },
    /// 认证错误
    Auth(String),
    /// 配额限制
    RateLimit(String),
    /// 画廊不存在或被删除
    GalleryNotFound(String),
    /// 需要 ExHentai 但未登录
    ExhentaiRequired,
    /// 其他错误
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Http(e) => write!(f, "HTTP error: {}", e),
            Error::Json(e) => write!(f, "JSON parse error: {}", e),
            Error::Api { message, status } => {
                write!(f, "API error ({}): {}", status, message)
            }
            Error::Auth(msg) => write!(f, "Auth error: {}", msg),
            Error::RateLimit(msg) => write!(f, "Rate limit: {}", msg),
            Error::GalleryNotFound(id) => write!(f, "Gallery not found: {}", id),
            Error::ExhentaiRequired => {
                write!(f, "ExHentai access required but not authenticated")
            }
            Error::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Http(e) => Some(e),
            Error::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Http(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
