use super::PixivGallery;
use crate::config::PixivConfig;
use anyhow::{anyhow, Context, Result};
use pixiv_client::{self, Illust};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use tracing::info;

// Pixiv's publication dates use Japan Standard Time, independent of the host TZ.
pub const SEARCH_TIMEZONE: chrono::FixedOffset = chrono::FixedOffset::east_opt(9 * 3600).unwrap();

#[derive(Clone)]
pub struct PixivClient {
    client: Arc<pixiv_client::PixivClient>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GalleryCursor {
    next: SourcePage,
    work_ids: VecDeque<u64>,
    works: VecDeque<OriginalWork>,
    pixivision_seen: HashSet<u64>,
    tag_anchor: Option<u64>,
}

impl GalleryCursor {
    fn queue_static_works(&mut self, illusts: impl IntoIterator<Item = Illust>) {
        self.works
            .extend(illusts.into_iter().filter_map(OriginalWork::from_illust));
    }

    fn queue_tag_page(
        &mut self,
        page: pixiv_client::SearchIllusts,
        offset: u32,
    ) -> Result<Option<u32>> {
        let next = next_offset(page.next_url, offset, "Tag")?;
        let last_id = page.illusts.last().map(|work| work.id);
        let mut works = page.illusts.into_iter();
        if offset > 0 {
            let anchor = self
                .tag_anchor
                .context("Missing previous tag result for pagination")?;
            anyhow::ensure!(
                works.any(|work| work.id == anchor),
                "Pixiv tag results changed at offset {offset}; restart the export"
            );
        }
        self.tag_anchor = if next.is_some() {
            Some(last_id.context("Empty tag page has no continuation boundary")?)
        } else {
            None
        };
        self.queue_static_works(works);
        Ok(next)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
enum SourcePage {
    #[default]
    First,
    Next(u64),
    End,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OriginalWork {
    id: u64,
    page: u32,
    page_count: u32,
    originals: VecDeque<Option<String>>,
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

fn next_offset(next_url: Option<String>, current: u32, label: &str) -> Result<Option<u32>> {
    next_url
        .map(|url| {
            let offset = u32::try_from(pagination_value(&url, "offset", label)?)?;
            anyhow::ensure!(offset > current, "{label} pagination did not advance");
            Ok(offset)
        })
        .transpose()
}

fn image_extension(url: &str, illust_id: u64, page: u32) -> Result<String> {
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

impl OriginalWork {
    fn from_illust(illust: Illust) -> Option<Self> {
        if !matches!(illust.illust_type.as_str(), "illust" | "manga") {
            return None;
        }
        let originals = if illust.page_count == 1 {
            VecDeque::from([illust
                .meta_single_page
                .original_image_url
                .or(illust.image_urls.original)])
        } else {
            illust
                .meta_pages
                .into_iter()
                .map(|page| page.image_urls.original)
                .collect()
        };
        Some(Self {
            id: illust.id,
            page: 0,
            page_count: illust.page_count,
            originals,
        })
    }

    fn append_images(&mut self, images: &mut Vec<(String, String)>, limit: usize) -> Result<()> {
        anyhow::ensure!(self.page_count > 0, "Static work {} has no pages", self.id);
        while self.page < self.page_count && images.len() < limit {
            let original = self.originals.pop_front().flatten().with_context(|| {
                format!("Missing original for work {} page {}", self.id, self.page)
            })?;
            let extension = image_extension(&original, self.id, self.page)?;
            images.push((format!("{}_p{}.{extension}", self.id, self.page), original));
            self.page += 1;
        }
        Ok(())
    }
}

impl PixivClient {
    pub fn new(config: PixivConfig) -> Result<Self> {
        let client = pixiv_client::PixivClient::new(config.refresh_token)?;

        Ok(Self {
            client: Arc::new(client),
        })
    }

    /// Login using refresh token
    pub async fn login(&mut self) -> Result<()> {
        self.client.login().await?;

        info!("✅ Pixiv authentication successful");
        Ok(())
    }

    /// Returns at most `limit` originals. A sparse source can yield a short or empty
    /// batch with a continuation when its metadata scan budget is spent.
    pub async fn gallery_image_batch(
        &self,
        source: &PixivGallery,
        cursor: Option<GalleryCursor>,
        limit: usize,
    ) -> Result<(Vec<(String, String)>, Option<GalleryCursor>)> {
        let mut cursor = cursor.unwrap_or_default();
        let mut images = Vec::new();
        let mut details_left = 100;
        let mut lists_left = 5;
        while images.len() < limit {
            if let Some(work) = cursor.works.front_mut() {
                work.append_images(&mut images, limit)?;
                if work.page == work.page_count {
                    cursor.works.pop_front();
                }
            } else if let Some(&id) = cursor.work_ids.front() {
                if details_left == 0 {
                    break;
                }
                details_left -= 1;
                let detail = self.client.illust_detail(id).await?;
                cursor.work_ids.pop_front();
                cursor.queue_static_works([detail.illust]);
            } else {
                if lists_left == 0 {
                    break;
                }
                let position = match cursor.next {
                    SourcePage::First => None,
                    SourcePage::Next(n) => Some(n),
                    SourcePage::End => break,
                };
                lists_left -= 1;
                self.load_gallery_page(source, position, &mut cursor)
                    .await?;
            }
        }
        let more = !cursor.works.is_empty()
            || !cursor.work_ids.is_empty()
            || !matches!(cursor.next, SourcePage::End);
        Ok((images, more.then_some(cursor)))
    }

    async fn load_gallery_page(
        &self,
        source: &PixivGallery,
        position: Option<u64>,
        cursor: &mut GalleryCursor,
    ) -> Result<()> {
        let next = match source {
            PixivGallery::Collection(id) => {
                cursor.work_ids = self.client.collection_work_ids(id).await?.into();
                None
            }
            PixivGallery::Author(id) => {
                cursor.work_ids = self.client.author_work_ids(*id).await?.into();
                None
            }
            PixivGallery::Pixivision { lang, id } => {
                let page = u32::try_from(position.unwrap_or(1))?;
                let (ids, next) = self.client.pixivision_work_page(lang, *id, page).await?;
                cursor.work_ids.extend(
                    ids.into_iter()
                        .filter(|id| cursor.pixivision_seen.insert(*id)),
                );
                next.map(u64::from)
            }
            PixivGallery::Bookmarks(id) => {
                let response = self.client.user_bookmarks_illust(*id, position).await?;
                cursor.queue_static_works(response.illusts);
                response
                    .next_url
                    .map(|url| -> Result<u64> {
                        let next = pagination_value(&url, "max_bookmark_id", "Bookmark")?;
                        anyhow::ensure!(
                            position.is_none_or(|p| next < p),
                            "Bookmark pagination did not advance"
                        );
                        Ok(next)
                    })
                    .transpose()?
            }
            PixivGallery::Ranking { mode, date, .. } => {
                anyhow::ensure!(
                    matches!(mode.as_str(), "day" | "week" | "month"),
                    "Unsupported ranking mode: {mode}"
                );
                let offset = position.map(u32::try_from).transpose()?;
                let response = self
                    .client
                    .illust_ranking(mode, Some(&date.to_string()), offset)
                    .await?;
                cursor.queue_static_works(response.illusts);
                next_offset(response.next_url, offset.unwrap_or(0), "Ranking")?.map(u64::from)
            }
            PixivGallery::Tag(word) => {
                let offset = u32::try_from(position.unwrap_or(0))?;
                // Re-read the boundary: newer works may have shifted the prior page since delivery.
                let response = self
                    .client
                    .search_tag_latest(word, offset.saturating_sub(1))
                    .await?;
                cursor.queue_tag_page(response, offset)?.map(u64::from)
            }
        };
        cursor.next = next.map_or(SourcePage::End, SourcePage::Next);
        Ok(())
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
        let mut work = OriginalWork::from_illust(three_page_illust()).unwrap();
        let mut images = Vec::new();
        work.append_images(&mut images, 2).unwrap();
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

        assert!(work.append_images(&mut Vec::new(), 1).is_err());
    }

    #[test]
    fn latest_tag_resumes_after_the_shifted_raw_boundary_or_rejects_the_page() {
        let page = |ids: &[u64], next: Option<u32>| pixiv_client::SearchIllusts {
            illusts: ids
                .iter()
                .map(|&id| {
                    let mut work = three_page_illust();
                    work.id = id;
                    if id == 7 {
                        work.illust_type = "ugoira".into();
                    }
                    work
                })
                .collect(),
            next_url: next.map(|offset| {
                format!("https://app-api.pixiv.net/v1/search/illust?offset={offset}")
            }),
        };
        let mut cursor = GalleryCursor::default();
        let offset = cursor
            .queue_tag_page(page(&[9, 8, 7], Some(3)), 0)
            .unwrap()
            .unwrap();
        let saved = serde_json::to_string(&cursor).unwrap();
        let mut cursor: GalleryCursor = serde_json::from_str(&saved).unwrap();
        assert!(cursor
            .queue_tag_page(page(&[8, 6, 5], Some(5)), offset)
            .is_err());

        // A new result pushed work 7 (a skipped animation) one slot past the overlap.
        let offset = cursor
            .queue_tag_page(page(&[8, 7, 6], Some(5)), offset)
            .unwrap()
            .unwrap();
        assert_eq!(offset, 5);
        assert!(cursor
            .queue_tag_page(page(&[6, 5], None), offset)
            .unwrap()
            .is_none());
        assert_eq!(
            cursor
                .works
                .into_iter()
                .map(|work| work.id)
                .collect::<Vec<_>>(),
            [9, 8, 6, 5]
        );
    }

    #[tokio::test]
    async fn resumed_batch_finishes_manga_then_consumes_the_list_tail() {
        let mut manga = three_page_illust();
        manga.meta_pages[2].image_urls.original = Some("https://i.pximg.net/123_p2.jpg".into());
        let mut single = three_page_illust();
        single.id = 456;
        single.page_count = 1;
        single.meta_pages.clear();
        single.image_urls.original = Some("https://i.pximg.net/456_p0.png".into());
        let mut cursor = GalleryCursor {
            next: SourcePage::End,
            ..Default::default()
        };
        cursor.queue_static_works([manga, single]);
        let client = PixivClient::new(PixivConfig {
            refresh_token: String::new(),
        })
        .unwrap();
        let source = PixivGallery::Bookmarks(1);
        let (first, next) = client
            .gallery_image_batch(&source, Some(cursor), 2)
            .await
            .unwrap();
        assert_eq!(first.len(), 2);
        let saved = serde_json::to_string(&next.unwrap()).unwrap();
        let cursor = serde_json::from_str(&saved).unwrap();
        let (second, end) = client
            .gallery_image_batch(&source, Some(cursor), 2)
            .await
            .unwrap();
        assert_eq!(
            second,
            vec![
                ("123_p2.jpg".into(), "https://i.pximg.net/123_p2.jpg".into()),
                ("456_p0.png".into(), "https://i.pximg.net/456_p0.png".into())
            ]
        );
        assert!(end.is_none());
    }
}
