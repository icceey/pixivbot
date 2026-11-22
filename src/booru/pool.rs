use super::BooruSiteRegistry;
use anyhow::{bail, ensure, Context, Result};
use booru_client::{BooruEngineType, BooruPost, Error};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};

/// Resume a pool image batch for the same site and pool ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PoolCursor {
    Danbooru {
        remaining: VecDeque<u64>,
        pending_images: VecDeque<(String, String)>,
    },
    Moebooru {
        next_page: u32,
        post_count: Option<usize>,
        seen: HashSet<u64>,
        pending_images: VecDeque<(String, String)>,
    },
}

/// Fetch up to `limit` original images in pool order. `limit` must be positive.
/// A scan can return fewer images (including none) with a cursor to resume.
pub async fn pool_image_batch(
    registry: &BooruSiteRegistry,
    site: &str,
    id: u64,
    cursor: Option<PoolCursor>,
    limit: usize,
) -> Result<(Vec<(String, String)>, Option<PoolCursor>)> {
    let site = registry.get(site).context("Booru site is not configured")?;
    ensure!(limit > 0, "Pool image batch limit must be positive");
    let engine = site.client.engine_type();
    let mut cursor = match cursor {
        Some(cursor) => cursor,
        None => match engine {
            BooruEngineType::Danbooru => {
                let pool = site.client.get_pool(id).await?;
                let mut seen = HashSet::new();
                if pool.post_ids.len() < pool.post_count as usize
                    || pool.post_ids.iter().any(|&id| !seen.insert(id))
                {
                    return Err(Error::Api {
                        message: format!("Danbooru pool {id} has inconsistent post IDs"),
                        status: 0,
                    }
                    .into());
                }
                PoolCursor::Danbooru {
                    remaining: pool.post_ids.into(),
                    pending_images: VecDeque::new(),
                }
            }
            BooruEngineType::Moebooru => PoolCursor::Moebooru {
                next_page: 1,
                post_count: None,
                seen: HashSet::new(),
                pending_images: VecDeque::new(),
            },
            BooruEngineType::Gelbooru => {
                return Err(Error::Api {
                    message: "Gelbooru does not support pool queries".to_string(),
                    status: 0,
                }
                .into());
            }
        },
    };

    let mut images = Vec::new();
    let has_more = match (engine, &mut cursor) {
        (
            BooruEngineType::Danbooru,
            PoolCursor::Danbooru {
                remaining,
                pending_images,
            },
        ) => {
            const POST_BUDGET: usize = 100;
            let mut posts_left = POST_BUDGET;
            loop {
                let count = (limit - images.len()).min(pending_images.len());
                images.extend(pending_images.drain(..count));
                if images.len() == limit || remaining.is_empty() || posts_left == 0 {
                    break;
                }
                // Only broaden the request after encountering non-images.
                let count = if posts_left == POST_BUDGET {
                    limit - images.len()
                } else {
                    posts_left
                }
                .min(remaining.len())
                .min(posts_left);
                let ids: Vec<_> = remaining.drain(..count).collect();
                for post in site.client.get_danbooru_pool_segment(id, &ids).await? {
                    if let Some(image) = original_image(post)? {
                        pending_images.push_back(image);
                    }
                }
                posts_left -= count;
            }
            !pending_images.is_empty() || !remaining.is_empty()
        }
        (
            BooruEngineType::Moebooru,
            PoolCursor::Moebooru {
                next_page,
                post_count,
                seen,
                pending_images,
            },
        ) => {
            let mut pages_left = 5;
            loop {
                let count = (limit - images.len()).min(pending_images.len());
                images.extend(pending_images.drain(..count));
                if images.len() == limit || *post_count == Some(seen.len()) || pages_left == 0 {
                    break;
                }
                let (count, posts) = site
                    .client
                    .get_moebooru_pool_posts_page(id, *next_page)
                    .await?;
                let expected = *post_count.get_or_insert(count as usize);
                let fetched = seen.len();
                if count as usize != expected
                    || fetched + posts.len() > expected
                    || (posts.is_empty() && fetched < expected)
                    || posts.iter().any(|post| !seen.insert(post.id))
                {
                    return Err(Error::Api {
                        message: format!(
                            "Moebooru pool {id} returned an inconsistent page {next_page} ({fetched}/{expected} posts fetched)"
                        ),
                        status: 0,
                    }
                    .into());
                }
                for post in posts {
                    if let Some(image) = original_image(post)? {
                        pending_images.push_back(image);
                    }
                }
                *next_page += 1;
                pages_left -= 1;
            }
            !pending_images.is_empty() || *post_count != Some(seen.len())
        }
        _ => bail!("Pool cursor does not match the Booru engine"),
    };
    Ok((images, has_more.then_some(cursor)))
}

fn original_image(post: BooruPost) -> Result<Option<(String, String)>> {
    let parsed_url = post
        .file_url
        .as_deref()
        .and_then(|s| url::Url::parse(s).ok());
    let ext = post
        .file_ext
        .as_deref()
        .or_else(|| {
            parsed_url
                .as_ref()?
                .path_segments()?
                .next_back()?
                .rsplit_once('.')
                .map(|(_, ext)| ext)
        })
        .ok_or_else(|| Error::Api {
            message: format!("Missing original file type for post {}", post.id),
            status: 0,
        })?
        .to_ascii_lowercase();
    if !matches!(
        ext.as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif" | "jxl" | "bmp"
    ) {
        return Ok(None);
    }
    let url = post.file_url.ok_or_else(|| Error::Api {
        message: format!("Missing original for post {}", post.id),
        status: 0,
    })?;
    Ok(Some((format!("{}.{ext}", post.id), url)))
}
