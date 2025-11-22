use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Illust {
    pub id: u64,
    pub title: String,
    #[serde(rename = "type")]
    pub illust_type: String,
    #[serde(rename = "image_urls")]
    pub image_urls: ImageUrls,
    pub caption: Option<String>,
    pub tags: Vec<Tag>,
    #[serde(rename = "user")]
    pub author: Author,
    #[serde(rename = "page_count")]
    pub page_count: u16,
    #[serde(rename = "meta_pages")]
    pub meta_pages: Option<Vec<MetaPage>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrls {
    #[serde(rename = "square_medium")]
    pub square_medium: String,
    pub medium: String,
    pub large: String,
    pub original: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Author {
    pub id: u64,
    pub name: String,
    pub account: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaPage {
    #[serde(rename = "image_urls")]
    pub image_urls: ImageUrls,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserIllustsResponse {
    #[serde(rename = "illusts")]
    pub illusts: Vec<Illust>,
    #[serde(rename = "next_url")]
    pub next_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResponse {
    #[serde(rename = "access_token")]
    pub access_token: String,
    #[serde(rename = "refresh_token")]
    pub refresh_token: String,
    #[serde(rename = "expires_in")]
    pub expires_in: u32,
}