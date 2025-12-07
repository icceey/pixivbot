//! 错误类型定义

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// HTTP 请求错误
    Http(reqwest::Error),
    /// JSON 解析错误
    Json(serde_json::Error),
    /// API 返回的错误
    Api { message: String, status: u16 },
    /// 认证错误
    Auth(String),
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
            Error::Auth(msg) => write!(f, "Authentication error: {}", msg),
            Error::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for Error {}

impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        Error::Http(err)
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error::Json(err)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
