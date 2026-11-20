use crate::booru::BooruSiteRegistry;
use crate::bot::link_handler::{parse_pixiv_links, PixivLink};
use crate::bot::{notifier::ThrottledBot, BotHandler};
use crate::pixiv::PixivGallery;
use booru_client::BooruEngineType;
use chrono::NaiveDate;
use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;
use teloxide::prelude::*;
use teloxide::types::MessageEntityKind;
use url::Url;

const MAX_RANKING_LIMIT: usize = 100;
const MIXED_INPUT: &str = "一次只能导出一个集合，不能同时提供其他链接或作品 ID。";

static URLS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)[a-z][a-z0-9+.-]*://[^\s<>"'`(){}，。；！？、（）【】《》]+"#)
        .expect("valid URL token regex")
});

pub(super) enum CollectionSource {
    Pixiv(PixivGallery),
    Pool { site: String, id: u64 },
}

type ParsedSource = Result<CollectionSource, &'static str>;

impl BotHandler {
    pub(super) async fn try_collection_export(
        &self,
        bot: &ThrottledBot,
        msg: &Message,
        chat_id: ChatId,
        args: &str,
        telegraph: bool,
    ) -> ResponseResult<bool> {
        match parse_collection_command(msg, args, &self.booru_registry) {
            None => return Ok(false),
            Some(Ok(source)) => {
                self.send_collection_export(bot, chat_id, source, telegraph)
                    .await?;
            }
            Some(Err(message)) => {
                bot.send_message(chat_id, format!("❌ {message}")).await?;
            }
        }
        Ok(true)
    }
}

fn parse_collection_command(
    msg: &Message,
    args: &str,
    registry: &BooruSiteRegistry,
) -> Option<ParsedSource> {
    if !args.trim().is_empty() {
        return parse_collection_input(args, HashSet::new(), false, true, registry);
    }

    let reply = msg.reply_to_message()?;
    let mut text = reply.text().or_else(|| reply.caption())?.to_owned();
    let mut urls = HashSet::new();
    for entity in reply
        .parse_entities()
        .into_iter()
        .flatten()
        .chain(reply.parse_caption_entities().into_iter().flatten())
    {
        let range = entity.range();
        let url = match entity.kind() {
            MessageEntityKind::TextLink { url } => Some(url.clone()),
            MessageEntityKind::Url => {
                let visible = &text[range.clone()];
                Url::parse(visible)
                    .or_else(|_| Url::parse(&format!("https://{visible}")))
                    .ok()
            }
            _ => None,
        };
        if let Some(url) = url {
            urls.insert(url);
            // A link label is not another URL or bare ID. Keep byte offsets for later entities.
            text.replace_range(range.clone(), &" ".repeat(range.len()));
        }
    }
    let bot_reply = reply.from.as_ref().is_some_and(|user| user.is_bot);
    parse_collection_input(&text, urls, bot_reply, false, registry)
}

fn parse_collection_input(
    text: &str,
    mut urls: HashSet<Url>,
    bot_reply: bool,
    explicit: bool,
    registry: &BooruSiteRegistry,
) -> Option<ParsedSource> {
    let mut other_url = false;
    for token in URLS.find_iter(text) {
        let raw = token.as_str().trim_end_matches(['.', ',', ';', '!']);
        let raw = if raw.contains('[') {
            raw
        } else {
            raw.trim_end_matches(']')
        };
        if let Ok(url) = Url::parse(raw) {
            urls.insert(url);
        } else {
            other_url = true;
        }
    }
    let bot_artwork = bot_reply
        && urls.iter().any(|url| {
            parse_pixiv_links(url.as_str())
                .iter()
                .any(|link| matches!(link, PixivLink::Illust(_)))
        });
    let mut source = None;
    for url in urls {
        match collection_from_url(&url, registry) {
            Some(Ok(CollectionSource::Pixiv(PixivGallery::Author(_)))) if bot_artwork => {}
            Some(_) if source.is_some() => return Some(Err(MIXED_INPUT)),
            Some(parsed) => source = Some(parsed),
            None => other_url = true,
        }
    }
    let source = source?;
    let tokens = || URLS.split(text).flat_map(str::split_whitespace);
    if other_url
        || tokens().any(|token| {
            let token = token.trim_matches(|c: char| {
                c.is_ascii_punctuation() || "，。；！？、（）【】《》".contains(c)
            });
            !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Some(Err(MIXED_INPUT));
    }
    Some(source.and_then(|source| apply_options(source, tokens(), explicit)))
}

fn apply_options<'a>(
    mut source: CollectionSource,
    tokens: impl Iterator<Item = &'a str>,
    explicit: bool,
) -> ParsedSource {
    let mut has_limit = false;
    for token in tokens {
        if let Some(value) = token.strip_prefix("limit=") {
            let CollectionSource::Pixiv(PixivGallery::Ranking { limit, .. }) = &mut source else {
                return Err("limit=N 只适用于 Pixiv 日期榜单。");
            };
            let Some(value) =
                positive_id(value).filter(|&n| !has_limit && n <= MAX_RANKING_LIMIT as u64)
            else {
                return Err("请只提供一次 limit=N，N 必须为 1 到 100 的整数。");
            };
            *limit = value as usize;
            has_limit = true;
        } else if explicit
            && !token
                .chars()
                .all(|c| "<>()[]{}\"'`，。；！？、（）【】《》".contains(c))
        {
            return Err("请只提供一个集合链接；Pixiv 日期榜单可额外指定 limit=N。");
        }
    }
    Ok(source)
}

fn positive_id(id: &str) -> Option<u64> {
    if !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    id.parse::<u64>().ok().filter(|&id| id > 0)
}

fn is_locale(segment: &str) -> bool {
    let alpha_pair = |part: &str| part.len() == 2 && part.bytes().all(|b| b.is_ascii_lowercase());
    segment.split_once('-').map_or_else(
        || alpha_pair(segment),
        |(lang, region)| alpha_pair(lang) && alpha_pair(region),
    )
}

fn collection_from_url(url: &Url, registry: &BooruSiteRegistry) -> Option<ParsedSource> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?;
    let path = url.path().strip_prefix('/')?;
    let path = path.strip_suffix('/').unwrap_or(path);
    let segments: Vec<_> = path.split('/').collect();
    match host {
        "pixiv.net" | "www.pixiv.net" => {
            let segments = match segments.as_slice() {
                [locale, rest @ ..] if is_locale(locale) => rest,
                segments => segments,
            };
            pixiv_collection(url, segments).map(|result| result.map(CollectionSource::Pixiv))
        }
        "pixivision.net" | "www.pixivision.net" => match segments.as_slice() {
            [lang, "a", id] if is_locale(lang) => Some(
                positive_id(id)
                    .map(|id| {
                        CollectionSource::Pixiv(PixivGallery::Pixivision {
                            lang: (*lang).to_owned(),
                            id,
                        })
                    })
                    .ok_or("无效的 pixivision 文章 ID。"),
            ),
            _ => None,
        },
        _ => pool_from_url(url, registry),
    }
}

fn pixiv_collection(url: &Url, segments: &[&str]) -> Option<Result<PixivGallery, &'static str>> {
    let mut query = url
        .query_pairs()
        .filter(|(key, _)| !key.starts_with("utm_"));
    match segments {
        ["collections", id] => Some(
            (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| PixivGallery::Collection((*id).to_owned()))
                .ok_or("无效的 Pixiv 合集 ID。"),
        ),
        ["users", id] | ["users", id, "artworks"] => Some(
            positive_id(id)
                .map(PixivGallery::Author)
                .ok_or("无效的 Pixiv 作者 ID。"),
        ),
        ["users", id, "bookmarks", "artworks", tag @ ..] => {
            let public = match query.next() {
                None => true,
                Some((key, value)) => key == "rest" && value == "show" && query.next().is_none(),
            };
            Some(if !tag.is_empty() || !public {
                Err("仅支持全部公开 Pixiv 收藏，请移除标签或私有筛选后重试。")
            } else {
                positive_id(id)
                    .map(PixivGallery::Bookmarks)
                    .ok_or("无效的 Pixiv 收藏用户 ID。")
            })
        }
        ["ranking.php"] => {
            let mut mode = None;
            let mut date = None;
            let invalid = "请提供 Pixiv 日期榜链接，mode 为 daily、weekly 或 monthly，date 为有效的 YYYYMMDD。";
            for (key, value) in query {
                match (key.as_ref(), value.as_ref()) {
                    ("mode", "daily") if mode.is_none() => mode = Some("day"),
                    ("mode", "weekly") if mode.is_none() => mode = Some("week"),
                    ("mode", "monthly") if mode.is_none() => mode = Some("month"),
                    ("date", value)
                        if date.is_none()
                            && value.len() == 8
                            && value.bytes().all(|byte| byte.is_ascii_digit()) =>
                    {
                        date = NaiveDate::parse_from_str(value, "%Y%m%d").ok();
                        if date.is_none() {
                            return Some(Err(invalid));
                        }
                    }
                    _ => return Some(Err(invalid)),
                }
            }
            Some(mode.zip(date).map_or(Err(invalid), |(mode, date)| {
                Ok(PixivGallery::Ranking {
                    mode: mode.to_owned(),
                    date,
                    limit: MAX_RANKING_LIMIT,
                })
            }))
        }
        _ => None,
    }
}

fn pool_from_url(url: &Url, registry: &BooruSiteRegistry) -> Option<ParsedSource> {
    const UNCONFIGURED: &str = "仅支持已配置的 Danbooru 或 Moebooru pool。";
    let candidates = registry.iter().filter_map(|site| {
        let base = Url::parse(&site.config.base_url).ok()?;
        if base.host_str() != url.host_str() {
            return None;
        }
        let prefix = base.path().trim_end_matches('/');
        let path = url.path().strip_prefix(prefix)?.strip_prefix('/')?;
        let route = parse_pool_path(url, path)?;
        let exact_origin = base.scheme() == url.scheme()
            && base.port_or_known_default() == url.port_or_known_default();
        let engine_matches = route
            .as_ref()
            .is_ok_and(|(engine, _)| *engine == site.config.engine_type);
        Some((
            (exact_origin, prefix.len(), engine_matches),
            &site.config.name,
            route,
        ))
    });
    candidates
        .max_by_key(|(key, _, _)| *key)
        .map(|((exact_origin, _, engine_matches), site, route)| {
            route.and_then(|(_, id)| {
                if exact_origin && engine_matches {
                    Ok(CollectionSource::Pool {
                        site: site.clone(),
                        id,
                    })
                } else {
                    Err(UNCONFIGURED)
                }
            })
        })
        .or_else(|| {
            let path = url.path().strip_prefix('/')?;
            parse_pool_path(url, path).map(|route| route.and(Err(UNCONFIGURED)))
        })
}

fn parse_pool_path(url: &Url, path: &str) -> Option<Result<(BooruEngineType, u64), &'static str>> {
    let path = path.strip_suffix('/').unwrap_or(path);
    let segments: Vec<_> = path.split('/').collect();
    let (engine, id) = match segments.as_slice() {
        ["pools", id] => (BooruEngineType::Danbooru, positive_id(id)),
        ["pool", "show", id] => (BooruEngineType::Moebooru, positive_id(id)),
        ["pool", "show"] => {
            let mut ids = url.query_pairs().filter(|(key, _)| key == "id");
            let id = ids.next().and_then(|(_, id)| positive_id(&id));
            (
                BooruEngineType::Moebooru,
                id.filter(|_| ids.next().is_none()),
            )
        }
        ["pools", ..] | ["pool", "show", ..] => return Some(Err("无效的 Booru pool 链接。")),
        [.., "index.php"]
            if url
                .query_pairs()
                .any(|(key, value)| key == "page" && value == "pool") =>
        {
            return Some(Err(
                "仅支持已配置的 Danbooru 或 Moebooru pool，不支持 Gelbooru pool。",
            ))
        }
        _ => return None,
    };
    Some(id.map(|id| (engine, id)).ok_or("无效的 Booru pool ID。"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn command_reply(mut reply: Value, is_bot: bool) -> Message {
        let chat = json!({"id": 1, "type": "private", "first_name": "Test"});
        reply["message_id"] = json!(1);
        reply["chat"] = chat.clone();
        reply["date"] = json!(0);
        reply["from"] = json!({"id": 2, "is_bot": is_bot, "first_name": "Sender"});
        serde_json::from_value(json!({
            "message_id": 2, "chat": chat, "date": 0, "text": "/download", "reply_to_message": reply,
        }))
        .unwrap()
    }

    #[test]
    fn reply_caption_links_are_used_only_when_arguments_are_empty() {
        let id = "184467440737095516160";
        let msg = command_reply(
            json!({
                "photo": [{"file_id": "file", "file_unique_id": "unique", "width": 1, "height": 1}],
                "caption": "🎨 123",
                "caption_entities": [{
                    "offset": 3, "length": 3, "type": "text_link",
                    "url": format!("https://www.pixiv.net/zh-cn/collections/{id}")
                }],
            }),
            false,
        );
        let registry = BooruSiteRegistry::default();
        assert!(matches!(
            parse_collection_command(&msg, "", &registry),
            Some(Ok(CollectionSource::Pixiv(PixivGallery::Collection(found)))) if found == id
        ));
        assert!(parse_collection_command(&msg, "12345", &registry).is_none());
    }

    #[test]
    fn artwork_reply_ignores_bot_author_attribution_without_ignoring_human_mixed_input() {
        let text = "https://www.pixiv.net/artworks/123\nBy Artist";
        let reply = json!({
            "text": text,
            "entities": [{
                "offset": text.find("Artist").unwrap(), "length": 6,
                "type": "text_link", "url": "https://www.pixiv.net/users/456"
            }],
        });
        let registry = BooruSiteRegistry::default();
        let bot = command_reply(reply.clone(), true);
        let human = command_reply(reply, false);
        assert!(parse_collection_command(&bot, "", &registry).is_none());
        assert!(matches!(
            parse_collection_command(&human, "", &registry),
            Some(Err(MIXED_INPUT))
        ));
    }

    #[test]
    fn bookmark_filters_do_not_expand_into_the_full_collection() {
        let prefix = "https://www.pixiv.net/en/users/123/bookmarks/artworks";
        let registry = BooruSiteRegistry::default();
        let parse =
            |input: &str| parse_collection_input(input, HashSet::new(), false, true, &registry);
        assert!(matches!(
            parse(&format!("{prefix}?utm_source=app&rest=show")),
            Some(Ok(CollectionSource::Pixiv(PixivGallery::Bookmarks(123))))
        ));
        for suffix in ["/cat", "?tag=cat", "?rest=hide"] {
            assert!(matches!(parse(&format!("{prefix}{suffix}")), Some(Err(_))));
        }
    }

    #[test]
    fn dated_ranking_rejects_mixed_targets_and_excess_limit() {
        let ranking = "https://www.pixiv.net/ranking.php?date=20240229&mode=monthly";
        let registry = BooruSiteRegistry::default();
        let parse =
            |input: &str| parse_collection_input(input, HashSet::new(), false, true, &registry);
        assert!(matches!(
            parse(&format!("{ranking} limit=25")),
            Some(Ok(CollectionSource::Pixiv(PixivGallery::Ranking { mode, date, limit: 25 })))
                if mode == "month" && date == NaiveDate::from_ymd_opt(2024, 2, 29).unwrap()
        ));
        for target in ["https://e-hentai.org/g/12345/0123456789/", "`12345`"] {
            assert!(matches!(
                parse(&format!("{ranking} {target}")),
                Some(Err(MIXED_INPUT))
            ));
        }
        assert!(matches!(
            parse(&format!("{ranking} limit=101")),
            Some(Err(_))
        ));
    }

    #[test]
    fn pool_routes_respect_configured_engines_before_the_legacy_post_parser() {
        let sites: Vec<crate::config::BooruSiteConfig> = serde_json::from_value(json!([
            {"name": "fanart", "engine_type": "danbooru", "base_url": "https://booru.example"},
            {"name": "port", "engine_type": "danbooru", "base_url": "https://booru.example:8443"},
            {"name": "prefix", "engine_type": "danbooru", "base_url": "https://booru.example:8443/boards/"},
            {"name": "archive", "engine_type": "moebooru", "base_url": "https://booru.example:8443/archive"},
            {"name": "gel", "engine_type": "gelbooru", "base_url": "https://gel.example"}
        ]))
        .unwrap();
        let registry = BooruSiteRegistry::from_configs(&sites);
        let parse =
            |input: &str| parse_collection_input(input, HashSet::new(), false, true, &registry);
        for (url, expected) in [
            ("https://booru.example:443/pools/91", "fanart"),
            ("https://booru.example:8443/pools/91", "port"),
            ("https://booru.example:8443/boards/pools/91", "prefix"),
            (
                "https://booru.example:8443/archive/pool/show?id=91",
                "archive",
            ),
        ] {
            assert!(
                matches!(
                    parse(url),
                    Some(Ok(CollectionSource::Pool { site, id: 91 })) if site == expected
                ),
                "{url}"
            );
        }
        for url in [
            "https://booru.example:9443/pools/91",
            "https://booru.example:9443/boards/pools/91",
            "http://booru.example:8443/boards/pools/91",
            "https://booru.example:8443/archive/pools/91",
        ] {
            assert!(matches!(parse(url), Some(Err(_))), "{url}");
        }
        assert!(parse("https://booru.example:8443/boards-extra/pools/91").is_none());
        let gel = "https://gel.example/index.php?page=pool&s=show&id=91";
        assert!(!crate::bot::link_handler::parse_booru_post_links(gel, &registry).is_empty());
        assert!(matches!(parse(gel), Some(Err(_))));
    }
}
