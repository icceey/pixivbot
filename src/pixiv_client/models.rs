//! Pixiv API 数据模型
//! 
//! 只包含项目需要的字段，参考 [pixivpy](https://github.com/upbit/pixivpy) 的 pixivpy3/models.py

use serde::{Deserialize, Serialize};

/// 用户信息
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct User {
    pub id: u64,
    pub name: String,
    pub account: String,
    #[serde(default)]
    pub is_followed: Option<bool>,
}

/// 图片 URL
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImageUrls {
    pub square_medium: String,
    pub medium: String,
    pub large: String,
    pub original: Option<String>,
}

/// 单页图片元数据
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MetaSinglePage {
    #[serde(default)]
    pub original_image_url: Option<String>,
}

/// 多页图片的单页
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetaPage {
    pub image_urls: ImageUrls,
}

/// 作品标签
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tag {
    pub name: String,
    #[serde(default)]
    pub translated_name: Option<String>,
}

/// 作品信息
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Illust {
    pub id: u64,
    pub title: String,
    #[serde(rename = "type")]
    pub illust_type: String,
    pub image_urls: ImageUrls,
    pub caption: String,
    pub restrict: u32,
    pub user: User,
    pub tags: Vec<Tag>,
    pub create_date: String,
    pub page_count: u32,
    pub width: u32,
    pub height: u32,
    pub sanity_level: u32,
    pub x_restrict: u32,
    #[serde(default)]
    pub series: Option<serde_json::Value>,
    pub meta_single_page: MetaSinglePage,
    #[serde(default)]
    pub meta_pages: Vec<MetaPage>,
    pub total_view: u64,
    pub total_bookmarks: u64,
    pub is_bookmarked: bool,
    pub visible: bool,
    #[serde(default)]
    pub is_muted: bool,
    #[serde(default)]
    pub total_comments: Option<u64>,
}

impl Illust {
    /// 是否为多图作品
    pub fn is_multi_page(&self) -> bool {
        self.page_count > 1
    }
    
    /// 获取所有图片的原图 URL
    /// 单图返回1个URL,多图返回所有页的URL
    pub fn get_all_image_urls(&self) -> Vec<String> {
        if self.is_multi_page() {
            // 多图: 从 meta_pages 获取每页的图片
            self.meta_pages
                .iter()
                .map(|page| page.image_urls.original
                    .clone()
                    .unwrap_or_else(|| page.image_urls.large.clone()))
                .collect()
        } else {
            // 单图: 优先使用 original_image_url,否则用 large
            vec![
                self.meta_single_page
                    .original_image_url
                    .clone()
                    .unwrap_or_else(|| self.image_urls.large.clone())
            ]
        }
    }
    
    /// 获取第一张图片的URL (用于缩略图或预览)
    pub fn get_first_image_url(&self) -> String {
        if let Some(original) = &self.meta_single_page.original_image_url {
            original.clone()
        } else {
            self.image_urls.large.clone()
        }
    }
}

/// 作品详情响应
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IllustDetail {
    pub illust: Illust,
}

/// 用户作品列表响应
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UserIllusts {
    pub illusts: Vec<Illust>,
    pub next_url: Option<String>,
}

/// 排行榜响应
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Ranking {
    pub illusts: Vec<Illust>,
    pub next_url: Option<String>,
}
