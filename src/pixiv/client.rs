use super::PixivGallery;
use crate::config::PixivConfig;
use anyhow::{anyhow, Context, Result};
use pixiv_client::{self, Illust};
use tracing::info;

// Pixiv's publication dates use Japan Standard Time, independent of the host TZ.
pub const SEARCH_TIMEZONE: chrono::FixedOffset = chrono::FixedOffset::east_opt(9 * 3600).unwrap();

pub struct PixivClient {
    client: pixiv_client::PixivClient,
}

fn pagination_value(next_url: &str, key: &str, label: &str) -> Result<u64> {
    let next = url::Url::parse(next_url)?;
    next.query_pairs()
        .find(|(name, _)| name == key)
        .ok_or_else(|| anyhow!("{label} pagination has no {key}"))?
        .1
        .parse()
        .with_context(|| format!("Invalid {label} pagination {key}"))
}

fn image_extension(url: &str, illust_id: u64, page: usize) -> Result<String> {
    let parsed = url::Url::parse(url)
        .with_context(|| format!("Invalid original URL for work {illust_id} page {page}"))?;
    let filename = parsed
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .with_context(|| {
            format!("Original URL has no filename for work {illust_id} page {page}")
        })?;
    let extension = filename
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .with_context(|| {
            format!("Original URL has no file type for work {illust_id} page {page}")
        })?;
    if !matches!(
        extension.as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "jxl" | "bmp"
    ) {
        return Err(anyhow!(
            "Unsupported original file type for work {illust_id} page {page}"
        ));
    }
    Ok(extension)
}

fn append_original_images(
    illust: &Illust,
    page_limit: Option<usize>,
    images: &mut Vec<(String, String)>,
) -> Result<()> {
    if !matches!(illust.illust_type.as_str(), "illust" | "manga") {
        return Ok(());
    }
    let page_count = usize::try_from(illust.page_count)?;
    if page_count == 0 {
        return Err(anyhow!("Static work {} has no pages", illust.id));
    }
    let required = page_limit.unwrap_or(page_count).min(page_count);
    for page in 0..required {
        let original = if page_count == 1 {
            illust
                .meta_single_page
                .original_image_url
                .as_deref()
                .or(illust.image_urls.original.as_deref())
        } else {
            illust
                .meta_pages
                .get(page)
                .and_then(|meta| meta.image_urls.original.as_deref())
        }
        .with_context(|| format!("Missing original for work {} page {page}", illust.id))?;
        let extension = image_extension(original, illust.id, page)?;
        images.push((
            format!("{}_p{page}.{extension}", illust.id),
            original.to_owned(),
        ));
    }
    Ok(())
}

impl PixivClient {
    pub fn new(config: PixivConfig) -> Result<Self> {
        let client = pixiv_client::PixivClient::new(config.refresh_token)?;

        Ok(Self { client })
    }

    /// Login using refresh token
    pub async fn login(&mut self) -> Result<()> {
        self.client.login().await?;

        info!("✅ Pixiv authentication successful");
        Ok(())
    }

    /// Enumerate original static images in gallery presentation order.
    pub async fn gallery_images(&self, source: &PixivGallery) -> Result<Vec<(String, String)>> {
        match source {
            PixivGallery::Collection(collection_id) => {
                let work_ids = self.client.collection_work_ids(collection_id).await?;
                self.work_images(&work_ids).await
            }
            PixivGallery::Bookmarks(user_id) => self.bookmark_images(*user_id).await,
            PixivGallery::Author(user_id) => {
                let work_ids = self.client.author_work_ids(*user_id).await?;
                self.work_images(&work_ids).await
            }
            PixivGallery::Ranking { mode, date, limit } => {
                self.ranking_images(mode, *date, *limit).await
            }
            PixivGallery::Pixivision { lang, id } => {
                let work_ids = self.client.pixivision_work_ids(lang, *id).await?;
                self.work_images(&work_ids).await
            }
        }
    }

    async fn work_images(&self, work_ids: &[u64]) -> Result<Vec<(String, String)>> {
        let mut images = Vec::new();
        for &work_id in work_ids {
            let detail = self.client.illust_detail(work_id).await?;
            append_original_images(&detail.illust, None, &mut images)?;
        }
        Ok(images)
    }

    async fn bookmark_images(&self, user_id: u64) -> Result<Vec<(String, String)>> {
        let mut images = Vec::new();
        let mut cursor = None;
        loop {
            let response = self.client.user_bookmarks_illust(user_id, cursor).await?;
            for illust in response.illusts {
                append_original_images(&illust, None, &mut images)?;
            }
            let Some(next_url) = response.next_url else {
                return Ok(images);
            };
            let next_cursor = pagination_value(&next_url, "max_bookmark_id", "Bookmark")?;
            if cursor.is_some_and(|current| next_cursor >= current) {
                return Err(anyhow!("Bookmark pagination did not advance"));
            }
            cursor = Some(next_cursor);
        }
    }

    async fn ranking_images(
        &self,
        mode: &str,
        date: chrono::NaiveDate,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        if !matches!(mode, "day" | "week" | "month") {
            return Err(anyhow!("Unsupported ranking mode: {mode}"));
        }
        let date = date.format("%Y-%m-%d").to_string();
        let mut images = Vec::with_capacity(limit);
        let mut offset = None;
        loop {
            let response = self
                .client
                .illust_ranking(mode, Some(&date), offset)
                .await?;
            for illust in response.illusts {
                let remaining = limit - images.len();
                if remaining == 0 {
                    return Ok(images);
                }
                append_original_images(&illust, Some(remaining), &mut images)?;
            }
            if images.len() == limit {
                return Ok(images);
            }
            let Some(next_url) = response.next_url else {
                return Ok(images);
            };
            let next_offset = pagination_value(&next_url, "offset", "Ranking")?;
            let next_offset = u32::try_from(next_offset).context("Ranking offset is too large")?;
            if offset.is_some_and(|current| next_offset <= current) {
                return Err(anyhow!("Ranking pagination did not advance"));
            }
            offset = Some(next_offset);
        }
    }

    /// Get latest illusts from an author
    pub async fn get_user_illusts(&self, user_id: u64, limit: usize) -> Result<Vec<Illust>> {
        let response = self
            .client
            .user_illusts(user_id, Some("illust"), None)
            .await?;

        let illusts: Vec<_> = response.illusts.into_iter().take(limit).collect();
        Ok(illusts)
    }

    /// Get ranking illusts
    pub async fn get_ranking(
        &self,
        mode: &str,
        date: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Illust>> {
        let response = self.client.illust_ranking(mode, date, None).await?;

        let illusts: Vec<_> = response.illusts.into_iter().take(limit).collect();
        info!("Fetched {} ranking illusts", illusts.len());

        Ok(illusts)
    }

    /// Search an inclusive date range from oldest to newest.
    pub async fn search_illusts(
        &self,
        query: &str,
        start: chrono::NaiveDate,
        end: chrono::NaiveDate,
        offset: u32,
    ) -> Result<(Vec<Illust>, Option<u32>)> {
        let start_date = start.format("%Y-%m-%d").to_string();
        let end_date = end.format("%Y-%m-%d").to_string();
        let response = self
            .client
            .search_illusts(query, &start_date, &end_date, offset)
            .await?;
        let next_offset = response
            .next_url
            .map(|next| -> Result<u32> {
                let next = url::Url::parse(&next)?;
                let offset_value = next
                    .query_pairs()
                    .find(|(key, _)| key == "offset")
                    .ok_or_else(|| anyhow::anyhow!("Search pagination has no offset"))?
                    .1;
                let next_offset: u32 = offset_value.parse()?;
                anyhow::ensure!(next_offset > offset, "Search pagination did not advance");
                Ok(next_offset)
            })
            .transpose()?;
        Ok((response.illusts, next_offset))
    }

    /// Get illust detail by ID
    pub async fn get_illust_detail(&self, illust_id: u64) -> Result<Illust> {
        let response = self.client.illust_detail(illust_id).await?;

        Ok(response.illust)
    }

    /// 获取用户详情
    pub async fn get_user_detail(&self, user_id: u64) -> Result<pixiv_client::User> {
        let response = self.client.user_detail(user_id).await?;

        info!(
            "Successfully fetched user detail: {} ({})",
            response.user.name, response.user.id
        );
        Ok(response.user)
    }

    /// 获取 Ugoira (动图) 元数据
    pub async fn get_ugoira_metadata(
        &self,
        illust_id: u64,
    ) -> Result<pixiv_client::UgoiraMetadataInfo> {
        let response = self.client.ugoira_metadata(illust_id).await?;
        info!("Fetched ugoira metadata for illust {}", illust_id);
        Ok(response.ugoira_metadata)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three_page_illust() -> Illust {
        serde_json::from_value(serde_json::json!({
            "id": 123,
            "title": "work",
            "type": "manga",
            "image_urls": {
                "square_medium": "https://i.pximg.net/square.jpg",
                "medium": "https://i.pximg.net/medium.jpg",
                "large": "https://i.pximg.net/large.jpg",
                "original": null
            },
            "caption": "",
            "restrict": 0,
            "user": {"id": 1, "name": "user", "account": "user"},
            "tags": [],
            "create_date": "2026-01-01T00:00:00+09:00",
            "page_count": 3,
            "width": 100,
            "height": 100,
            "sanity_level": 2,
            "x_restrict": 0,
            "meta_single_page": {},
            "meta_pages": [
                {"image_urls":{"square_medium":"s0","medium":"m0","large":"l0","original":"https://i.pximg.net/img-original/123_p0.jpg"}},
                {"image_urls":{"square_medium":"s1","medium":"m1","large":"l1","original":"https://i.pximg.net/img-original/123_p1.png"}},
                {"image_urls":{"square_medium":"s2","medium":"m2","large":"l2","original":null}}
            ],
            "total_view": 0,
            "total_bookmarks": 0,
            "is_bookmarked": false,
            "visible": true
        }))
        .unwrap()
    }

    #[test]
    fn ranking_page_cap_only_requires_pages_inside_the_limit() {
        let illust = three_page_illust();
        let mut images = Vec::new();
        append_original_images(&illust, Some(2), &mut images).unwrap();
        assert_eq!(
            images,
            vec![
                (
                    "123_p0.jpg".to_string(),
                    "https://i.pximg.net/img-original/123_p0.jpg".to_string()
                ),
                (
                    "123_p1.png".to_string(),
                    "https://i.pximg.net/img-original/123_p1.png".to_string()
                )
            ]
        );

        assert!(append_original_images(&illust, None, &mut Vec::new()).is_err());
    }

    #[test]
    fn single_page_uses_image_urls_original_when_meta_single_page_omits_it() {
        let mut illust = three_page_illust();
        illust.page_count = 1;
        illust.meta_pages.clear();
        illust.image_urls.original =
            Some("https://i.pximg.net/img-original/123_p0.png".to_string());

        let mut images = Vec::new();
        append_original_images(&illust, None, &mut images).unwrap();
        assert_eq!(
            images,
            vec![(
                "123_p0.png".to_string(),
                "https://i.pximg.net/img-original/123_p0.png".to_string()
            )]
        );
    }
}
