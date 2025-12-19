//! E-Hentai API 模型定义
//!
//! 基于 E-Hentai JSON API 响应格式

use serde::{Deserialize, Serialize};

/// 画廊分类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Doujinshi,
    Manga,
    #[serde(rename = "artist cg")]
    ArtistCg,
    #[serde(rename = "game cg")]
    GameCg,
    Western,
    #[serde(rename = "non-h")]
    NonH,
    #[serde(rename = "image set")]
    ImageSet,
    Cosplay,
    #[serde(rename = "asian porn")]
    AsianPorn,
    Misc,
    #[serde(other)]
    Unknown,
}

impl Category {
    /// 获取分类的显示名称
    pub fn display_name(&self) -> &'static str {
        match self {
            Category::Doujinshi => "同人志",
            Category::Manga => "漫画",
            Category::ArtistCg => "Artist CG",
            Category::GameCg => "Game CG",
            Category::Western => "西方",
            Category::NonH => "Non-H",
            Category::ImageSet => "图集",
            Category::Cosplay => "Cosplay",
            Category::AsianPorn => "亚洲色情",
            Category::Misc => "杂项",
            Category::Unknown => "未知",
        }
    }

    /// 从字符串解析分类
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "doujinshi" => Some(Category::Doujinshi),
            "manga" => Some(Category::Manga),
            "artistcg" | "artist cg" | "artist_cg" => Some(Category::ArtistCg),
            "gamecg" | "game cg" | "game_cg" => Some(Category::GameCg),
            "western" => Some(Category::Western),
            "non-h" | "nonh" | "non_h" => Some(Category::NonH),
            "imageset" | "image set" | "image_set" => Some(Category::ImageSet),
            "cosplay" => Some(Category::Cosplay),
            "asianporn" | "asian porn" | "asian_porn" => Some(Category::AsianPorn),
            "misc" => Some(Category::Misc),
            _ => None,
        }
    }

    /// 获取所有分类名称
    pub fn all_names() -> Vec<&'static str> {
        vec![
            "doujinshi",
            "manga",
            "artistcg",
            "gamecg",
            "western",
            "non-h",
            "imageset",
            "cosplay",
            "asianporn",
            "misc",
        ]
    }
}

/// 画廊标签
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GalleryTag {
    /// 标签命名空间 (如 artist, female, male, etc.)
    pub namespace: String,
    /// 标签名称
    pub tag: String,
}

impl GalleryTag {
    /// 获取完整标签字符串 (namespace:tag)
    pub fn full_tag(&self) -> String {
        format!("{}:{}", self.namespace, self.tag)
    }
}

/// 画廊元数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GalleryMetadata {
    /// 画廊 ID
    pub gid: u64,
    /// 画廊 Token
    pub token: String,
    /// 归档 Key
    #[serde(default)]
    pub archiver_key: Option<String>,
    /// 标题
    pub title: String,
    /// 日文标题
    #[serde(default)]
    pub title_jpn: Option<String>,
    /// 分类
    pub category: String,
    /// 缩略图 URL
    pub thumb: String,
    /// 上传者
    pub uploader: String,
    /// 发布时间 (Unix 时间戳字符串)
    pub posted: String,
    /// 文件数量
    pub filecount: String,
    /// 文件大小 (字节)
    pub filesize: u64,
    /// 是否已删除
    pub expunged: bool,
    /// 评分
    pub rating: String,
    /// torrent 数量
    pub torrentcount: String,
    /// 父画廊 gid (用于检测更新)
    #[serde(default)]
    pub parent_gid: Option<String>,
    /// 父画廊 key
    #[serde(default)]
    pub parent_key: Option<String>,
    /// 第一个 gid
    #[serde(default)]
    pub first_gid: Option<String>,
    /// 第一个 key
    #[serde(default)]
    pub first_key: Option<String>,
    /// 标签列表 (格式: "namespace:tag")
    #[serde(default)]
    pub tags: Vec<String>,
}

impl GalleryMetadata {
    /// 获取画廊 URL
    pub fn url(&self) -> String {
        format!("https://e-hentai.org/g/{}/{}/", self.gid, self.token)
    }

    /// 获取 ExHentai URL
    pub fn exhentai_url(&self) -> String {
        format!("https://exhentai.org/g/{}/{}/", self.gid, self.token)
    }

    /// 获取文件数量
    pub fn file_count(&self) -> u32 {
        self.filecount.parse().unwrap_or(0)
    }

    /// 获取评分
    pub fn get_rating(&self) -> f32 {
        self.rating.parse().unwrap_or(0.0)
    }

    /// 获取发布时间戳
    pub fn posted_timestamp(&self) -> i64 {
        self.posted.parse().unwrap_or(0)
    }

    /// 解析标签
    pub fn parsed_tags(&self) -> Vec<GalleryTag> {
        self.tags
            .iter()
            .map(|t| {
                let parts: Vec<&str> = t.splitn(2, ':').collect();
                if parts.len() == 2 {
                    GalleryTag {
                        namespace: parts[0].to_string(),
                        tag: parts[1].to_string(),
                    }
                } else {
                    GalleryTag {
                        namespace: "misc".to_string(),
                        tag: t.clone(),
                    }
                }
            })
            .collect()
    }

    /// 获取艺术家标签
    pub fn artists(&self) -> Vec<String> {
        self.parsed_tags()
            .iter()
            .filter(|t| t.namespace == "artist")
            .map(|t| t.tag.clone())
            .collect()
    }

    /// 检查是否包含特定标签
    pub fn has_tag(&self, namespace: &str, tag: &str) -> bool {
        let full_tag = format!("{}:{}", namespace, tag);
        self.tags.iter().any(|t| t == &full_tag)
    }
}

/// API 响应: 画廊元数据列表
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GalleryMetadataResponse {
    #[serde(rename = "gmetadata")]
    pub gmetadata: Vec<GalleryMetadata>,
}

/// 搜索结果页面信息
#[derive(Debug, Clone, Default)]
pub struct SearchResult {
    /// 画廊列表
    pub galleries: Vec<GalleryMetadata>,
    /// 是否还有更多页面
    pub has_next: bool,
    /// 当前页码
    pub page: u32,
    /// 总结果数 (如果可用)
    pub total: Option<u32>,
}

/// 画廊页面图片信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GalleryImage {
    /// 图片索引 (从 1 开始)
    pub index: u32,
    /// 图片 URL
    pub url: String,
    /// 原图 URL (如果可用)
    #[serde(default)]
    pub original_url: Option<String>,
    /// 宽度
    #[serde(default)]
    pub width: Option<u32>,
    /// 高度
    #[serde(default)]
    pub height: Option<u32>,
}

/// 画廊页面列表
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct GalleryPages {
    /// 图片列表
    pub images: Vec<GalleryImage>,
    /// 总页数
    pub total_pages: u32,
}

/// 简化的画廊信息 (用于搜索结果显示)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GalleryInfo {
    /// 画廊 ID
    pub gid: u64,
    /// 画廊 Token
    pub token: String,
    /// 标题
    pub title: String,
    /// 缩略图
    pub thumb: String,
    /// 分类
    pub category: String,
    /// 评分
    pub rating: f32,
    /// 页数
    pub filecount: u32,
    /// 上传者
    pub uploader: String,
    /// 标签
    pub tags: Vec<String>,
}

impl From<GalleryMetadata> for GalleryInfo {
    fn from(meta: GalleryMetadata) -> Self {
        let rating = meta.get_rating();
        let filecount = meta.file_count();
        Self {
            gid: meta.gid,
            token: meta.token,
            title: meta.title,
            thumb: meta.thumb,
            category: meta.category,
            rating,
            filecount,
            uploader: meta.uploader,
            tags: meta.tags,
        }
    }
}
