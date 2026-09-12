use super::*;

#[tokio::test]
async fn test_collect_overflow_pending_enqueued_on_next_tick() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;

    // Create task and subscription
    let task_key =
        crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::default());
    let task_value = task_key.to_task_value();
    let task = repo
        .get_or_create_task(
            crate::db::types::TaskType::Ehentai,
            task_value.clone(),
            None,
        )
        .await
        .unwrap();

    // Make the task immediately available (get_or_create_task sets next_poll_at 60s in future)
    let task_id = task.id;
    let mut active: tasks::ActiveModel = task.into();
    active.next_poll_at = Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
    active.update(repo.db()).await.unwrap();

    repo.upsert_eh_subscription(-100, task_id, crate::db::types::TagFilter::default(), None)
        .await
        .unwrap();

    let eh_server = MockServer::start().await;

    mock_eh_search_with_four_galleries(&eh_server).await;
    mock_eh_metadata_for_four_galleries(&eh_server).await;

    let mut config = make_config();
    config.max_push_per_tick = 3;
    let mut engine = EhEngine::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(config),
        true,
        60,
    );
    engine.search_request_interval = Duration::ZERO;
    engine.tick().await.unwrap();

    let sub = repo
        .list_subscriptions_by_task(task_id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let state = eh_tag_subscription_state(&sub).unwrap();
    assert_eq!(state.latest_posted_ts, 0);
    assert_eq!(state.pending_galleries.len(), 1);
    assert_eq!(state.pending_galleries[0].gid, 1004);
    assert_eq!(state.pending_high_water_ts, 400);

    // Second tick: drain the pending backlog (4th gallery) before searching again.
    // The 4th gallery was overflow, not silently dropped.
    // Reset next_poll_at to make the task available again.
    let task_model = repo
        .get_task_by_type_value(crate::db::types::TaskType::Ehentai, &task_value)
        .await
        .unwrap()
        .unwrap();
    let mut active: tasks::ActiveModel = task_model.into();
    active.next_poll_at = Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
    active.update(repo.db()).await.unwrap();

    engine.tick().await.unwrap();
    let mut queued_gids: Vec<_> = eh_download_queue::Entity::find()
        .all(repo.db())
        .await
        .unwrap()
        .into_iter()
        .map(|delivery| delivery.gid)
        .collect();
    queued_gids.sort_unstable();
    assert_eq!(
        queued_gids,
        [1001, 1002, 1003, 1004],
        "draining the backlog must preserve every gallery without duplicates"
    );

    let sub = repo
        .list_subscriptions_by_task(task_id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let state = eh_tag_subscription_state(&sub).unwrap();
    assert!(state.pending_galleries.is_empty());
    assert_eq!(state.latest_posted_ts, 400);
    assert_eq!(state.pending_high_water_ts, 0);
}

#[tokio::test]
async fn test_collect_telegraph_subscription_without_token_enqueues_upload_intent() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;

    let task_key =
        crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::default());
    let task_value = task_key.to_task_value();
    let task = repo
        .get_or_create_task(
            crate::db::types::TaskType::Ehentai,
            task_value.clone(),
            None,
        )
        .await
        .unwrap();
    let task_id = task.id;
    let mut active: tasks::ActiveModel = task.into();
    active.next_poll_at = Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
    active.update(repo.db()).await.unwrap();

    repo.upsert_eh_subscription(
        -100,
        task_id,
        crate::db::types::TagFilter::default(),
        Some(crate::db::types::EhFilter {
            telegraph: true,
            ..Default::default()
        }),
    )
    .await
    .unwrap();

    let eh_server = MockServer::start().await;
    mock_eh_search_with_four_galleries(&eh_server).await;
    mock_eh_metadata_for_four_galleries(&eh_server).await;

    let mut config = make_config();
    config.upload_telegraph = true;
    config.telegraph_access_token = None;
    let mut engine = EhEngine::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(config),
        true,
        60,
    );
    engine.search_request_interval = Duration::ZERO;
    engine.tick().await.unwrap();

    let claimed_download = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    assert!(claimed_download.telegraph_required);
    repo.mark_eh_job_downloaded(
        Main,
        claimed_download.id,
        claimed_download.started_at.unwrap(),
        100,
        "data/test_cache/archive.zip",
        0,
    )
    .await
    .unwrap();

    let claimed_upload = repo.get_next_eh_job_for_upload().await.unwrap().unwrap();
    assert_eq!(claimed_upload.id, claimed_download.id);
}

#[tokio::test]
async fn test_collect_telegraph_unavailable_enqueues_archive_only() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;

    let task_key =
        crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::default());
    let task_value = task_key.to_task_value();
    let task = repo
        .get_or_create_task(crate::db::types::TaskType::Ehentai, task_value, None)
        .await
        .unwrap();
    let task_id = task.id;
    let mut active: tasks::ActiveModel = task.into();
    active.next_poll_at = Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
    active.update(repo.db()).await.unwrap();

    repo.upsert_eh_subscription(
        -100,
        task_id,
        crate::db::types::TagFilter::default(),
        Some(crate::db::types::EhFilter {
            telegraph: true,
            ..Default::default()
        }),
    )
    .await
    .unwrap();

    let eh_server = MockServer::start().await;
    mock_eh_search_with_four_galleries(&eh_server).await;
    mock_eh_metadata_for_four_galleries(&eh_server).await;

    let mut config = make_config();
    config.upload_telegraph = true;
    config.telegraph_access_token = None;
    let mut engine = EhEngine::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(config),
        false,
        60,
    );
    engine.search_request_interval = Duration::ZERO;
    engine.tick().await.unwrap();

    let claimed_download = repo
        .claim_eh_download_job(Main, true)
        .await
        .unwrap()
        .unwrap();
    assert!(!claimed_download.telegraph_required);
    let downloaded = repo
        .mark_eh_job_downloaded(
            Main,
            claimed_download.id,
            claimed_download.started_at.unwrap(),
            100,
            "data/test_cache/archive.zip",
            0,
        )
        .await
        .unwrap();

    assert!(!downloaded.telegraph_required);
    assert!(repo.get_next_eh_job_for_upload().await.unwrap().is_none());
    let delivery = eh_download_queue::Entity::find()
        .filter(eh_download_queue::Column::JobId.eq(downloaded.id))
        .one(repo.db())
        .await
        .unwrap()
        .unwrap();
    assert!(!delivery.telegraph);
    assert_eq!(delivery.status, DELIVERY_STATUS_WAITING);
}

#[tokio::test]
async fn test_collect_drains_pending_backlog_when_search_empty() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;

    let task_key =
        crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::default());
    let task_value = task_key.to_task_value();
    let task = repo
        .get_or_create_task(
            crate::db::types::TaskType::Ehentai,
            task_value.clone(),
            None,
        )
        .await
        .unwrap();
    let task_id = task.id;
    let mut active: tasks::ActiveModel = task.into();
    active.next_poll_at = Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
    active.update(repo.db()).await.unwrap();

    repo.upsert_eh_subscription(-100, task_id, crate::db::types::TagFilter::default(), None)
        .await
        .unwrap();
    let sub = repo
        .list_subscriptions_by_task(task_id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    repo.update_subscription_latest_data(
        sub.id,
        Some(SubscriptionState::EhTag(EhTagState {
            pushed_gids: Vec::new(),
            latest_posted_ts: 0,
            pending_galleries: vec![EhPendingGallery {
                gid: 2001,
                token: "eeeeeeeeee".to_string(),
                title: "Pending Gallery".to_string(),
                posted: 500,
                fingerprint: None,
            }],
            pending_high_water_ts: 500,
        })),
    )
    .await
    .unwrap();

    let eh_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(""))
        .expect(1)
        .mount(&eh_server)
        .await;

    let mut engine = EhEngine::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(make_config()),
        true,
        60,
    );
    engine.search_request_interval = Duration::ZERO;
    engine.tick().await.unwrap();

    assert_eq!(repo.count_pending_eh_downloads().await.unwrap(), 1);
    let sub = repo
        .list_subscriptions_by_task(task_id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let state = eh_tag_subscription_state(&sub).unwrap();
    assert!(state.pending_galleries.is_empty());
    assert_eq!(state.latest_posted_ts, 500);
    assert_eq!(state.pending_high_water_ts, 0);
}

#[tokio::test]
async fn collect_ledgered_gallery_does_not_consume_enqueue_slot() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let task = repo
        .get_or_create_task(
            crate::db::types::TaskType::Ehentai,
            "eh:artist:ledgered-slot".to_string(),
            None,
        )
        .await
        .unwrap();
    repo.upsert_eh_subscription(-100, task.id, crate::db::types::TagFilter::default(), None)
        .await
        .unwrap();
    let sub = repo
        .list_subscriptions_by_task(task.id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let sent_at = Local::now().naive_local();
    eh_gallery_push_ledger::ActiveModel {
        chat_id: Set(-100),
        gid: Set(2201),
        archive_sent_at: Set(Some(sent_at)),
        telegraph_sent_at: Set(Some(sent_at)),
        updated_at: Set(sent_at),
        ..Default::default()
    }
    .insert(repo.db())
    .await
    .unwrap();
    let server = MockServer::start().await;
    let engine = EhEngine::new(
        Arc::clone(&repo),
        make_eh_client(&server),
        Arc::new(make_config()),
        true,
        60,
    );
    let galleries = [
        EhGallery {
            gid: 2201,
            token: "ledgered-token".to_string(),
            title: "Ledgered Gallery".to_string(),
            title_jpn: None,
            category: "Doujinshi".to_string(),
            thumb: "https://ehgt.org/t/2201.jpg".to_string(),
            uploader: "tester".to_string(),
            posted: 2201,
            filecount: 10,
            filesize: 1000,
            expunged: false,
            rating: 4.0,
            tags: vec!["artist:ledgered-slot".to_string()],
        },
        EhGallery {
            gid: 2202,
            token: "unsent-token".to_string(),
            title: "Unsent Gallery".to_string(),
            title_jpn: None,
            category: "Doujinshi".to_string(),
            thumb: "https://ehgt.org/t/2202.jpg".to_string(),
            uploader: "tester".to_string(),
            posted: 2202,
            filecount: 10,
            filesize: 1000,
            expunged: false,
            rating: 4.0,
            tags: vec!["artist:ledgered-slot".to_string()],
        },
    ];

    engine
        .process_eh_sub_with_slots(&sub, &galleries, 1)
        .await
        .unwrap();

    assert_eq!(
        eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::ChatId.eq(-100))
            .filter(eh_download_queue::Column::Gid.eq(2201))
            .count(repo.db())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::ChatId.eq(-100))
            .filter(eh_download_queue::Column::Gid.eq(2202))
            .count(repo.db())
            .await
            .unwrap(),
        1
    );
    let updated_sub = repo
        .list_subscriptions_by_task(task.id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let state = eh_tag_subscription_state(&updated_sub).unwrap();
    assert_eq!(state.pushed_gids, vec![2201, 2202]);
    assert_eq!(state.latest_posted_ts, 2202);
    assert!(state.pending_galleries.is_empty());
    assert_eq!(state.pending_high_water_ts, 0);
}

#[tokio::test]
async fn backlog_ledgered_gallery_does_not_consume_enqueue_slot() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    let task = repo
        .get_or_create_task(
            crate::db::types::TaskType::Ehentai,
            "eh:artist:ledgered-backlog-slot".to_string(),
            None,
        )
        .await
        .unwrap();
    repo.upsert_eh_subscription(-100, task.id, crate::db::types::TagFilter::default(), None)
        .await
        .unwrap();
    let sub = repo
        .list_subscriptions_by_task(task.id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let sent_at = Local::now().naive_local();
    eh_gallery_push_ledger::ActiveModel {
        chat_id: Set(-100),
        gid: Set(2301),
        archive_sent_at: Set(Some(sent_at)),
        telegraph_sent_at: Set(Some(sent_at)),
        updated_at: Set(sent_at),
        ..Default::default()
    }
    .insert(repo.db())
    .await
    .unwrap();
    let server = MockServer::start().await;
    let engine = EhEngine::new(
        Arc::clone(&repo),
        make_eh_client(&server),
        Arc::new(make_config()),
        true,
        60,
    );
    let state = EhTagState {
        pushed_gids: Vec::new(),
        latest_posted_ts: 0,
        pending_galleries: vec![
            EhPendingGallery {
                gid: 2301,
                token: "ledgered-backlog-token".to_string(),
                title: "Ledgered Backlog Gallery".to_string(),
                posted: 2301,
                fingerprint: Some("2301|10|1000|false".to_string()),
            },
            EhPendingGallery {
                gid: 2302,
                token: "unsent-backlog-token".to_string(),
                title: "Unsent Backlog Gallery".to_string(),
                posted: 2302,
                fingerprint: Some("2302|10|1000|false".to_string()),
            },
        ],
        pending_high_water_ts: 2302,
    };

    let (_, state, remaining_slots) = engine
        .drain_pending_backlog(&sub, state, 1, true)
        .await
        .unwrap();

    assert_eq!(remaining_slots, 0);
    assert_eq!(
        eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::ChatId.eq(-100))
            .filter(eh_download_queue::Column::Gid.eq(2301))
            .count(repo.db())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        eh_download_queue::Entity::find()
            .filter(eh_download_queue::Column::ChatId.eq(-100))
            .filter(eh_download_queue::Column::Gid.eq(2302))
            .count(repo.db())
            .await
            .unwrap(),
        1
    );
    assert_eq!(state.pushed_gids, vec![2301, 2302]);
    assert_eq!(state.latest_posted_ts, 2302);
    assert!(state.pending_galleries.is_empty());
    assert_eq!(state.pending_high_water_ts, 0);
}

#[tokio::test]
async fn test_collect_drains_pending_backlog_before_search_failure() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;

    let task_key =
        crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::default());
    let task_value = task_key.to_task_value();
    let task = repo
        .get_or_create_task(
            crate::db::types::TaskType::Ehentai,
            task_value.clone(),
            None,
        )
        .await
        .unwrap();
    let task_id = task.id;
    let mut active: tasks::ActiveModel = task.into();
    active.next_poll_at = Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
    active.update(repo.db()).await.unwrap();

    repo.upsert_eh_subscription(-100, task_id, crate::db::types::TagFilter::default(), None)
        .await
        .unwrap();
    let sub = repo
        .list_subscriptions_by_task(task_id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    repo.update_subscription_latest_data(
        sub.id,
        Some(SubscriptionState::EhTag(EhTagState {
            pushed_gids: Vec::new(),
            latest_posted_ts: 0,
            pending_galleries: vec![EhPendingGallery {
                gid: 2101,
                token: "ffffffffff".to_string(),
                title: "Pending Before Failure".to_string(),
                posted: 600,
                fingerprint: None,
            }],
            pending_high_water_ts: 600,
        })),
    )
    .await
    .unwrap();

    let eh_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&eh_server)
        .await;

    let mut engine = EhEngine::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(make_config()),
        true,
        60,
    );
    engine.search_request_interval = Duration::ZERO;
    engine.tick().await.unwrap();

    assert_eq!(repo.count_pending_eh_downloads().await.unwrap(), 1);
    let sub = repo
        .list_subscriptions_by_task(task_id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let state = eh_tag_subscription_state(&sub).unwrap();
    assert!(state.pending_galleries.is_empty());
    assert_eq!(state.latest_posted_ts, 600);
    let task = repo
        .get_task_by_type_value(crate::db::types::TaskType::Ehentai, &task_value)
        .await
        .unwrap()
        .unwrap();
    assert!(task.next_poll_at > chrono::Local::now().naive_local());
}

#[tokio::test]
async fn empty_search_preserves_existing_subscription_cursor() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;
    setup_chat(&repo, -200, true).await;

    let task_key =
        crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::default());
    let task_value = task_key.to_task_value();
    let task = repo
        .get_or_create_task(
            crate::db::types::TaskType::Ehentai,
            task_value.clone(),
            None,
        )
        .await
        .unwrap();
    let task_id = task.id;
    let mut active: tasks::ActiveModel = task.into();
    active.next_poll_at = Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
    active.update(repo.db()).await.unwrap();

    repo.upsert_eh_subscription(-100, task_id, crate::db::types::TagFilter::default(), None)
        .await
        .unwrap();
    repo.upsert_eh_subscription(-200, task_id, crate::db::types::TagFilter::default(), None)
        .await
        .unwrap();
    let subs = repo.list_subscriptions_by_task(task_id).await.unwrap();
    let existing = subs.iter().find(|s| s.chat_id == -100).unwrap();
    repo.update_subscription_latest_data(
        existing.id,
        Some(SubscriptionState::EhTag(EhTagState {
            pushed_gids: vec![999],
            latest_posted_ts: 500,
            pending_galleries: Vec::new(),
            pending_high_water_ts: 0,
        })),
    )
    .await
    .unwrap();

    let eh_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(""))
        .mount(&eh_server)
        .await;

    let mut engine = EhEngine::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(make_config()),
        true,
        60,
    );
    engine.search_request_interval = Duration::ZERO;
    engine.tick().await.unwrap();

    let fresh = repo
        .list_subscriptions_by_task(task_id)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.chat_id == -200)
        .unwrap();
    let state = eh_tag_subscription_state(&fresh).unwrap();
    assert_eq!(state.latest_posted_ts, 500);
}

#[tokio::test]
async fn test_collect_enqueue_failure_persists_failed_and_remaining_backlog() {
    let repo = Arc::new(tests_helpers::setup_test_db().await.unwrap());
    setup_chat(&repo, -100, true).await;

    repo.db()
        .execute(Statement::from_string(
            DbBackend::Sqlite,
            r#"
                CREATE TRIGGER fail_eh_enqueue_1002
                BEFORE INSERT ON eh_download_queue
                WHEN NEW.gid = 1002
                BEGIN
                    SELECT RAISE(FAIL, 'injected enqueue failure');
                END
                "#,
        ))
        .await
        .unwrap();

    let task_key =
        crate::db::types::EhTaskKey::new("artist:test", 0, &crate::db::types::EhFilter::default());
    let task_value = task_key.to_task_value();
    let task = repo
        .get_or_create_task(
            crate::db::types::TaskType::Ehentai,
            task_value.clone(),
            None,
        )
        .await
        .unwrap();
    let task_id = task.id;
    let mut active: tasks::ActiveModel = task.into();
    active.next_poll_at = Set(chrono::Local::now().naive_local() - chrono::Duration::seconds(1));
    active.update(repo.db()).await.unwrap();

    repo.upsert_eh_subscription(-100, task_id, crate::db::types::TagFilter::default(), None)
        .await
        .unwrap();

    let eh_server = MockServer::start().await;
    mock_eh_search_with_four_galleries(&eh_server).await;
    mock_eh_metadata_for_four_galleries(&eh_server).await;

    let mut engine = EhEngine::new(
        Arc::clone(&repo),
        make_eh_client(&eh_server),
        Arc::new(make_config()),
        true,
        60,
    );
    engine.search_request_interval = Duration::ZERO;
    engine.tick().await.unwrap();

    assert_eq!(repo.count_pending_eh_downloads().await.unwrap(), 1);
    let sub = repo
        .list_subscriptions_by_task(task_id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let state = eh_tag_subscription_state(&sub).unwrap();
    assert_eq!(state.latest_posted_ts, 0);
    assert_eq!(state.pending_galleries.len(), 3);
    assert_eq!(state.pending_galleries[0].gid, 1002);
    assert_eq!(state.pending_galleries[1].gid, 1003);
    assert_eq!(state.pending_galleries[2].gid, 1004);
    assert_eq!(state.pending_high_water_ts, 400);
    let task = repo
        .get_task_by_type_value(crate::db::types::TaskType::Ehentai, &task_value)
        .await
        .unwrap()
        .unwrap();
    assert!(task.next_poll_at > chrono::Local::now().naive_local());
}
