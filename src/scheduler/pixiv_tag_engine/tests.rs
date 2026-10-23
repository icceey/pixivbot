use super::*;
use crate::cache::FileCacheManager;
use crate::config::PixivConfig;
use crate::db::types::{AuthorState, PendingIllust, SubscriptionState, TagFilter};
use crate::pixiv::downloader::Downloader;
use migration::MigratorTrait;
use sea_orm::{ConnectionTrait, Database};
use std::future::Future;
use teloxide::adaptors::throttle::Limits;
use teloxide::requests::RequesterExt;
use teloxide::Bot;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn illustration(id: u64, host: &str, pages: usize) -> Illust {
    let image = |page| {
        serde_json::json!({
            "square_medium": format!("{host}/image/{page}.jpg"),
            "medium": format!("{host}/image/{page}.jpg"),
            "large": format!("{host}/image/{page}.jpg")
        })
    };
    serde_json::from_value(serde_json::json!({
        "id": id, "title": "fixture", "type": "illust", "image_urls": image(0),
        "caption": "", "restrict": 0, "user": {"id": 1, "name": "artist", "account": "fixture"},
        "tags": [{"name": "test"}], "create_date": Utc::now().to_rfc3339(),
        "page_count": pages, "width": 100, "height": 100, "sanity_level": 2, "x_restrict": 0,
        "meta_single_page": {}, "meta_pages": (0..pages).map(|i| serde_json::json!({"image_urls": image(i)})).collect::<Vec<_>>(),
        "total_view": 2000, "total_bookmarks": 300, "is_bookmarked": false, "visible": true
    })).unwrap()
}

async fn open_repo(url: &str) -> Repo {
    let db = Database::connect(url).await.unwrap();
    db.execute_unprepared("PRAGMA foreign_keys=ON")
        .await
        .unwrap();
    migration::Migrator::up(&db, None).await.unwrap();
    Repo::new(db)
}

async fn subscribe(repo: &Repo) -> (tasks::Model, subscriptions::Model, chats::Model) {
    let chat = repo
        .upsert_chat(42, "private".to_string(), None, true, Default::default())
        .await
        .unwrap();
    let task = repo
        .get_or_create_task(TaskType::PixivTag, "test".to_string(), None)
        .await
        .unwrap();
    repo.upsert_pixiv_tag_subscription(
        42,
        task.id,
        TagFilter::default(),
        &TagSettings { min_bookmarks: 300 },
    )
    .await
    .unwrap();
    let sub = repo
        .get_subscription_by_chat_task(42, task.id)
        .await
        .unwrap()
        .unwrap();
    (task, sub, chat)
}

#[tokio::test]
async fn page_checkpoint_and_candidates_commit_together_and_resume_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}/state.db?mode=rwc",
        dir.path().to_string_lossy().replace('\\', "/")
    );
    let repo = open_repo(&url).await;
    let (task, _, _) = subscribe(&repo).await;
    let today = NaiveDate::from_ymd_opt(2026, 10, 24).unwrap();
    let oldest = today - Days::new(7);
    let boundary = (oldest + Days::new(1))
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(SEARCH_TIMEZONE)
        .unwrap()
        .timestamp();
    let scan = repo.pixiv_tag_scan(task.id, oldest, 7).await.unwrap();
    let illusts = vec![
        (illustration(1, "https://example.test", 1), boundary - 1),
        (illustration(2, "https://example.test", 1), boundary),
    ];
    // Switching the query's start date must rebase its offset. Carrying 30 into
    // the new day would skip its first 29 not-yet-observed works.
    let checkpoint = advance_scan(&scan, &illusts, Some(30), today).unwrap();
    assert_eq!(
        (checkpoint.cursor_date, checkpoint.cursor_offset),
        (oldest + Days::new(1), 1)
    );
    repo.db()
        .execute_unprepared(
            "CREATE TRIGGER fail_page BEFORE INSERT ON pixiv_tag_candidates
        WHEN NEW.illust_id=2 BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
        )
        .await
        .unwrap();
    assert!(repo
        .save_pixiv_tag_page(task.id, &illusts, &checkpoint)
        .await
        .is_err());
    assert!(repo.pixiv_tag_candidates(task.id).await.unwrap().is_empty());
    assert_eq!(
        repo.pixiv_tag_scan(task.id, oldest, 7)
            .await
            .unwrap()
            .cursor_offset,
        0
    );
    repo.db()
        .execute_unprepared("DROP TRIGGER fail_page")
        .await
        .unwrap();
    repo.save_pixiv_tag_page(task.id, &illusts, &checkpoint)
        .await
        .unwrap();
    repo.db().clone().close().await.unwrap();
    let repo = open_repo(&url).await;
    let resumed = repo.pixiv_tag_scan(task.id, oldest, 7).await.unwrap();
    assert_eq!(
        (
            resumed.cursor_date,
            resumed.cursor_offset,
            resumed.pages_scanned
        ),
        (checkpoint.cursor_date, 1, 1)
    );
    assert_eq!(repo.pixiv_tag_candidates(task.id).await.unwrap().len(), 2);
    // A rolling window may discard old backlog, but not reuse its offset in the
    // new start day or forget the page estimate that controls the next interval.
    let clipped = repo
        .pixiv_tag_scan(task.id, oldest + Days::new(2), 7)
        .await
        .unwrap();
    assert_eq!(
        (
            clipped.cursor_date,
            clipped.cursor_offset,
            clipped.pages_scanned,
            clipped.total_pages
        ),
        (oldest + Days::new(2), 0, 1, 1)
    );
    let resized = repo
        .pixiv_tag_scan(task.id, today - Days::new(14), 14)
        .await
        .unwrap();
    assert_eq!(
        (resized.cursor_date, resized.cursor_offset),
        (today - Days::new(14), 0)
    );
    assert_eq!(repo.pixiv_tag_candidates(task.id).await.unwrap().len(), 2);
    let finished = advance_scan(&resized, &illusts, None, today).unwrap();
    repo.save_pixiv_tag_page(task.id, &illusts, &finished)
        .await
        .unwrap();
    repo.db().clone().close().await.unwrap();
    let repo = open_repo(&url).await;
    let restarted = repo
        .pixiv_tag_scan(task.id, today - Days::new(14), 14)
        .await
        .unwrap();
    assert_eq!(
        (
            restarted.cursor_date,
            restarted.cursor_offset,
            restarted.pages_scanned,
            restarted.total_pages,
            restarted.complete
        ),
        (today - Days::new(14), 0, 0, finished.pages_scanned, false)
    );
    repo.db().clone().close().await.unwrap();
}

#[tokio::test]
async fn failed_download_and_telegram_retry_resume_only_unsent_pages_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}/delivery.db?mode=rwc",
        dir.path().to_string_lossy().replace('\\', "/")
    );
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex("/image/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"mock image".to_vec()))
        .mount(&server)
        .await;
    // A one-request fault avoids scoped-guard indices being invalidated by
    // priority sorting in wiremock 0.6.5.
    Mock::given(method("GET"))
        .and(path("/image/4.jpg"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(1)
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/botfake_token/SendChatAction"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok": true, "result": true})),
        )
        .mount(&server)
        .await;
    let message = |id| serde_json::json!({"message_id": id, "date": 1700000000, "chat": {"id": 42, "type": "private"}});
    Mock::given(path("/botfake_token/SendMediaGroup"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"ok": true, "result": (100..110).map(message).collect::<Vec<_>>()}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/botfake_token/SendPhoto"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok": true, "result": message(110)})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let bot = Bot::new("fake_token")
        .set_api_url(url::Url::parse(&server.uri()).unwrap())
        .throttle(Limits::default());
    let notifier = Notifier::new(
        bot,
        Arc::new(Downloader::new(
            reqwest::Client::new(),
            FileCacheManager::new(dir.path().join("cache"), 7),
        )),
    );
    let pixiv = Arc::new(RwLock::new(
        PixivClient::new(PixivConfig {
            refresh_token: "unused".to_string(),
        })
        .unwrap(),
    ));
    let repo = Arc::new(open_repo(&url).await);
    let (task, sub, chat) = subscribe(&repo).await;
    let illust = illustration(10, &server.uri(), 11);
    let engine = PixivTagEngine::new(
        repo.clone(),
        pixiv.clone(),
        notifier.clone(),
        &serde_json::from_value(serde_json::json!({})).unwrap(),
        ImageSize::Large,
    );
    let now = Utc::now().timestamp();
    let scan = repo
        .pixiv_tag_scan(
            task.id,
            Utc::now().with_timezone(&SEARCH_TIMEZONE).date_naive() - Days::new(7),
            7,
        )
        .await
        .unwrap();
    repo.save_pixiv_tag_page(task.id, &[(illust.clone(), now)], &scan)
        .await
        .unwrap();
    engine
        .enqueue_eligible(
            &[(sub.clone(), TagSettings { min_bookmarks: 300 })],
            &illust,
            now,
            now,
        )
        .await
        .unwrap();
    let delivery = repo
        .pixiv_tag_deliveries_due(sub.id, now)
        .await
        .unwrap()
        .pop()
        .unwrap();
    engine.send_delivery(&sub, &chat, delivery).await.unwrap();
    // A missing middle image must not compress the success indices and mark a
    // different page as delivered. No Telegram media request can occur yet.
    let requests = server.received_requests().await.unwrap();
    assert!(!requests
        .iter()
        .any(|r| r.url.path().ends_with("SendMediaGroup")));
    assert!(repo
        .pixiv_tag_deliveries_due(sub.id, now)
        .await
        .unwrap()
        .is_empty());
    let retry = repo
        .pixiv_tag_deliveries_due(sub.id, now + 3600)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(repo
        .pixiv_push_progress(chat.id, illust.id)
        .await
        .unwrap()
        .sent_pages
        .is_empty());
    Mock::given(path("/botfake_token/SendPhoto"))
        .respond_with(ResponseTemplate::new(400).set_body_json(
            serde_json::json!({"ok": false, "error_code": 400, "description": "injected failure"}),
        ))
        .with_priority(1)
        .up_to_n_times(2)
        .expect(2)
        .mount(&server)
        .await;
    engine.send_delivery(&sub, &chat, retry).await.unwrap();
    drop(engine);
    repo.db().clone().close().await.unwrap();
    drop(repo);

    let repo = Arc::new(open_repo(&url).await);
    // Config edits and candidate expiry must not erase an already queued work.
    repo.upsert_pixiv_tag_subscription(
        42,
        task.id,
        TagFilter::default(),
        &TagSettings {
            min_bookmarks: 10000,
        },
    )
    .await
    .unwrap();
    repo.prune_pixiv_tag_candidates(task.id, now + 86400)
        .await
        .unwrap();
    let engine = PixivTagEngine::new(
        repo.clone(),
        pixiv.clone(),
        notifier.clone(),
        &serde_json::from_value(serde_json::json!({"pixiv_tag_window_days": 1})).unwrap(),
        ImageSize::Large,
    );
    let resumed = repo
        .pixiv_tag_deliveries_due(sub.id, now + 86400)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        repo.pixiv_push_progress(chat.id, illust.id)
            .await
            .unwrap()
            .sent_pages,
        (0..10).collect::<Vec<_>>()
    );
    engine.send_delivery(&sub, &chat, resumed).await.unwrap();
    let retry = repo
        .pixiv_tag_deliveries_due(sub.id, now + 86400)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(repo
        .get_message_with_subscription(42, 100)
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        repo.pixiv_push_progress(chat.id, illust.id)
            .await
            .unwrap()
            .sent_pages,
        (0..10).collect::<Vec<_>>()
    );
    // An author subscription has no local page cursor for this work. It must
    // still resume the tag's ten acknowledged pages after the restart.
    let author_task = repo
        .get_or_create_task(TaskType::Author, "1".to_string(), None)
        .await
        .unwrap();
    let author_sub = repo
        .upsert_subscription(chat.id, author_task.id, TagFilter::default())
        .await
        .unwrap();
    assert!(matches!(
        process_illust_push(
            &repo,
            &notifier,
            &pixiv,
            &chat,
            &illust,
            author_sub.id,
            ImageSize::Large,
        )
        .await
        .unwrap(),
        PushResult::Success { .. }
    ));
    assert!(repo
        .get_message_with_subscription(42, 110)
        .await
        .unwrap()
        .is_some());
    // Replaying a stale tag queue after the author finishes must not send again.
    repo.db()
        .execute_unprepared(
            "CREATE TRIGGER fail_complete BEFORE UPDATE ON pixiv_tag_deliveries
         WHEN NEW.done=1 BEGIN SELECT RAISE(ABORT, 'injected queue write failure'); END;",
        )
        .await
        .unwrap();
    assert!(engine.send_delivery(&sub, &chat, retry).await.is_err());
    repo.db()
        .execute_unprepared("DROP TRIGGER fail_complete")
        .await
        .unwrap();
    let retry = repo
        .pixiv_tag_deliveries_due(sub.id, now + 86400)
        .await
        .unwrap()
        .pop()
        .unwrap();
    engine.send_delivery(&sub, &chat, retry).await.unwrap();
    drop(engine);
    repo.db().clone().close().await.unwrap();
    drop(repo);
    let repo = open_repo(&url).await;
    // Replaying a discovered work does not replace the completed dedup row.
    repo.enqueue_pixiv_tag_delivery(sub.id, &illust, now + 86400)
        .await
        .unwrap();
    assert!(repo
        .pixiv_tag_deliveries_due(sub.id, now + 86400)
        .await
        .unwrap()
        .is_empty());
    assert!(repo
        .get_message_with_subscription(42, 100)
        .await
        .unwrap()
        .is_some());
    let requests = server.received_requests().await.unwrap();
    let photo = requests
        .iter()
        .rev()
        .find(|r| r.url.path().ends_with("SendPhoto"))
        .unwrap();
    assert!(String::from_utf8_lossy(&photo.body).contains("continued 2/2"));
    server.verify().await;
    repo.db().clone().close().await.unwrap();
}

#[tokio::test]
async fn existing_author_history_preserves_partial_work_and_excludes_rankings() {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    db.execute_unprepared("PRAGMA foreign_keys=ON")
        .await
        .unwrap();
    // Enable shared history over existing author subscriptions and pending work.
    let released_migrations = u32::try_from(migration::Migrator::migrations().len() - 1).unwrap();
    migration::Migrator::up(&db, Some(released_migrations))
        .await
        .unwrap();
    let repo = Repo::new(db);
    repo.upsert_chat(42, "private".to_string(), None, true, Default::default())
        .await
        .unwrap();
    let author_task = repo
        .get_or_create_task(TaskType::Author, "1".to_string(), None)
        .await
        .unwrap();
    let author_sub = repo
        .upsert_subscription(42, author_task.id, TagFilter::default())
        .await
        .unwrap();
    repo.update_subscription_latest_data(
        author_sub.id,
        Some(SubscriptionState::Author(AuthorState {
            latest_illust_id: 9,
            pending_illust: Some(PendingIllust {
                illust_id: 10,
                sent_pages: vec![0],
                total_pages: 2,
                retry_count: 1,
            }),
        })),
    )
    .await
    .unwrap();
    repo.save_message(42, 200, author_sub.id, Some(10))
        .await
        .unwrap();
    repo.save_message(42, 201, author_sub.id, Some(9))
        .await
        .unwrap();
    let ranking_task = repo
        .get_or_create_task(TaskType::Ranking, "day".to_string(), None)
        .await
        .unwrap();
    let ranking_sub = repo
        .upsert_subscription(42, ranking_task.id, TagFilter::default())
        .await
        .unwrap();
    repo.save_message(42, 202, ranking_sub.id, Some(20))
        .await
        .unwrap();

    migration::Migrator::up(repo.db(), None).await.unwrap();
    let pending = repo.pixiv_push_progress(42, 10).await.unwrap();
    assert!(!pending.done);
    assert_eq!(pending.sent_pages, vec![0]);
    assert!(repo.pixiv_push_progress(42, 9).await.unwrap().done);
    assert!(!repo.pixiv_push_progress(42, 20).await.unwrap().done);
}

#[tokio::test]
async fn interrupted_batch_resumes_without_waiting_for_search_or_replaying_acknowledged_pages() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}/interrupted.db?mode=rwc",
        dir.path().to_string_lossy().replace('\\', "/")
    );
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex("/image/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"mock image".to_vec()))
        .mount(&server)
        .await;
    Mock::given(path("/botfake_token/SendChatAction"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok": true, "result": true})),
        )
        .mount(&server)
        .await;
    let message = |id| serde_json::json!({"message_id": id, "date": 1700000000, "chat": {"id": 42, "type": "private"}});
    Mock::given(path("/botfake_token/SendMediaGroup"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true, "result": (100..110).map(message).collect::<Vec<_>>()
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/botfake_token/SendPhoto"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok": true, "result": message(110)})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let second_batch_arrived = Arc::new(tokio::sync::Notify::new());
    let arrived = second_batch_arrived.clone();
    Mock::given(path("/botfake_token/SendPhoto"))
        .respond_with(move |_: &wiremock::Request| {
            arrived.notify_one();
            ResponseTemplate::new(400)
                .set_body_json(serde_json::json!({"ok": false, "error_code": 400, "description": "interrupted batch"}))
                .set_delay(std::time::Duration::from_secs(3600))
        }).with_priority(1).up_to_n_times(1).expect(1).mount(&server).await;
    let notifier = Notifier::new(
        Bot::new("fake_token")
            .set_api_url(url::Url::parse(&server.uri()).unwrap())
            .throttle(Limits::default()),
        Arc::new(Downloader::new(
            reqwest::Client::new(),
            FileCacheManager::new(dir.path().join("cache"), 7),
        )),
    );
    let pixiv = Arc::new(RwLock::new(
        PixivClient::new(PixivConfig {
            refresh_token: "unused".to_string(),
        })
        .unwrap(),
    ));
    let config: SchedulerConfig = serde_json::from_value(serde_json::json!({})).unwrap();
    let repo = Arc::new(open_repo(&url).await);
    let (task, sub, chat) = subscribe(&repo).await;
    let next_search = Local::now() + chrono::Duration::hours(12);
    repo.update_task_after_poll(task.id, next_search)
        .await
        .unwrap();
    let engine = PixivTagEngine::new(
        repo.clone(),
        pixiv.clone(),
        notifier.clone(),
        &config,
        ImageSize::Large,
    );

    // Changing a threshold re-evaluates cached data at the current time. An
    // expired observation must not become new queue work under the new setting.
    let now = Utc::now();
    let mut expired = illustration(20, &server.uri(), 1);
    expired.total_bookmarks = 200;
    let mut scan = repo
        .pixiv_tag_scan(
            task.id,
            now.with_timezone(&SEARCH_TIMEZONE).date_naive() - Days::new(7),
            7,
        )
        .await
        .unwrap();
    repo.save_pixiv_tag_page(
        task.id,
        &[(expired, now.timestamp() - 7 * 86400 - 1)],
        &scan,
    )
    .await
    .unwrap();
    repo.upsert_pixiv_tag_subscription(
        42,
        task.id,
        TagFilter::default(),
        &TagSettings { min_bookmarks: 100 },
    )
    .await
    .unwrap();
    // A completed scan needs no network page; exercise cached re-evaluation and
    // pruning through the production task, without another Pixiv request.
    scan.complete = true;
    engine.execute_task(&task, &mut scan, now).await.unwrap();
    assert!(repo
        .pixiv_tag_deliveries_due(sub.id, now.timestamp())
        .await
        .unwrap()
        .is_empty());

    let illust = illustration(10, &server.uri(), 11);
    repo.enqueue_pixiv_tag_delivery(sub.id, &illust, now.timestamp())
        .await
        .unwrap();
    let delivery = repo
        .pixiv_tag_deliveries_due(sub.id, now.timestamp())
        .await
        .unwrap()
        .pop()
        .unwrap();
    {
        let send = engine.send_delivery(&sub, &chat, delivery);
        tokio::pin!(send);
        // The responder signals receipt and holds the response in flight. Wait
        // for that observable event, then drop the sender without a timed sleep.
        tokio::select! {
            biased;
            _ = second_batch_arrived.notified() => {}
            result = &mut send => panic!("sender completed before interruption: {result:?}"),
        }
    }
    assert_eq!(
        repo.pixiv_push_progress(chat.id, illust.id)
            .await
            .unwrap()
            .sent_pages,
        (0..10).collect::<Vec<_>>()
    );
    assert!(repo
        .get_message_with_subscription(42, 100)
        .await
        .unwrap()
        .is_some());
    drop(engine);
    repo.db().clone().close().await.unwrap();
    drop(repo);
    let repo = Arc::new(open_repo(&url).await);
    let engine = PixivTagEngine::new(repo.clone(), pixiv, notifier, &config, ImageSize::Large);
    engine.tick().await.unwrap();
    assert!(
        repo.pixiv_push_progress(chat.id, illust.id)
            .await
            .unwrap()
            .done
    );
    assert!(repo
        .get_message_with_subscription(42, 110)
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        repo.get_task_by_type_value(TaskType::PixivTag, &task.value)
            .await
            .unwrap()
            .unwrap()
            .next_poll_at,
        next_search.naive_local()
    );
    server.verify().await;
    repo.db().clone().close().await.unwrap();
}

#[tokio::test]
async fn acknowledged_pages_and_message_mapping_roll_back_together() {
    let repo = open_repo("sqlite::memory:").await;
    let (_, sub, chat) = subscribe(&repo).await;
    let _guard = repo.lock_pixiv_push(chat.id).await;
    repo.db()
        .execute_unprepared(
            "CREATE TRIGGER fail_message BEFORE INSERT ON messages
         BEGIN SELECT RAISE(ABORT, 'injected message write failure'); END;",
        )
        .await
        .unwrap();
    assert!(repo
        .save_pixiv_push_progress(chat.id, 10, &[0], true, Some((sub.id, 100)))
        .await
        .is_err());
    assert!(!repo.pixiv_push_progress(chat.id, 10).await.unwrap().done);
    assert!(repo
        .get_message_with_subscription(chat.id, 100)
        .await
        .unwrap()
        .is_none());
    repo.db()
        .execute_unprepared("DROP TRIGGER fail_message")
        .await
        .unwrap();
    repo.save_pixiv_push_progress(chat.id, 10, &[0], true, Some((sub.id, 100)))
        .await
        .unwrap();
    assert!(repo.pixiv_push_progress(chat.id, 10).await.unwrap().done);
    assert!(repo
        .get_message_with_subscription(chat.id, 100)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn concurrent_author_and_tag_pushes_share_chat_history_after_resubscribe() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex("/image/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"mock image".to_vec()))
        .mount(&server)
        .await;
    Mock::given(path("/botfake_token/SendChatAction"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok": true, "result": true})),
        )
        .mount(&server)
        .await;
    Mock::given(path("/botfake_token/SendPhoto"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true, "result": {"message_id": 100, "date": 1700000000,
            "chat": {"id": 42, "type": "private"}}
        })))
        .expect(2)
        .mount(&server)
        .await;
    let notifier = Notifier::new(
        Bot::new("fake_token")
            .set_api_url(url::Url::parse(&server.uri()).unwrap())
            .throttle(Limits::default()),
        Arc::new(Downloader::new(
            reqwest::Client::new(),
            FileCacheManager::new(dir.path().join("cache"), 7),
        )),
    );
    let pixiv = Arc::new(RwLock::new(
        PixivClient::new(PixivConfig {
            refresh_token: "unused".to_string(),
        })
        .unwrap(),
    ));
    let repo = Arc::new(open_repo("sqlite::memory:").await);
    let (task, sub, chat) = subscribe(&repo).await;
    let author = repo
        .get_or_create_task(TaskType::Author, "1".to_string(), None)
        .await
        .unwrap();
    let author_sub = repo
        .upsert_subscription(chat.id, author.id, TagFilter::default())
        .await
        .unwrap();
    let illust = illustration(10, &server.uri(), 1);
    let now = Utc::now().timestamp();
    repo.enqueue_pixiv_tag_delivery(sub.id, &illust, now)
        .await
        .unwrap();
    let delivery = repo
        .pixiv_tag_deliveries_due(sub.id, now)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let engine = PixivTagEngine::new(
        repo.clone(),
        pixiv.clone(),
        notifier.clone(),
        &serde_json::from_value(serde_json::json!({})).unwrap(),
        ImageSize::Large,
    );

    // Poll both real send paths while the shared lock is held. Both have reached
    // the send boundary before either may proceed; no timing/sleep assumptions.
    let guard = repo.lock_pixiv_push(chat.id).await;
    let author_send = process_illust_push(
        &repo,
        &notifier,
        &pixiv,
        &chat,
        &illust,
        author_sub.id,
        ImageSize::Large,
    );
    let tag_send = engine.send_delivery(&sub, &chat, delivery);
    tokio::pin!(author_send, tag_send);
    std::future::poll_fn(|cx| {
        assert!(author_send.as_mut().poll(cx).is_pending());
        assert!(tag_send.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    drop(guard);
    let (author_result, tag_result) = tokio::join!(&mut author_send, &mut tag_send);
    assert!(matches!(author_result.unwrap(), PushResult::Success { .. }));
    tag_result.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path().ends_with("SendPhoto"))
            .count(),
        1
    );

    repo.delete_subscription(author_sub.id).await.unwrap();
    repo.delete_subscription(sub.id).await.unwrap();
    // Neither cancelling/recreating nor adding another language can reset the
    // target's history (the original queue record has been cascade-deleted).
    for query in [task.value.as_str(), "translated tag"] {
        let task = repo
            .get_or_create_task(TaskType::PixivTag, query.to_string(), None)
            .await
            .unwrap();
        repo.upsert_pixiv_tag_subscription(
            chat.id,
            task.id,
            TagFilter::default(),
            &TagSettings { min_bookmarks: 1 },
        )
        .await
        .unwrap();
        let sub = repo
            .get_subscription_by_chat_task(chat.id, task.id)
            .await
            .unwrap()
            .unwrap();
        repo.enqueue_pixiv_tag_delivery(sub.id, &illust, now)
            .await
            .unwrap();
        assert!(repo
            .pixiv_tag_deliveries_due(sub.id, now)
            .await
            .unwrap()
            .is_empty());
    }
    let other_chat = repo
        .upsert_chat(43, "private".to_string(), None, true, Default::default())
        .await
        .unwrap();
    let other_sub = repo
        .upsert_subscription(other_chat.id, author.id, TagFilter::default())
        .await
        .unwrap();
    assert!(matches!(
        process_illust_push(
            &repo,
            &notifier,
            &pixiv,
            &other_chat,
            &illust,
            other_sub.id,
            ImageSize::Large,
        )
        .await
        .unwrap(),
        PushResult::Success { .. }
    ));
    server.verify().await;
}
