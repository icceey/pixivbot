use crate::booru::BooruSiteRegistry;
use crate::bot::{notifier::Notifier, BotHandler, TelegraphServices};
use crate::cache::FileCacheManager;
use crate::config::{BooruSiteConfig, PixivConfig};
use crate::db::repo::tests_helpers::setup_test_db;
use crate::pixiv::{client::PixivClient, downloader::Downloader};
use eh_client::{PixiUploader, TelegraphClient};
use image::ImageEncoder;
use serde_json::{json, Value};
use std::io::{Cursor, Read};
use std::sync::Arc;
use teloxide::adaptors::throttle::Limits;
use teloxide::prelude::*;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn png(rgb: [u8; 3]) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&rgb, 1, 1, image::ExtendedColorType::Rgb8)
        .unwrap();
    bytes
}

fn multipart_files(request: &Request) -> Vec<&[u8]> {
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
            headers
                .contains("; filename=")
                .then(|| part[end + 4..].strip_suffix(b"\r\n").unwrap())
        })
        .collect()
}

#[tokio::test]
async fn collection_commands_deliver_pool_images_in_order() {
    let server = MockServer::start().await;
    let base = server.uri();
    let red = png([255, 0, 0]);
    let blue = png([0, 0, 255]);

    Mock::given(method("GET"))
        .and(path("/pools/9.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 9, "post_count": 2, "post_ids": [31, 22]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/posts.json"))
        .and(query_param("tags", "pool:9 status:any"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 22, "file_url": format!("{base}/blue.png"), "file_ext": "png"},
            {"id": 31, "file_url": format!("{base}/red.png"), "file_ext": "png"}
        ])))
        .mount(&server)
        .await;
    for (route, bytes) in [("/red.png", &red), ("/blue.png", &blue)] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
            .mount(&server)
            .await;
    }

    let chat_id = ChatId(42);
    let message = json!({
        "message_id": 1, "date": 1700000000,
        "chat": {"id": chat_id.0, "type": "private"}, "text": "/download"
    });
    for endpoint in ["/botfake_token/SendDocument", "/botfake_token/SendMessage"] {
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": message})),
            )
            .mount(&server)
            .await;
    }

    let upload_url = format!("{base}/image-host");
    let image_urls = [
        format!("{base}/published/red.png"),
        format!("{base}/published/blue.png"),
    ];
    let page_url = format!("{base}/pool-home");
    Mock::given(method("POST"))
        .and(path("/image-host"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "images": image_urls.each_ref().map(|url| json!({"direct_url": url}))
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/createPage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "result": {"url": page_url}
        })))
        .mount(&server)
        .await;

    let bot = Bot::new("fake_token")
        .set_api_url(base.parse().unwrap())
        .throttle(Limits::default());
    let cache = tempfile::tempdir().unwrap();
    let site: BooruSiteConfig = serde_json::from_value(json!({
        "name": "pool-test", "engine_type": "danbooru", "base_url": base
    }))
    .unwrap();
    let handler = BotHandler::new(
        Arc::new(setup_test_db().await.unwrap()),
        Arc::new(tokio::sync::RwLock::new(
            PixivClient::new(PixivConfig {
                refresh_token: "fake".into(),
            })
            .unwrap(),
        )),
        Notifier::new(
            bot.clone(),
            Arc::new(Downloader::new(
                reqwest::Client::new(),
                FileCacheManager::new(cache.path(), 7),
            )),
        ),
        vec![],
        None,
        false,
        pixiv_client::ImageSize::Large,
        3,
        true,
        String::new(),
        String::new(),
        BooruSiteRegistry::from_configs(&[site]),
        None,
        Arc::default(),
        Some(TelegraphServices {
            client: Arc::new(TelegraphClient::new_with_urls(
                "fake".into(),
                format!("{base}/unused-upload"),
                base.clone(),
            )),
            image_uploader: Arc::new(PixiUploader::new_with_url(upload_url)),
        }),
    );
    let message: Message = serde_json::from_value(message).unwrap();
    let pool = format!("{base}/pools/9");
    handler
        .handle_download(bot.clone(), message.clone(), chat_id, pool.clone())
        .await
        .unwrap();
    handler
        .handle_telegraph(bot, message, chat_id, pool)
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    let request = |route| {
        requests
            .iter()
            .find(|request| request.url.path() == route)
            .unwrap_or_else(|| panic!("no request to {route}"))
    };
    let files = multipart_files(request("/botfake_token/SendDocument"));
    let [zip] = files.as_slice() else {
        panic!("expected one file in Telegram SendDocument");
    };
    let mut archive = zip::ZipArchive::new(Cursor::new(*zip)).unwrap();
    assert_eq!(archive.len(), 2);
    for (index, (name, expected)) in [("1_31.png", &red), ("2_22.png", &blue)]
        .into_iter()
        .enumerate()
    {
        let mut entry = archive.by_index(index).unwrap();
        assert_eq!(entry.name().unwrap(), name);
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        assert_eq!(&bytes, expected);
    }

    assert_eq!(
        multipart_files(request("/image-host")),
        [red.as_slice(), blue.as_slice()]
    );
    let (_, content) = url::form_urlencoded::parse(&request("/createPage").body)
        .find(|(key, _)| key == "content")
        .expect("Telegraph page must include content");
    let nodes: Value = serde_json::from_str(&content).unwrap();
    assert_eq!(
        nodes,
        json!(image_urls
            .each_ref()
            .map(|url| json!({"tag": "img", "attrs": {"src": url}})))
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/botfake_token/SendMessage")
            .any(|request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                body["chat_id"] == chat_id.0
                    && body["text"].as_str().and_then(|text| text.lines().last())
                        == Some(page_url.as_str())
            }),
        "Telegram chat did not receive the Telegraph homepage URL"
    );
}
