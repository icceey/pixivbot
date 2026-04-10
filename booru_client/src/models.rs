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
            "s" => BooruRating::Safe,
            "q" => BooruRating::Questionable,
            "e" => BooruRating::Explicit,
            _ => BooruRating::Safe,
        }
    }

    pub fn from_gelbooru(s: &str) -> Self {
        match s {
            "general" => BooruRating::General,
            "sensitive" => BooruRating::Safe,
            "questionable" => BooruRating::Questionable,
            "explicit" => BooruRating::Explicit,
            _ => BooruRating::Safe,
        }
    }

    pub fn as_short_str(&self) -> &'static str {
        match self {
            BooruRating::General => "g",
            BooruRating::Safe => "s",
            BooruRating::Questionable => "q",
            BooruRating::Explicit => "e",
        }
    }

    pub fn as_gelbooru_str(&self) -> &'static str {
        match self {
            BooruRating::General => "general",
            BooruRating::Safe => "sensitive",
            BooruRating::Questionable => "questionable",
            BooruRating::Explicit => "explicit",
        }
    }

    pub fn from_short_str(s: &str) -> Self {
        match s {
            "g" => BooruRating::General,
            "s" => BooruRating::Safe,
            "q" => BooruRating::Questionable,
            "e" => BooruRating::Explicit,
            _ => BooruRating::Safe,
        }
    }

    pub fn is_nsfw(&self) -> bool {
        matches!(self, BooruRating::Questionable | BooruRating::Explicit)
    }
}

impl fmt::Display for BooruRating {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BooruRating::General => write!(f, "General"),
            BooruRating::Safe => write!(f, "Safe"),
            BooruRating::Questionable => write!(f, "Questionable"),
            BooruRating::Explicit => write!(f, "Explicit"),
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
    #[allow(dead_code)]
    pub created_at: Option<String>,
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
        });
        BooruPost {
            id: self.id,
            tags: self.tags,
            score: self.score,
            fav_count: self.fav_count.unwrap_or(0),
            file_url: self.file_url,
            sample_url: self.sample_url,
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
    fn test_moebooru_raw_post_deserialization() {
        let json = r#"{
            "id": 1234567,
            "tags": "landscape sky cloud scenery",
            "created_at": 1700000000,
            "source": "https://example.com/source",
            "score": 42,
            "md5": "abc123def456",
            "file_size": 2048000,
            "file_ext": "jpg",
            "file_url": "https://files.yande.re/image/abc123/yande.re%201234567.jpg",
            "preview_url": "https://files.yande.re/preview/abc123/yande.re%201234567.jpg",
            "sample_url": "https://files.yande.re/sample/abc123/yande.re%201234567.jpg",
            "rating": "s",
            "width": 3840,
            "height": 2160,
            "fav_count": 15,
            "status": "active"
        }"#;

        let raw: MoebooruRawPost = serde_json::from_str(json).unwrap();
        assert_eq!(raw.id, 1234567);
        assert_eq!(raw.tags, "landscape sky cloud scenery");
        assert_eq!(raw.score, 42);
        assert_eq!(raw.fav_count, 15);
        assert_eq!(raw.rating, "s");
        assert_eq!(raw.md5.as_deref(), Some("abc123def456"));
        assert_eq!(raw.width, 3840);
        assert_eq!(raw.height, 2160);
        assert!(raw.file_url.is_some());
        assert!(raw.sample_url.is_some());

        let post = raw.into_booru_post();
        assert_eq!(post.id, 1234567);
        assert_eq!(post.rating, BooruRating::Safe);
        assert!(post.created_at.is_some());
        assert_eq!(post.score, 42);
    }

    #[test]
    fn test_moebooru_missing_optional_fields() {
        let json = r#"{
            "id": 999,
            "tags": "test",
            "score": 0,
            "rating": "q",
            "width": 100,
            "height": 100
        }"#;

        let raw: MoebooruRawPost = serde_json::from_str(json).unwrap();
        assert_eq!(raw.id, 999);
        assert!(raw.file_url.is_none());
        assert!(raw.sample_url.is_none());
        assert!(raw.md5.is_none());
        assert!(raw.source.is_none());

        let post = raw.into_booru_post();
        assert_eq!(post.rating, BooruRating::Questionable);
        assert!(post.file_url.is_none());
        assert!(post.md5.is_none());
        assert!(post.created_at.is_none());
    }

    #[test]
    fn test_rating_from_moebooru() {
        assert_eq!(BooruRating::from_moebooru("s"), BooruRating::Safe);
        assert_eq!(BooruRating::from_moebooru("q"), BooruRating::Questionable);
        assert_eq!(BooruRating::from_moebooru("e"), BooruRating::Explicit);
        assert_eq!(BooruRating::from_moebooru("unknown"), BooruRating::Safe);
    }

    #[test]
    fn test_rating_from_danbooru() {
        assert_eq!(BooruRating::from_danbooru("g"), BooruRating::General);
        assert_eq!(BooruRating::from_danbooru("s"), BooruRating::Safe);
        assert_eq!(BooruRating::from_danbooru("q"), BooruRating::Questionable);
        assert_eq!(BooruRating::from_danbooru("e"), BooruRating::Explicit);
        assert_eq!(BooruRating::from_danbooru("x"), BooruRating::Safe);
    }

    #[test]
    fn test_rating_from_gelbooru() {
        assert_eq!(BooruRating::from_gelbooru("general"), BooruRating::General);
        assert_eq!(BooruRating::from_gelbooru("sensitive"), BooruRating::Safe);
        assert_eq!(
            BooruRating::from_gelbooru("questionable"),
            BooruRating::Questionable
        );
        assert_eq!(
            BooruRating::from_gelbooru("explicit"),
            BooruRating::Explicit
        );
        assert_eq!(BooruRating::from_gelbooru("???"), BooruRating::Safe);
    }

    #[test]
    fn test_rating_short_str() {
        assert_eq!(BooruRating::General.as_short_str(), "g");
        assert_eq!(BooruRating::Safe.as_short_str(), "s");
        assert_eq!(BooruRating::Questionable.as_short_str(), "q");
        assert_eq!(BooruRating::Explicit.as_short_str(), "e");
    }

    #[test]
    fn test_danbooru_raw_post_deserialization() {
        let json = r#"{
            "id": 7654321,
            "tag_string": "1girl solo blue_eyes",
            "score": 100,
            "fav_count": 50,
            "file_url": "https://cdn.donmai.us/original/ab/cd/abcdef.jpg",
            "large_file_url": "https://cdn.donmai.us/sample/ab/cd/sample-abcdef.jpg",
            "preview_file_url": "https://cdn.donmai.us/preview/ab/cd/abcdef.jpg",
            "rating": "g",
            "image_width": 1920,
            "image_height": 1080,
            "md5": "abcdef123456",
            "source": "https://pixiv.net/artworks/12345",
            "created_at": "2024-06-01T12:00:00.000+00:00",
            "file_size": 1024000,
            "file_ext": "png",
            "is_deleted": false,
            "is_banned": false
        }"#;

        let raw: DanbooruRawPost = serde_json::from_str(json).unwrap();
        let post = raw.into_booru_post();
        assert_eq!(post.id, 7654321);
        assert_eq!(post.tags, "1girl solo blue_eyes");
        assert_eq!(post.rating, BooruRating::General);
        assert_eq!(post.score, 100);
        assert_eq!(post.fav_count, 50);
        assert_eq!(post.width, 1920);
        assert!(post.created_at.is_some());
        assert_eq!(post.status.as_deref(), Some("active"));
    }

    #[test]
    fn test_gelbooru_raw_post_deserialization() {
        let json = r#"{
            "id": 9999999,
            "tags": "cat animal cute",
            "score": 10,
            "file_url": "https://img3.gelbooru.com/images/ab/cd/abcdef.jpg",
            "sample_url": "https://img3.gelbooru.com/samples/ab/cd/sample_abcdef.jpg",
            "preview_url": "https://img3.gelbooru.com/thumbnails/ab/cd/thumbnail_abcdef.jpg",
            "rating": "general",
            "width": 800,
            "height": 600,
            "hash": "aabbccdd",
            "source": "",
            "created_at": "2024-01-15 10:30:00"
        }"#;

        let raw: GelbooruRawPost = serde_json::from_str(json).unwrap();
        let post = raw.into_booru_post();
        assert_eq!(post.id, 9999999);
        assert_eq!(post.tags, "cat animal cute");
        assert_eq!(post.rating, BooruRating::General);
        assert_eq!(post.md5.as_deref(), Some("aabbccdd"));
        assert!(post.created_at.is_some());
        assert_eq!(post.fav_count, 0); // Gelbooru fav_count is optional
    }

    #[test]
    fn test_gelbooru_posts_response_wrapper() {
        let json = r#"{"post": [
            {"id": 1, "tags": "a", "score": 5, "rating": "general", "width": 100, "height": 100},
            {"id": 2, "tags": "b", "score": 10, "rating": "sensitive", "width": 200, "height": 200}
        ]}"#;

        let resp: GelbooruPostsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.post.len(), 2);
        assert_eq!(resp.post[0].id, 1);
        assert_eq!(resp.post[1].id, 2);
    }

    #[test]
    fn test_moebooru_raw_pool_deserialization() {
        let json = r#"{
            "id": 12345,
            "name": "Test_Pool",
            "post_count": 3,
            "description": "A test pool",
            "posts": [
                {"id": 100, "tags": "a", "score": 1, "rating": "s", "width": 100, "height": 100},
                {"id": 101, "tags": "b", "score": 2, "rating": "q", "width": 200, "height": 200},
                {"id": 102, "tags": "c", "score": 3, "rating": "e", "width": 300, "height": 300}
            ]
        }"#;

        let raw: MoebooruRawPool = serde_json::from_str(json).unwrap();
        let pool = raw.into_pool_info();
        assert_eq!(pool.id, 12345);
        assert_eq!(pool.name, "Test_Pool");
        assert_eq!(pool.post_count, 3);
        assert_eq!(pool.post_ids, vec![100, 101, 102]);
        assert_eq!(pool.description.as_deref(), Some("A test pool"));
    }

    #[test]
    fn test_danbooru_raw_pool_deserialization() {
        let json = r#"{
            "id": 67890,
            "name": "another_pool",
            "post_count": 2,
            "post_ids": [500, 501],
            "description": "",
            "created_at": "2024-03-15T08:00:00.000+00:00"
        }"#;

        let raw: DanbooruRawPool = serde_json::from_str(json).unwrap();
        let pool = raw.into_pool_info();
        assert_eq!(pool.id, 67890);
        assert_eq!(pool.post_ids, vec![500, 501]);
        assert!(pool.created_at.is_some());
    }

    #[test]
    fn test_booru_post_serde_roundtrip() {
        let post = BooruPost {
            id: 42,
            tags: "test tag".to_string(),
            score: 10,
            fav_count: 5,
            file_url: Some("https://example.com/file.jpg".to_string()),
            sample_url: Some("https://example.com/sample.jpg".to_string()),
            preview_url: None,
            rating: BooruRating::Safe,
            width: 1920,
            height: 1080,
            md5: Some("abcdef".to_string()),
            source: None,
            created_at: None,
            file_size: Some(1024),
            file_ext: Some("jpg".to_string()),
            status: Some("active".to_string()),
        };

        let json = serde_json::to_string(&post).unwrap();
        let parsed: BooruPost = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 42);
        assert_eq!(parsed.tags, "test tag");
        assert_eq!(parsed.rating, BooruRating::Safe);
    }
}
