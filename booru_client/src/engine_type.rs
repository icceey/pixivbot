//! Booru 引擎类型定义

use serde::{Deserialize, Serialize};
use std::fmt;

/// Booru 站点引擎类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BooruEngineType {
    /// Moebooru 引擎 (yande.re, konachan.com)
    Moebooru,
    /// Danbooru 引擎 (danbooru.donmai.us)
    Danbooru,
    /// Gelbooru 引擎 (gelbooru.com)
    Gelbooru,
}

impl BooruEngineType {
    /// Returns the post URL path for a given post ID, relative to the base URL.
    pub fn post_path(&self, post_id: u64) -> String {
        match self {
            BooruEngineType::Moebooru => format!("/post/show/{}", post_id),
            BooruEngineType::Danbooru => format!("/posts/{}", post_id),
            BooruEngineType::Gelbooru => {
                format!("/index.php?page=post&s=view&id={}", post_id)
            }
        }
    }

    pub fn supports_fav_count(&self) -> bool {
        matches!(self, BooruEngineType::Danbooru)
    }
}

impl fmt::Display for BooruEngineType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BooruEngineType::Moebooru => write!(f, "moebooru"),
            BooruEngineType::Danbooru => write!(f, "danbooru"),
            BooruEngineType::Gelbooru => write!(f, "gelbooru"),
        }
    }
}
