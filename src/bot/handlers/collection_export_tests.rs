use crate::booru::BooruSiteRegistry;
use crate::bot::{
    notifier::{Notifier, ThrottledBot},
    BotHandler,
};
use crate::cache::FileCacheManager;
use crate::config::{BooruSiteConfig, EhentaiConfig, PixivConfig};
use crate::db::repo::{tests_helpers::setup_test_db, Repo};
use crate::db::types::UserRole;
use crate::pixiv::{client::PixivClient, downloader::Downloader};
use crate::scheduler::{CollectionEngine, CollectionEngineConfig, CollectionSource};
use image::ImageEncoder;
use media_publish::{PixiUploader, TelegraphClient, TelegraphPublisher};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde_json::{json, Value};
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use teloxide::adaptors::throttle::Limits;
use teloxide::prelude::*;
use tokio::sync::mpsc;
use tokio::time::timeout;
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockBuilder, MockServer, Request, ResponseTemplate};

const DELIVERY_TIMEOUT: Duration = Duration::from_secs(15);

fn png(rgb: [u8; 3]) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&rgb, 1, 1, image::ExtendedColorType::Rgb8)
        .unwrap();
    bytes
}

fn multipart_parts(request: &Request) -> Vec<(&str, &[u8])> {
    let boundary = request.headers["content-type"]
        .to_str()
        .unwrap()
        .strip_prefix("multipart/form-data; boundary=")
        .unwrap();
    let delimiter = regex::bytes::Regex::new(&regex::escape(&format!("--{boundary}"))).unwrap();
    delimiter
        .split(&request.body)
        .filter_map(|part| {
            let end = part.windows(4).position(|bytes| bytes == b"\r\n\r\n")?;
            let headers = std::str::from_utf8(&part[..end]).unwrap();
            Some((headers, part[end + 4..].strip_suffix(b"\r\n").unwrap()))
        })
        .collect()
}

fn assert_zip<'a>(
    parts: &[(&str, &[u8])],
    chat_id: ChatId,
    expected: impl ExactSizeIterator<Item = (u64, &'a [u8])>,
) {
    let chat = chat_id.0.to_string();
    assert!(parts.iter().any(|(headers, bytes)| {
        headers.contains("name=\"chat_id\"") && *bytes == chat.as_bytes()
    }));
    let files: Vec<_> = parts
        .iter()
        .filter_map(|(headers, bytes)| headers.contains("; filename=").then_some(*bytes))
        .collect();
    let [zip] = files.as_slice() else {
        panic!("expected one file in Telegram SendDocument");
    };
    let mut archive = zip::ZipArchive::new(Cursor::new(*zip)).unwrap();
    assert_eq!(archive.len(), expected.len());
    for (index, (id, expected)) in expected.enumerate() {
        let mut entry = archive.by_index(index).unwrap();
        assert_eq!(
            entry.name().unwrap().split_once('_').unwrap().1,
            format!("{id}.png")
        );
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.as_slice(), expected);
    }
}

async fn mock_pool(server: &MockServer, id: u64, posts: &[(u64, &str)], requests: u64) {
    let base = server.uri();
    let ids: Vec<_> = posts.iter().map(|(id, _)| id).collect();
    Mock::given(method("GET"))
        .and(path(format!("/pools/{id}.json")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": id, "post_count": posts.len(), "post_ids": ids
        })))
        .expect(requests)
        .mount(server)
        .await;
    for batch in posts.chunks(50) {
        let ids = batch
            .iter()
            .map(|(id, _)| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let response: Vec<_> = batch
            .iter()
            .rev()
            .map(|(id, image)| {
                json!({
                    "id": id,
                    "file_url": format!("{base}/pool-images/{id}/{image}.png"),
                    "file_ext": "png"
                })
            })
            .collect();
        Mock::given(method("GET"))
            .and(path("/posts.json"))
            .and(query_param("tags", format!("id:{ids} status:any")))
            .and(query_param("limit", batch.len().to_string()))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .expect(requests)
            .mount(server)
            .await;
    }
}

async fn job_status(repo: &Repo, job_id: i64) -> Option<String> {
    repo.db()
        .query_one(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "SELECT status FROM collection_jobs WHERE id = ?",
            [job_id.into()],
        ))
        .await
        .unwrap()
        .map(|row| row.try_get_by_index(0).unwrap())
}

async fn wait_for_job_status(repo: &Repo, job_id: i64, expected: Option<&str>) {
    timeout(DELIVERY_TIMEOUT, async {
        while job_status(repo, job_id).await.as_deref() != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("collection job {job_id} did not reach {expected:?}"));
}

fn collection_harness(
    repo: Arc<Repo>,
    bot: ThrottledBot,
    base: &str,
    cache: &Path,
) -> (BotHandler, CollectionEngine) {
    let site: BooruSiteConfig = serde_json::from_value(json!({
        "name": "pool-test", "engine_type": "danbooru", "base_url": base
    }))
    .unwrap();
    let pixiv_client = PixivClient::new(PixivConfig {
        refresh_token: "fake".into(),
    })
    .unwrap();
    let downloader = Arc::new(Downloader::new(
        reqwest::Client::new(),
        FileCacheManager::new(cache, 7),
    ));
    let booru_registry = BooruSiteRegistry::from_configs(&[site]);
    let wakeup = Arc::new(tokio::sync::Notify::new());
    let handler = BotHandler::new(crate::bot::BotHandlerConfig {
        repo: repo.clone(),
        pixiv_client: pixiv_client.clone(),
        notifier: Notifier::new(bot.clone(), downloader.clone()),
        default_sensitive_tags: vec![],
        owner_id: None,
        is_public_mode: false,
        image_size: pixiv_client::ImageSize::Large,
        download_original_threshold: 3,
        require_mention_in_group: true,
        cache_dir: String::new(),
        log_dir: String::new(),
        booru_registry: booru_registry.clone(),
        eh_client: None,
        eh_config: Arc::new(EhentaiConfig::default()),
        has_telegraph: true,
        collection_wakeup: wakeup.clone(),
    });
    let engine = CollectionEngine::new(CollectionEngineConfig {
        repo,
        bot,
        pixiv_client,
        downloader,
        booru_registry,
        telegraph: Some(TelegraphPublisher::new(
            Arc::new(TelegraphClient::new_with_urls(
                "fake".into(),
                format!("{base}/unused-upload"),
                base.into(),
            )),
            Arc::new(PixiUploader::new_with_url(format!("{base}/image-host"))),
        )),
        max_retry_count: 1,
        wakeup,
    });
    (handler, engine)
}

fn page_delivery(chat_id: ChatId, page_url: String) -> MockBuilder {
    Mock::given(method("POST"))
        .and(path("/botfake_token/SendMessage"))
        .and(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            body["chat_id"] == chat_id.0
                && body["text"].as_str().and_then(|text| text.lines().last())
                    == Some(page_url.as_str())
        })
}

#[tokio::test]
async fn collection_worker_delivers_ordered_zip_batches_and_retries_telegraph_upload() {
    let server = MockServer::start().await;
    let base = server.uri();
    let red = png([255, 0, 0]);
    let blue = png([0, 0, 255]);
    let green = png([0, 255, 0]);
    let mut posts: Vec<_> = (102u64..=151)
        .map(|id| (id, if id.is_multiple_of(2) { "blue" } else { "red" }))
        .collect();
    posts.rotate_left(17);
    posts.push((101, "green"));
    mock_pool(&server, 9, &posts, 1).await;
    mock_pool(&server, 10, &[(31, "red"), (22, "blue")], 2).await;
    for (image, bytes) in [("red", &red), ("blue", &blue), ("green", &green)] {
        Mock::given(method("GET"))
            .and(path_regex(format!(r"^/pool-images/\d+/{image}\.png$")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
            .mount(&server)
            .await;
    }

    let chat_id = ChatId(-42);
    let user = |id| json!({"id": id, "is_bot": false, "first_name": "Member"});
    let message = json!({
        "message_id": 1, "date": 1700000000, "from": user(42),
        "chat": {"id": chat_id.0, "type": "group", "title": "Collections"},
        "text": "/download"
    });
    let mut sent_message = json!({
        "message_id": 2, "date": 1700000000, "chat": message["chat"],
        "from": {"id": 99, "is_bot": true, "first_name": "Test bot"}, "text": "Collection"
    });
    let sent =
        ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": sent_message}));
    let (documents_tx, mut documents_rx) = mpsc::unbounded_channel();
    let document_sent = sent.clone();
    Mock::given(method("POST"))
        .and(path("/botfake_token/SendDocument"))
        .respond_with(move |request: &Request| {
            documents_tx.send(request.clone()).unwrap();
            document_sent.clone()
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/botfake_token/SendMessage"))
        .respond_with(sent.clone())
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/botfake_token/AnswerCallbackQuery"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": true})))
        .expect(4)
        .mount(&server)
        .await;

    let image_urls = [
        format!("{base}/published/red.png"),
        format!("{base}/published/blue.png"),
    ];
    let page_url = format!("{base}/pool-home");
    let page_notice = page_delivery(chat_id, page_url.clone())
        .respond_with(sent)
        .with_priority(1)
        .expect(1)
        .mount_as_scoped(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/image-host"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    let upload_response = ResponseTemplate::new(200).set_body_json(json!({
        "success": true,
        "images": image_urls.each_ref().map(|url| json!({"direct_url": url}))
    }));
    let (retry_uploads_tx, mut retry_uploads_rx) = mpsc::unbounded_channel();
    Mock::given(method("POST"))
        .and(path("/image-host"))
        .respond_with(move |_: &Request| {
            retry_uploads_tx
                .send(chrono::Utc::now().timestamp())
                .unwrap();
            upload_response.clone()
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/createPage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "result": {"url": page_url}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let bot = Bot::new("fake_token")
        .set_api_url(base.parse().unwrap())
        .throttle(Limits::default());
    let cache = tempfile::tempdir().unwrap();
    let repo = Arc::new(setup_test_db().await.unwrap());
    repo.upsert_chat(chat_id.0, "group".into(), None, true, Default::default())
        .await
        .unwrap();
    for id in [42, 43] {
        repo.upsert_user(id, None, UserRole::User).await.unwrap();
    }
    let (handler, engine) = collection_harness(repo.clone(), bot.clone(), &base, cache.path());
    let download = serde_json::from_value(message.clone()).unwrap();
    handler
        .handle_download(bot.clone(), download, chat_id, format!("{base}/pools/9"))
        .await
        .unwrap();
    let queued = server.received_requests().await.unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].url.path(), "/botfake_token/SendMessage");

    let worker = tokio::spawn(async move { engine.run().await });
    let first = timeout(DELIVERY_TIMEOUT, documents_rx.recv())
        .await
        .expect("worker did not send the first ZIP")
        .unwrap();
    let parts = multipart_parts(&first);
    assert_zip(
        &parts,
        chat_id,
        posts[..50].iter().map(|(id, image)| {
            (
                *id,
                if *image == "red" {
                    red.as_slice()
                } else {
                    blue.as_slice()
                },
            )
        }),
    );
    let (_, keyboard) = parts
        .iter()
        .find(|(headers, _)| headers.contains("name=\"reply_markup\""))
        .expect("first batch must offer continuation");
    let markup: Value = serde_json::from_slice(keyboard).unwrap();
    let data = markup["inline_keyboard"][0][0]["callback_data"]
        .as_str()
        .unwrap()
        .to_owned();
    let segments: Vec<_> = data.split(':').collect();
    let ["col", job_id, generation] = segments.as_slice() else {
        panic!("unexpected continuation data: {data}");
    };
    let job_id: i64 = job_id.parse().unwrap();
    assert!(generation.parse::<i64>().is_ok());
    // Telegram can see the keyboard before the worker commits the cursor.
    wait_for_job_status(&repo, job_id, Some("waiting")).await;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|req| req.url.path() == "/posts.json")
            .count(),
        1
    );
    assert!(!requests
        .iter()
        .any(|req| req.url.path() == "/pool-images/101/green.png"));

    sent_message["reply_markup"] = markup;
    let click = |id, actor| {
        serde_json::from_value(json!({
            "id": id, "from": user(actor), "message": sent_message,
            "chat_instance": "group", "data": data
        }))
        .unwrap()
    };
    handler
        .handle_collection_more(&bot, &click("other-member", 43))
        .await
        .unwrap();
    assert_eq!(job_status(&repo, job_id).await.as_deref(), Some("waiting"));
    repo.set_chat_enabled(chat_id.0, false).await.unwrap();
    handler
        .handle_collection_more(&bot, &click("chat-disabled", 42))
        .await
        .unwrap();
    assert_eq!(job_status(&repo, job_id).await.as_deref(), Some("waiting"));
    repo.set_chat_enabled(chat_id.0, true).await.unwrap();
    for id in ["author", "duplicate"] {
        handler
            .handle_collection_more(&bot, &click(id, 42))
            .await
            .unwrap();
    }

    // The next queued job shows the worker has drained any work queued by the clicks.
    let mut message = message;
    message["text"] = json!("/telegraph");
    handler
        .handle_telegraph(
            bot.clone(),
            serde_json::from_value(message).unwrap(),
            chat_id,
            format!("{base}/pools/10"),
        )
        .await
        .unwrap();
    let second = timeout(DELIVERY_TIMEOUT, documents_rx.recv())
        .await
        .expect("worker did not send the continuation ZIP")
        .unwrap();
    let parts = multipart_parts(&second);
    assert_zip(&parts, chat_id, std::iter::once((101, green.as_slice())));
    assert!(!parts
        .iter()
        .any(|(headers, _)| headers.contains("name=\"reply_markup\"")));
    let (telegraph_job, scheduled_retry): (i64, i64) = timeout(DELIVERY_TIMEOUT, async {
        loop {
            let retry = repo
                .db()
                .query_one(Statement::from_string(
                    DbBackend::Sqlite,
                    "SELECT id, next_retry_at FROM collection_jobs
                     WHERE telegraph = 1 AND status = 'queued' AND next_retry_at IS NOT NULL",
                ))
                .await
                .unwrap();
            if let Some(row) = retry {
                break (
                    row.try_get_by_index(0).unwrap(),
                    row.try_get_by_index(1).unwrap(),
                );
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker did not schedule a retry after the image host error");
    assert!(scheduled_retry > chrono::Utc::now().timestamp());
    assert!(job_status(&repo, job_id).await.is_none());
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());

    let (_handler, engine) = collection_harness(repo.clone(), bot.clone(), &base, cache.path());
    let retry_at = chrono::Utc::now().timestamp() + 2;
    repo.db()
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE collection_jobs SET next_retry_at = ? WHERE id = ?",
            [retry_at.into(), telegraph_job.into()],
        ))
        .await
        .unwrap();
    engine.recover().await.unwrap();
    let worker = tokio::spawn(async move { engine.run().await });
    let retried_at = timeout(DELIVERY_TIMEOUT, retry_uploads_rx.recv())
        .await
        .expect("restarted worker did not wake up for the retry")
        .unwrap();
    assert!(retried_at >= retry_at, "worker retried before the deadline");
    timeout(DELIVERY_TIMEOUT, page_notice.wait_until_satisfied())
        .await
        .expect("worker did not send the Telegraph homepage to the original chat");
    wait_for_job_status(&repo, telegraph_job, None).await;
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert!(
        documents_rx.try_recv().is_err(),
        "continuation sent a duplicate ZIP"
    );

    let requests = server.received_requests().await.unwrap();
    for upload in requests
        .iter()
        .filter(|request| request.url.path() == "/image-host")
    {
        let uploaded_files: Vec<_> = multipart_parts(upload)
            .into_iter()
            .filter_map(|(headers, bytes)| headers.contains("; filename=").then_some(bytes))
            .collect();
        assert_eq!(uploaded_files, [red.as_slice(), blue.as_slice()]);
    }
    let page = requests
        .iter()
        .find(|request| request.url.path() == "/createPage")
        .unwrap();
    let (_, content) = url::form_urlencoded::parse(&page.body)
        .find(|(key, _)| key == "content")
        .expect("Telegraph page must include content");
    let nodes: Value = serde_json::from_str(&content).unwrap();
    assert_eq!(
        nodes,
        json!(image_urls
            .each_ref()
            .map(|url| json!({"tag": "img", "attrs": {"src": url}})))
    );
}

#[tokio::test]
async fn collection_worker_does_not_retry_publication_errors() {
    for fail_create_page in [true, false] {
        let server = MockServer::start().await;
        let base = server.uri();
        let chat_id = ChatId(-42);
        let page_url = format!("{base}/pool-home");
        mock_pool(&server, 10, &[(31, "red")], 1).await;
        Mock::given(method("GET"))
            .and(path("/pool-images/31/red.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(png([255, 0, 0])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/image-host"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true, "direct_url": format!("{base}/published/red.png")
            })))
            .expect(1)
            .mount(&server)
            .await;
        let page_response = if fail_create_page {
            ResponseTemplate::new(500)
        } else {
            ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": {"url": page_url}
            }))
        };
        Mock::given(method("POST"))
            .and(path("/createPage"))
            .respond_with(page_response)
            .expect(1)
            .mount(&server)
            .await;
        page_delivery(chat_id, page_url)
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({
                "ok": false, "error_code": 500, "description": "Internal Server Error"
            })))
            .with_priority(1)
            .expect(u64::from(!fail_create_page))
            .mount(&server)
            .await;
        let failure_notice = Mock::given(method("POST"))
            .and(path("/botfake_token/SendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": {
                    "message_id": 2, "date": 1700000000, "text": "Collection failed",
                    "chat": {"id": chat_id.0, "type": "group", "title": "Collections"}
                }
            })))
            .expect(1)
            .mount_as_scoped(&server)
            .await;

        let repo = Arc::new(setup_test_db().await.unwrap());
        repo.upsert_chat(chat_id.0, "group".into(), None, true, Default::default())
            .await
            .unwrap();
        let source = serde_json::to_string(&CollectionSource::Pool {
            site: "pool-test".into(),
            id: 10,
        })
        .unwrap();
        let job_id = repo
            .enqueue_collection_job(chat_id.0, 42, source, true)
            .await
            .unwrap();
        let bot = Bot::new("fake_token")
            .set_api_url(base.parse().unwrap())
            .throttle(Limits::default());
        let cache = tempfile::tempdir().unwrap();
        let (_handler, engine) = collection_harness(repo.clone(), bot.clone(), &base, cache.path());
        let worker = tokio::spawn(async move { engine.run().await });
        timeout(DELIVERY_TIMEOUT, failure_notice.wait_until_satisfied())
            .await
            .unwrap_or_else(|_| {
                panic!("worker did not report terminal failure (createPage failed: {fail_create_page})")
            });
        assert!(job_status(&repo, job_id).await.is_none());
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
    }
}
