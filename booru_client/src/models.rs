use crate::engine_type::BooruEngineType;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BooruPost {
    pub id: u64,
    pub tags: String,
    #[serde(default)]
    pub score: i32,
    #[serde(default)]
    pub fav_count: i32,
    #[serde(default)]
    pub file_url: Option<String>,
    #[serde(default)]
    pub sample_url: Option<String>,
    #[serde(default)]
    pub jpeg_url: Option<String>,
    #[serde(default)]
    pub preview_url: Option<String>,
    pub rating: BooruRating,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub md5: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub file_size: Option<u64>,
    #[serde(default)]
    pub file_ext: Option<String>,
    /// active / deleted / flagged
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BooruRating {
    General,
    Safe,
    /// Gelbooru's "sensitive" rating — between safe and questionable.
    Sensitive,
    Questionable,
    Explicit,
}

impl BooruRating {
    pub fn from_moebooru(s: &str) -> Self {
        match s {
            "q" => BooruRating::Questionable,
            "e" => BooruRating::Explicit,
            _ => BooruRating::Safe,
        }
    }

    pub fn from_danbooru(s: &str) -> Self {
        match s {
            "g" => BooruRating::General,
            "s" => BooruRating::Sensitive,
            "q" => BooruRating::Questionable,
            "e" => BooruRating::Explicit,
            _ => BooruRating::Safe,
        }
    }

    pub fn from_gelbooru(s: &str) -> Self {
        match s {
            "general" => BooruRating::General,
            "sensitive" => BooruRating::Sensitive,
            "questionable" => BooruRating::Questionable,
            "explicit" => BooruRating::Explicit,
            _ => BooruRating::Safe,
        }
    }

    pub fn as_short_str(&self) -> &'static str {
        match self {
            BooruRating::General => "g",
            BooruRating::Safe => "s",
            BooruRating::Sensitive => "se",
            BooruRating::Questionable => "q",
            BooruRating::Explicit => "e",
        }
    }

    pub fn as_gelbooru_str(&self) -> &'static str {
        match self {
            BooruRating::General | BooruRating::Safe => "general",
            BooruRating::Sensitive => "sensitive",
            BooruRating::Questionable => "questionable",
            BooruRating::Explicit => "explicit",
        }
    }

    /// Returns the correct rating string for API queries on the given engine.
    ///
    /// Each booru engine uses different rating vocabularies:
    /// - Moebooru: s (safe), q (questionable), e (explicit)
    /// - Danbooru: g (general), s (sensitive), q (questionable), e (explicit)
    /// - Gelbooru: general, sensitive, questionable, explicit
    ///
    /// `BooruRating::Safe` (a Moebooru concept) maps to the safest tier on each engine.
    pub fn as_api_str(&self, engine_type: BooruEngineType) -> &'static str {
        match engine_type {
            BooruEngineType::Moebooru => match self {
                BooruRating::General | BooruRating::Safe => "s",
                BooruRating::Sensitive | BooruRating::Questionable => "q",
                BooruRating::Explicit => "e",
            },
            BooruEngineType::Danbooru => match self {
                BooruRating::General | BooruRating::Safe => "g",
                BooruRating::Sensitive => "s",
                BooruRating::Questionable => "q",
                BooruRating::Explicit => "e",
            },
            BooruEngineType::Gelbooru => self.as_gelbooru_str(),
        }
    }

    pub fn from_short_str(s: &str) -> Self {
        match s {
            "g" => BooruRating::General,
            "s" => BooruRating::Safe,
            "se" => BooruRating::Sensitive,
            "q" => BooruRating::Questionable,
            "e" => BooruRating::Explicit,
            _ => BooruRating::Safe,
        }
    }

    pub fn is_nsfw(&self) -> bool {
        matches!(
            self,
            BooruRating::Sensitive | BooruRating::Questionable | BooruRating::Explicit
        )
    }
}

impl fmt::Display for BooruRating {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BooruRating::General => write!(f, "General"),
            BooruRating::Safe => write!(f, "Safe"),
            BooruRating::Sensitive => write!(f, "Sensitive"),
            BooruRating::Questionable => write!(f, "Questionable"),
            BooruRating::Explicit => write!(f, "Explicit"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PopularScale {
    Day,
    Week,
    Month,
}

impl PopularScale {
    pub fn as_str(&self) -> &'static str {
        match self {
            PopularScale::Day => "day",
            PopularScale::Week => "week",
            PopularScale::Month => "month",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "day" => Some(PopularScale::Day),
            "week" => Some(PopularScale::Week),
            "month" => Some(PopularScale::Month),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BooruPoolInfo {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub post_count: u32,
    #[serde(default)]
    pub post_ids: Vec<u64>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MoebooruRawPost {
    pub id: u64,
    #[serde(default)]
    pub tags: String,
    #[serde(default)]
    pub score: i32,
    #[serde(default)]
    pub fav_count: i32,
    #[serde(default)]
    pub file_url: Option<String>,
    #[serde(default)]
    pub sample_url: Option<String>,
    #[serde(default)]
    pub jpeg_url: Option<String>,
    #[serde(default)]
    pub preview_url: Option<String>,
    #[serde(default)]
    pub rating: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub md5: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub file_size: Option<u64>,
    #[serde(default)]
    pub file_ext: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

impl MoebooruRawPost {
    pub fn into_booru_post(self) -> BooruPost {
        let created_at = self
            .created_at
            .and_then(|ts| DateTime::from_timestamp(ts, 0));
        BooruPost {
            id: self.id,
            tags: self.tags,
            score: self.score,
            fav_count: self.fav_count,
            file_url: self.file_url,
            sample_url: self.sample_url,
            jpeg_url: self.jpeg_url,
            preview_url: self.preview_url,
            rating: BooruRating::from_moebooru(&self.rating),
            width: self.width,
            height: self.height,
            md5: self.md5,
            source: self.source,
            created_at,
            file_size: self.file_size,
            file_ext: self.file_ext,
            status: self.status,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MoebooruRawPool {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub post_count: u32,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub posts: Vec<MoebooruRawPost>,
}

impl MoebooruRawPool {
    pub fn into_pool_info(self) -> BooruPoolInfo {
        let post_ids = self.posts.iter().map(|p| p.id).collect();
        BooruPoolInfo {
            id: self.id,
            name: self.name,
            post_count: self.post_count,
            post_ids,
            description: self.description,
            created_at: None, // Moebooru 的 created_at 格式不固定，暂不解析
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DanbooruRawPost {
    pub id: u64,
    #[serde(default)]
    pub tag_string: String,
    #[serde(default)]
    pub score: i32,
    #[serde(default)]
    pub fav_count: i32,
    #[serde(default)]
    pub file_url: Option<String>,
    #[serde(default)]
    pub large_file_url: Option<String>,
    #[serde(default)]
    pub preview_file_url: Option<String>,
    #[serde(default)]
    pub rating: Option<String>,
    #[serde(default)]
    pub image_width: u32,
    #[serde(default)]
    pub image_height: u32,
    #[serde(default)]
    pub md5: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub file_size: Option<u64>,
    #[serde(default)]
    pub file_ext: Option<String>,
    #[serde(default)]
    pub is_deleted: bool,
    #[serde(default)]
    pub is_banned: bool,
}

impl DanbooruRawPost {
    pub fn into_booru_post(self) -> BooruPost {
        let created_at = self
            .created_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        let status = if self.is_deleted {
            Some("deleted".to_string())
        } else if self.is_banned {
            Some("banned".to_string())
        } else {
            Some("active".to_string())
        };
        BooruPost {
            id: self.id,
            tags: self.tag_string,
            score: self.score,
            fav_count: self.fav_count,
            file_url: self.file_url,
            sample_url: self.large_file_url,
            jpeg_url: None,
            preview_url: self.preview_file_url,
            rating: BooruRating::from_danbooru(self.rating.as_deref().unwrap_or("s")),
            width: self.image_width,
            height: self.image_height,
            md5: self.md5,
            source: self.source,
            created_at,
            file_size: self.file_size,
            file_ext: self.file_ext,
            status,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DanbooruRawPool {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub post_count: u32,
    #[serde(default)]
    pub post_ids: Vec<u64>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

impl DanbooruRawPool {
    pub fn into_pool_info(self) -> BooruPoolInfo {
        let created_at = self
            .created_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc));
        BooruPoolInfo {
            id: self.id,
            name: self.name,
            post_count: self.post_count,
            post_ids: self.post_ids,
            description: self.description,
            created_at,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GelbooruPostsResponse {
    #[serde(default)]
    pub post: Vec<GelbooruRawPost>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GelbooruRawPost {
    pub id: u64,
    #[serde(default)]
    pub tags: String,
    #[serde(default)]
    pub score: i32,
    #[serde(default)]
    pub fav_count: Option<i32>,
    #[serde(default)]
    pub file_url: Option<String>,
    #[serde(default)]
    pub sample_url: Option<String>,
    #[serde(default)]
    pub preview_url: Option<String>,
    #[serde(default)]
    pub rating: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    /// Gelbooru 使用 `hash` 而非 `md5`
    #[serde(default, alias = "md5")]
    pub hash: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub file_size: Option<u64>,
    #[serde(default)]
    pub file_ext: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

impl GelbooruRawPost {
    pub fn into_booru_post(self) -> BooruPost {
        // Gelbooru 日期格式示例: "Wed Jun 01 12:34:56 -0500 2022"
        // 或 "2022-06-01 12:34:56"，格式不统一，best-effort 解析
        let created_at = self.created_at.as_deref().and_then(|s| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|ndt| ndt.and_utc())
                .or_else(|| {
                    DateTime::parse_from_rfc3339(s)
                        .ok()
                        .map(|dt| dt.with_timezone(&Utc))
                })
                .or_else(|| {
                    DateTime::parse_from_str(s, "%a %b %d %H:%M:%S %z %Y")
                        .ok()
                        .map(|dt| dt.with_timezone(&Utc))
                })
        });
        BooruPost {
            id: self.id,
            tags: self.tags,
            score: self.score,
            fav_count: self.fav_count.unwrap_or(0),
            file_url: self.file_url,
            sample_url: self.sample_url,
            jpeg_url: None,
            preview_url: self.preview_url,
            rating: BooruRating::from_gelbooru(&self.rating),
            width: self.width,
            height: self.height,
            md5: self.hash,
            source: self.source,
            created_at,
            file_size: self.file_size,
            file_ext: self.file_ext,
            status: self.status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratings_parse_each_provider_vocabulary() {
        for (moe, dan, gel, expected) in [
            ("q", "q", "questionable", BooruRating::Questionable),
            ("e", "e", "explicit", BooruRating::Explicit),
            ("unknown", "x", "???", BooruRating::Safe),
        ] {
            assert_eq!(BooruRating::from_moebooru(moe), expected);
            assert_eq!(BooruRating::from_danbooru(dan), expected);
            assert_eq!(BooruRating::from_gelbooru(gel), expected);
        }
        assert_eq!(BooruRating::from_moebooru("s"), BooruRating::Safe);
        assert_eq!(BooruRating::from_danbooru("g"), BooruRating::General);
        assert_eq!(BooruRating::from_gelbooru("general"), BooruRating::General);
        assert_eq!(BooruRating::from_danbooru("s"), BooruRating::Sensitive);
        assert_eq!(
            BooruRating::from_gelbooru("sensitive"),
            BooruRating::Sensitive
        );
    }

    #[test]
    fn ratings_encode_for_each_provider() {
        for (rating, moe, dan, gel) in [
            (BooruRating::General, "s", "g", "general"),
            (BooruRating::Safe, "s", "g", "general"),
            (BooruRating::Sensitive, "q", "s", "sensitive"),
            (BooruRating::Questionable, "q", "q", "questionable"),
            (BooruRating::Explicit, "e", "e", "explicit"),
        ] {
            for (engine, expected) in [
                (BooruEngineType::Moebooru, moe),
                (BooruEngineType::Danbooru, dan),
                (BooruEngineType::Gelbooru, gel),
            ] {
                assert_eq!(
                    rating.as_api_str(engine),
                    expected,
                    "{rating:?}, {engine:?}"
                );
            }
        }
    }
}
