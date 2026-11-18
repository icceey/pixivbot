use crate::error::{Error, Result};
use reqwest::Url;
use scraper::{Html, Selector};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::collections::HashSet;

#[derive(Debug, Deserialize)]
struct AjaxEnvelope {
    error: bool,
    message: String,
    body: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct CollectionBody {
    data: CollectionData,
}

#[derive(Debug, Deserialize)]
struct CollectionData {
    detail: CollectionDetail,
}

#[derive(Debug, Deserialize)]
struct CollectionDetail {
    tiles: Vec<CollectionTile>,
}

#[derive(Debug, Deserialize)]
struct CollectionTile {
    layout: TileLayout,
    status: String,
    #[serde(rename = "type")]
    tile_type: String,
    #[serde(rename = "workType")]
    work_type: Option<String>,
    #[serde(rename = "workId")]
    work_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TileLayout {
    position: TilePosition,
}

#[derive(Debug, Deserialize)]
struct TilePosition {
    x: u32,
    y: u32,
}

#[derive(Debug, Deserialize)]
struct ProfileAllBody {
    illusts: serde_json::Value,
    manga: serde_json::Value,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PixivisionPage {
    pub work_ids: Vec<u64>,
    pub next_page: Option<u32>,
}

fn ajax_body<T: DeserializeOwned>(text: &str) -> Result<T> {
    let response: AjaxEnvelope = serde_json::from_str(text)?;
    if response.error {
        return Err(Error::Other(format!(
            "Pixiv web API error: {}",
            response.message
        )));
    }
    Ok(serde_json::from_value(response.body)?)
}

pub(crate) fn collection_work_ids(text: &str) -> Result<Vec<u64>> {
    let body: CollectionBody = ajax_body(text)?;
    let mut tiles: Vec<_> = body
        .data
        .detail
        .tiles
        .into_iter()
        .filter(|tile| {
            tile.status == "Active"
                && tile.tile_type == "Work"
                && tile.work_type.as_deref() == Some("illust")
        })
        .collect();
    tiles.sort_by_key(|tile| (tile.layout.position.y, tile.layout.position.x));
    let mut work_ids = tiles
        .into_iter()
        .map(|tile| {
            tile.work_id
                .ok_or_else(|| Error::Other("Active collection work has no workId".to_string()))?
                .parse()
                .map_err(|error| Error::Other(format!("Invalid collection workId: {error}")))
        })
        .collect::<Result<Vec<u64>>>()?;
    let mut seen = HashSet::new();
    work_ids.retain(|work_id| seen.insert(*work_id));
    Ok(work_ids)
}

fn profile_work_ids(value: serde_json::Value, category: &str) -> Result<Vec<u64>> {
    let keys = match value {
        serde_json::Value::Object(works) => works.into_iter().map(|(id, _)| id).collect(),
        serde_json::Value::Array(works) if works.is_empty() => Vec::new(),
        _ => {
            return Err(Error::Other(format!(
                "Unexpected author {category} work list"
            )))
        }
    };
    keys.into_iter()
        .map(|id| {
            id.parse()
                .map_err(|error| Error::Other(format!("Invalid author work ID: {error}")))
        })
        .collect()
}

pub(crate) fn author_work_ids(text: &str) -> Result<Vec<u64>> {
    let body: ProfileAllBody = ajax_body(text)?;
    let mut work_ids = profile_work_ids(body.illusts, "illust")?;
    work_ids.extend(profile_work_ids(body.manga, "manga")?);
    work_ids.sort_unstable_by_key(|id| std::cmp::Reverse(*id));
    Ok(work_ids)
}

fn pixiv_work_id(current_url: &Url, href: &str) -> Result<u64> {
    let url = current_url
        .join(href)
        .map_err(|error| Error::Other(format!("Invalid pixivision work URL: {error}")))?;
    if !matches!(url.host_str(), Some("www.pixiv.net" | "pixiv.net")) {
        return Err(Error::Other(format!(
            "Unexpected pixivision work host: {url}"
        )));
    }
    let segments: Vec<_> = url
        .path_segments()
        .ok_or_else(|| Error::Other(format!("Invalid pixivision work URL: {url}")))?
        .filter(|segment| !segment.is_empty())
        .collect();
    let id = match segments.as_slice() {
        ["artworks", id] | [_, "artworks", id] => *id,
        _ => {
            return Err(Error::Other(format!(
                "Unexpected pixivision work URL: {url}"
            )))
        }
    };
    id.parse()
        .map_err(|error| Error::Other(format!("Invalid pixivision work ID: {error}")))
}

fn next_pixivision_page(
    current_url: &Url,
    href: &str,
    lang: &str,
    article_id: u64,
    current_page: u32,
) -> Result<u32> {
    let url = current_url
        .join(href)
        .map_err(|error| Error::Other(format!("Invalid pixivision next-page URL: {error}")))?;
    let expected_path = format!("/{lang}/a/{article_id}");
    if url.origin() != current_url.origin() || url.path() != expected_path {
        return Err(Error::Other(format!(
            "Unexpected pixivision next-page URL: {url}"
        )));
    }
    let query: Vec<_> = url.query_pairs().collect();
    if query.len() != 1 || query[0].0 != "p" {
        return Err(Error::Other(format!(
            "Unexpected pixivision next-page query: {url}"
        )));
    }
    let page: u32 = query[0]
        .1
        .parse()
        .map_err(|error| Error::Other(format!("Invalid pixivision page number: {error}")))?;
    if page != current_page + 1 {
        return Err(Error::Other(format!(
            "Pixivision pagination did not advance by one: {url}"
        )));
    }
    Ok(page)
}

pub(crate) fn pixivision_page(
    html: &str,
    current_url: &Url,
    lang: &str,
    article_id: u64,
    current_page: u32,
) -> Result<PixivisionPage> {
    let document = Html::parse_document(html);
    let body_selector =
        Selector::parse("._feature-article-body").expect("valid pixivision body selector");
    let body = document
        .select(&body_selector)
        .next()
        .ok_or_else(|| Error::Other("Pixivision article body not found".to_string()))?;

    let cards = Selector::parse("._feature-article-body__pixiv_illust").unwrap();
    let title_link = Selector::parse(".am__work__title a[href]").unwrap();
    let work_ids = body
        .select(&cards)
        .map(|card| {
            let link = card.select(&title_link).next().ok_or_else(|| {
                Error::Other("Pixivision illustration card has no artwork link".into())
            })?;
            pixiv_work_id(current_url, link.attr("href").unwrap())
        })
        .collect::<Result<Vec<_>>>()?;

    let next_selector = Selector::parse("._feature-article-body__pager .next-page-link a[href]")
        .expect("valid pixivision next-page selector");
    let next_page = body
        .select(&next_selector)
        .next()
        .map(|element| {
            next_pixivision_page(
                current_url,
                element.attr("href").unwrap(),
                lang,
                article_id,
                current_page,
            )
        })
        .transpose()?;

    Ok(PixivisionPage {
        work_ids,
        next_page,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_uses_visual_work_order_and_excludes_non_illust_tiles() {
        let response = r#"{
            "error": false,
            "message": "",
            "body": {"data":{"detail":{"tiles":[
                {"layout":{"position":{"x":0,"y":0}},"status":"Active","type":"Work","workType":"illust","workId":"101092995"},
                {"layout":{"position":{"x":0,"y":12}},"status":"Active","type":"Work","workType":"illust","workId":"126740576"},
                {"layout":{"position":{"x":2,"y":6}},"status":"Active","type":"Work","workType":"illust","workId":"124619883"},
                {"layout":{"position":{"x":2,"y":18}},"status":"Active","type":"Work","workType":"illust","workId":"120906610"},
                {"layout":{"position":{"x":0,"y":24}},"status":"Active","type":"Work","workType":"illust","workId":"129438870"},
                {"layout":{"position":{"x":2,"y":24}},"status":"Active","type":"Work","workType":"illust","workId":"101092995"},
                {"layout":{"position":{"x":1,"y":1}},"status":"Active","type":"Work","workType":"novel","workId":"999"},
                {"layout":{"position":{"x":1,"y":2}},"status":"Active","type":"ExternalUrl"},
                {"layout":{"position":{"x":1,"y":3}},"status":"Inactive","type":"Work","workType":"illust","workId":"998"}
            ]}}}
        }"#;

        assert_eq!(
            collection_work_ids(response).unwrap(),
            vec![101092995, 124619883, 126740576, 120906610, 129438870]
        );
    }

    #[test]
    fn author_profile_accepts_empty_array_categories_and_merges_by_display_order() {
        let response = r#"{
            "error": false,
            "message": "",
            "body": {"illusts":{"57899873":null,"60000000":null},"manga":[]}
        }"#;
        assert_eq!(author_work_ids(response).unwrap(), vec![60000000, 57899873]);
    }

    #[test]
    fn pixivision_scopes_real_article_body_and_follows_its_pager() {
        let html = r#"
            <div class="_feature-article-body__pixiv_illust">
              <div class="am__work__title"><a href="https://www.pixiv.net/artworks/999">outside</a></div>
            </div>
            <div class="_feature-article-body__pager"><div class="next-page-link">
              <a href="/ja/a/999?p=2">outside pager</a>
            </div></div>
            <div class="am__article-body-container"><div class="am__body">
              <div class="_feature-article-body">
                <div class="article-item _feature-article-body__paragraph"></div>
                <div class="article-item _feature-article-body__pixiv_illust"><div class="am__work">
                  <div class="am__work__info"><div class="am__work__title">
                    <a href="https://www.pixiv.net/en/artworks/101162440?utm_source=pixivision">work</a>
                  </div></div>
                </div></div>
                <div class="article-item _feature-article-body__pager"><div class="next-page-link">
                  <a href="/ja/a/8200?p=2">next</a>
                </div></div>
              </div><div class="am__footer"></div>
            </div><div class="next-page-area"><a href="/ja/a/8200?p=1">1</a><a href="/ja/a/8200?p=2">2</a></div></div>
        "#;
        let url = Url::parse("https://www.pixivision.net/ja/a/8200").unwrap();
        let page = pixivision_page(html, &url, "ja", 8200, 1).unwrap();
        assert_eq!(page.work_ids, vec![101162440]);
        assert_eq!(page.next_page, Some(2));
    }

    #[test]
    fn pixivision_continuation_ignores_navigation_outside_the_article_body() {
        let html = r#"
            <div class="am__article-body-container"><div class="am__body">
              <div class="_feature-article-body">
                <div class="article-item _feature-article-body__pixiv_illust">
                  <div class="am__work__title"><a href="https://www.pixiv.net/artworks/100993883">work</a></div>
                </div>
              </div><div class="am__footer"></div>
            </div><div class="next-page-area"><a href="/ja/a/8200?p=1">1</a><a href="/ja/a/8200?p=2">2</a></div></div>
        "#;
        let url = Url::parse("https://www.pixivision.net/ja/a/8200?p=2").unwrap();
        let page = pixivision_page(html, &url, "ja", 8200, 2).unwrap();
        assert_eq!(page.work_ids, vec![100993883]);
        assert_eq!(page.next_page, None);
    }

    #[test]
    fn pixivision_rejects_missing_article_body() {
        let html = r#"
            <div class="_feature-article-body__pixiv_illust">
              <div class="am__work__title"><a href="https://www.pixiv.net/artworks/999">outside</a></div>
            </div>
        "#;
        let url = Url::parse("https://www.pixivision.net/ja/a/8200").unwrap();
        assert!(pixivision_page(html, &url, "ja", 8200, 1).is_err());
    }

    #[test]
    fn pixivision_does_not_report_unreadable_work_cards_as_an_empty_article() {
        let html = r#"<div class="_feature-article-body"><div class="_feature-article-body__pixiv_illust"><div class="aiwsp__info"></div></div></div>"#;
        let url = Url::parse("https://www.pixivision.net/ja/a/8200").unwrap();
        assert!(pixivision_page(html, &url, "ja", 8200, 1).is_err());
    }

    #[test]
    fn pixivision_rejects_next_page_outside_the_same_article() {
        let html = r#"
            <div class="_feature-article-body">
              <div class="article-item _feature-article-body__pager"><div class="next-page-link">
                <a href="https://example.com/ja/a/8200?p=2">next</a>
              </div></div>
            </div>
        "#;
        let url = Url::parse("https://www.pixivision.net/ja/a/8200").unwrap();
        assert!(pixivision_page(html, &url, "ja", 8200, 1).is_err());
    }
}
