mod author;
mod booru;
mod channel;
mod ehentai;
mod helpers;
mod list;
mod ranking;
mod types;

pub use list::{parse_list_callback_data, LIST_CALLBACK_PREFIX};
pub use types::ListPaginationAction;

pub(super) use types::{BatchResult, PAGE_SIZE};

#[cfg(test)]
mod tests {
    use crate::booru::BooruSiteRegistry;
    use crate::bot::notifier::Notifier;
    use crate::bot::BotHandler;
    use crate::cache::FileCacheManager;
    use crate::config::{EhentaiConfig, PixivConfig};
    use crate::db::repo::tests_helpers;
    use crate::db::types::{TagFilter, TaskType};
    use crate::pixiv::client::PixivClient;
    use crate::pixiv::downloader::Downloader;
    use reqwest::Client;
    use std::sync::Arc;
    use teloxide::adaptors::throttle::Limits;
    use teloxide::requests::RequesterExt;
    use teloxide::types::ChatId;
    use teloxide::Bot;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn telegram_inline_code_contents(markdown: &str) -> Vec<String> {
        let mut contents = Vec::new();
        let mut chars = markdown.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch != '`' {
                continue;
            }

            let mut value = String::new();
            while let Some(ch) = chars.next() {
                match ch {
                    '`' => {
                        contents.push(value);
                        break;
                    }
                    '\\' if chars.peek().is_some_and(|ch| ('\u{1}'..='~').contains(ch)) => {
                        value.push(chars.next().unwrap());
                    }
                    _ => value.push(ch),
                }
            }
        }

        contents
    }

    #[tokio::test]
    async fn copied_ids_unsubscribe_exact_chat_task_before_legacy_parsing() {
        let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
        for chat_id in [-100, -200] {
            repo.upsert_chat(
                chat_id,
                "private".to_string(),
                None,
                true,
                Default::default(),
            )
            .await
            .unwrap();
        }

        let eh_id_task = repo
            .get_or_create_task(TaskType::Ehentai, "eh:foo".to_string(), None)
            .await
            .unwrap();
        let eh_query_looks_like_id_task = repo
            .get_or_create_task(TaskType::Ehentai, "eh:eh:foo".to_string(), None)
            .await
            .unwrap();
        let complex_eh_task = repo
            .get_or_create_task(
                TaskType::Ehentai,
                r"ehq:artist:pipe%7Ctick`slash\name".to_string(),
                None,
            )
            .await
            .unwrap();
        repo.upsert_eh_subscription(-100, eh_id_task.id, TagFilter::default(), None)
            .await
            .unwrap();
        repo.upsert_eh_subscription(-200, eh_id_task.id, TagFilter::default(), None)
            .await
            .unwrap();
        repo.upsert_eh_subscription(
            -100,
            eh_query_looks_like_id_task.id,
            TagFilter::default(),
            None,
        )
        .await
        .unwrap();
        repo.upsert_eh_subscription(-100, complex_eh_task.id, TagFilter::default(), None)
            .await
            .unwrap();

        let booru_tag_task = repo
            .get_or_create_task(
                TaskType::BooruTag,
                "yd:landscape scale=day".to_string(),
                None,
            )
            .await
            .unwrap();
        let booru_ranking_task = repo
            .get_or_create_task(
                TaskType::BooruRanking,
                "yd:landscape|r=day".to_string(),
                None,
            )
            .await
            .unwrap();
        let booru_spaced_key_task = repo
            .get_or_create_task(TaskType::BooruTag, "yd:blue sky|f=s".to_string(), None)
            .await
            .unwrap();
        for task_id in [
            booru_tag_task.id,
            booru_ranking_task.id,
            booru_spaced_key_task.id,
        ] {
            repo.upsert_booru_subscription(-100, task_id, TagFilter::default(), None)
                .await
                .unwrap();
        }

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/botfake_token/SendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {
                    "message_id": 42,
                    "date": 1700000000,
                    "chat": {"id": -100, "type": "private"}
                }
            })))
            .mount(&server)
            .await;
        let api_url = url::Url::parse(&server.uri()).unwrap();
        let bot = Bot::new("fake_token")
            .set_api_url(api_url)
            .throttle(Limits::default());
        let downloader = Arc::new(Downloader::new(
            Client::new(),
            FileCacheManager::new("data/test_cache", 7),
        ));
        let handler = BotHandler::new(
            Arc::clone(&repo),
            Arc::new(tokio::sync::RwLock::new(
                PixivClient::new(PixivConfig {
                    refresh_token: "fake_refresh_token".to_string(),
                })
                .unwrap(),
            )),
            Notifier::new(bot.clone(), downloader),
            Vec::new(),
            None,
            false,
            pixiv_client::ImageSize::Large,
            3,
            true,
            "data/test_cache".to_string(),
            "data/test_logs".to_string(),
            Arc::new(BooruSiteRegistry::default()),
            None,
            Arc::new(EhentaiConfig::default()),
            false,
        );

        handler
            .handle_list(bot.clone(), ChatId(-100), None, String::new())
            .await
            .unwrap();

        let list_payload = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
            .find(|payload| {
                payload["text"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("📋"))
            })
            .unwrap();
        assert_eq!(list_payload["parse_mode"], "MarkdownV2");
        let listed_codes = telegram_inline_code_contents(list_payload["text"].as_str().unwrap());
        let copied_eh_id = listed_codes
            .iter()
            .find(|value| value.as_str() == eh_id_task.value.as_str())
            .unwrap()
            .clone();
        let copied_eh_query_id = listed_codes
            .iter()
            .find(|value| value.as_str() == eh_query_looks_like_id_task.value.as_str())
            .unwrap()
            .clone();
        let copied_complex_eh_id = listed_codes
            .iter()
            .find(|value| value.as_str() == complex_eh_task.value.as_str())
            .unwrap()
            .clone();
        let copied_booru_tag_id = listed_codes
            .iter()
            .find(|value| value.as_str() == booru_tag_task.value.as_str())
            .unwrap()
            .clone();
        let copied_booru_spaced_id = listed_codes
            .iter()
            .find(|value| value.as_str() == booru_spaced_key_task.value.as_str())
            .unwrap()
            .clone();

        handler
            .handle_eunsub(bot.clone(), ChatId(-100), None, copied_eh_id.clone())
            .await
            .unwrap();
        assert!(repo
            .get_subscription_by_chat_task(-100, eh_id_task.id)
            .await
            .unwrap()
            .is_none());
        assert!(repo
            .get_subscription_by_chat_task(-200, eh_id_task.id)
            .await
            .unwrap()
            .is_some());

        handler
            .handle_eunsub(bot.clone(), ChatId(-100), None, copied_eh_id)
            .await
            .unwrap();
        let repeated_eh_reply = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .last()
            .and_then(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
            .unwrap();
        assert_eq!(repeated_eh_reply["text"], "❌ 未找到对应的订阅");
        assert!(repo
            .get_subscription_by_chat_task(-100, eh_query_looks_like_id_task.id)
            .await
            .unwrap()
            .is_some());
        handler
            .handle_eunsub(bot.clone(), ChatId(-100), None, copied_eh_query_id)
            .await
            .unwrap();
        assert!(repo
            .get_subscription_by_chat_task(-100, eh_query_looks_like_id_task.id)
            .await
            .unwrap()
            .is_none());
        handler
            .handle_eunsub(bot.clone(), ChatId(-100), None, copied_complex_eh_id)
            .await
            .unwrap();
        assert!(repo
            .get_subscription_by_chat_task(-100, complex_eh_task.id)
            .await
            .unwrap()
            .is_none());

        handler
            .handle_bunsub(bot.clone(), ChatId(-100), None, copied_booru_tag_id)
            .await
            .unwrap();
        assert!(repo
            .get_subscription_by_chat_task(-100, booru_tag_task.id)
            .await
            .unwrap()
            .is_none());
        assert!(repo
            .get_subscription_by_chat_task(-100, booru_ranking_task.id)
            .await
            .unwrap()
            .is_some());

        handler
            .handle_bunsub(bot.clone(), ChatId(-100), None, copied_booru_spaced_id)
            .await
            .unwrap();
        assert!(repo
            .get_subscription_by_chat_task(-100, booru_spaced_key_task.id)
            .await
            .unwrap()
            .is_none());

        handler
            .handle_bunsub(
                bot,
                ChatId(-100),
                None,
                "yd:landscape scale=day".to_string(),
            )
            .await
            .unwrap();
        assert!(repo
            .get_subscription_by_chat_task(-100, booru_ranking_task.id)
            .await
            .unwrap()
            .is_none());
    }
}
