//! Integration tests for e-hentai DB layer: full download queue lifecycle,
//! upsert_eh_subscription, and EhFilter/EhTaskKey roundtrip with DB.

use crate::db::repo::tests_helpers;
use crate::db::types::{EhFilter, EhTagState, EhTaskKey, SubscriptionState, TagFilter, TaskType};

#[tokio::test]
async fn test_upsert_eh_subscription_insert() {
    let repo = tests_helpers::setup_test_db().await.unwrap();

    // Create a chat first
    repo.upsert_chat(-100, "private".into(), None, true, Default::default())
        .await
        .unwrap();

    // Create a task
    let task = repo
        .get_or_create_task(TaskType::Ehentai, "eh:female:elf|c=0|f=".to_string(), None)
        .await
        .unwrap();

    // Upsert eh subscription
    let filter = EhFilter {
        min_rating: Some(4),
        min_pages: None,
        max_pages: None,
        telegraph: true,
    };

    let sub = repo
        .upsert_eh_subscription(-100, task.id, TagFilter::default(), Some(filter.clone()))
        .await
        .unwrap();

    assert_eq!(sub.chat_id, -100);
    assert_eq!(sub.task_id, task.id);
    assert_eq!(sub.eh_filter, Some(filter));
    assert!(sub.latest_data.is_none());
}

#[tokio::test]
async fn test_upsert_eh_subscription_update_on_conflict() {
    let repo = tests_helpers::setup_test_db().await.unwrap();

    repo.upsert_chat(-100, "private".into(), None, true, Default::default())
        .await
        .unwrap();

    let task = repo
        .get_or_create_task(TaskType::Ehentai, "eh:artist:wlop|c=0|f=".to_string(), None)
        .await
        .unwrap();

    // First insert
    let filter1 = EhFilter {
        min_rating: Some(3),
        min_pages: None,
        max_pages: None,
        telegraph: false,
    };
    let sub1 = repo
        .upsert_eh_subscription(-100, task.id, TagFilter::default(), Some(filter1.clone()))
        .await
        .unwrap();

    // Second upsert (should update, not insert duplicate)
    let filter2 = EhFilter {
        min_rating: Some(4),
        min_pages: Some(20),
        max_pages: None,
        telegraph: true,
    };
    let sub2 = repo
        .upsert_eh_subscription(-100, task.id, TagFilter::default(), Some(filter2.clone()))
        .await
        .unwrap();

    assert_eq!(sub1.id, sub2.id); // same subscription
    assert_eq!(sub2.eh_filter, Some(filter2)); // filter updated
}

#[tokio::test]
async fn test_upsert_eh_subscription_with_no_filter() {
    let repo = tests_helpers::setup_test_db().await.unwrap();

    repo.upsert_chat(-100, "private".into(), None, true, Default::default())
        .await
        .unwrap();

    let task = repo
        .get_or_create_task(TaskType::Ehentai, "eh:manga|c=0|f=".to_string(), None)
        .await
        .unwrap();

    let sub = repo
        .upsert_eh_subscription(-100, task.id, TagFilter::default(), None)
        .await
        .unwrap();

    assert_eq!(sub.eh_filter, None);
}

#[tokio::test]
async fn test_update_subscription_latest_data_eh_tag() {
    let repo = tests_helpers::setup_test_db().await.unwrap();

    repo.upsert_chat(-100, "private".into(), None, true, Default::default())
        .await
        .unwrap();

    let task = repo
        .get_or_create_task(TaskType::Ehentai, "eh:test|c=0|f=".to_string(), None)
        .await
        .unwrap();

    let sub = repo
        .upsert_eh_subscription(-100, task.id, TagFilter::default(), None)
        .await
        .unwrap();

    // Set initial state
    let state = SubscriptionState::EhTag(EhTagState {
        pushed_gids: vec![100, 200],
        latest_posted_ts: 1700000000,
        pending_galleries: Vec::new(),
        pending_high_water_ts: 0,
    });

    repo.update_subscription_latest_data(sub.id, Some(state.clone()))
        .await
        .unwrap();

    // Verify it was saved by listing subscriptions and checking latest_data
    let subs = repo.list_subscriptions_by_task(task.id).await.unwrap();
    assert_eq!(subs.len(), 1);
    let saved = &subs[0];
    assert!(saved.latest_data.is_some());
    let saved_state = saved.latest_data.as_ref().unwrap();
    match saved_state {
        SubscriptionState::EhTag(s) => {
            assert_eq!(s.pushed_gids, vec![100, 200]);
            assert_eq!(s.latest_posted_ts, 1700000000);
        }
        _ => panic!("expected EhTag state"),
    }
}

#[tokio::test]
async fn test_eh_download_queue_full_lifecycle() {
    let repo = tests_helpers::setup_test_db().await.unwrap();

    // Enqueue 3 downloads
    let m1 = repo
        .enqueue_eh_download(-100, 100, "tok1", "Gallery 1", false, "subscription")
        .await
        .unwrap();
    let m2 = repo
        .enqueue_eh_download(-100, 200, "tok2", "Gallery 2", true, "subscription")
        .await
        .unwrap();
    let m3 = repo
        .enqueue_eh_download(-100, 300, "tok3", "Gallery 3", false, "direct")
        .await
        .unwrap();

    assert_eq!(m1.status, "pending");
    assert!(m2.telegraph);
    assert_eq!(m3.source, "direct");

    // FIFO: get m1 first (download stage)
    let next1 = repo.get_next_pending_eh_download().await.unwrap().unwrap();
    assert_eq!(next1.id, m1.id);
    assert_eq!(next1.status, "downloading");

    // Download m1
    repo.mark_eh_download_downloaded(m1.id, 50000, "/tmp/100.zip", 0)
        .await
        .unwrap();

    // Complete m1 (publish stage)
    let pub1 = repo.get_next_for_publish().await.unwrap().unwrap();
    assert_eq!(pub1.id, m1.id);
    repo.mark_eh_download_done(m1.id, 50000).await.unwrap();

    // Get m2
    let next2 = repo.get_next_pending_eh_download().await.unwrap().unwrap();
    assert_eq!(next2.id, m2.id);

    // Fail m2
    repo.mark_eh_download_failed(m2.id, "network timeout")
        .await
        .unwrap();

    // Get m3
    let next3 = repo.get_next_pending_eh_download().await.unwrap().unwrap();
    assert_eq!(next3.id, m3.id);

    // No more pending
    let none = repo.get_next_pending_eh_download().await.unwrap();
    assert!(none.is_none());

    // Verify downloaded bytes (only m1 was done)
    let bytes = repo.get_eh_downloaded_bytes_in_window(24).await.unwrap();
    assert_eq!(bytes, 50000);
}

#[tokio::test]
async fn test_eh_download_queue_fifo_ordering() {
    let repo = tests_helpers::setup_test_db().await.unwrap();

    // Enqueue in order
    let m1 = repo
        .enqueue_eh_download(-100, 1, "a", "A", false, "direct")
        .await
        .unwrap();
    let m2 = repo
        .enqueue_eh_download(-100, 2, "b", "B", false, "direct")
        .await
        .unwrap();
    let m3 = repo
        .enqueue_eh_download(-100, 3, "c", "C", false, "direct")
        .await
        .unwrap();

    // Should be FIFO (oldest first by created_at)
    let next1 = repo.get_next_pending_eh_download().await.unwrap().unwrap();
    assert_eq!(next1.id, m1.id);
    repo.mark_eh_download_downloaded(m1.id, 100, "/tmp/1.zip", 0)
        .await
        .unwrap();
    let pub1 = repo.get_next_for_publish().await.unwrap().unwrap();
    assert_eq!(pub1.id, m1.id);
    repo.mark_eh_download_done(m1.id, 100).await.unwrap();

    let next2 = repo.get_next_pending_eh_download().await.unwrap().unwrap();
    assert_eq!(next2.id, m2.id);
    repo.mark_eh_download_downloaded(m2.id, 200, "/tmp/2.zip", 0)
        .await
        .unwrap();
    let pub2 = repo.get_next_for_publish().await.unwrap().unwrap();
    assert_eq!(pub2.id, m2.id);
    repo.mark_eh_download_done(m2.id, 200).await.unwrap();

    let next3 = repo.get_next_pending_eh_download().await.unwrap().unwrap();
    assert_eq!(next3.id, m3.id);
}

#[tokio::test]
async fn test_eh_download_queue_reset_stale_then_reprocess() {
    let repo = tests_helpers::setup_test_db().await.unwrap();

    let m = repo
        .enqueue_eh_download(-100, 1, "tok", "T", false, "direct")
        .await
        .unwrap();

    // Mark as downloading (simulating a crash mid-download)
    repo.get_next_pending_eh_download().await.unwrap();

    // Reset stale
    let count = repo.reset_stale_eh_downloads().await.unwrap();
    assert_eq!(count, 1);

    // Should be processable again
    let next = repo.get_next_pending_eh_download().await.unwrap().unwrap();
    assert_eq!(next.id, m.id);

    // Complete through full pipeline
    repo.mark_eh_download_downloaded(m.id, 1000, "/tmp/1.zip", 0)
        .await
        .unwrap();
    let pub_next = repo.get_next_for_publish().await.unwrap().unwrap();
    assert_eq!(pub_next.id, m.id);
    repo.mark_eh_download_done(m.id, 1000).await.unwrap();

    let bytes = repo.get_eh_downloaded_bytes_in_window(24).await.unwrap();
    assert_eq!(bytes, 1000);
}

#[tokio::test]
async fn test_eh_task_key_db_roundtrip() {
    let repo = tests_helpers::setup_test_db().await.unwrap();

    // Create task with EhTaskKey value
    let filter = EhFilter {
        min_rating: Some(4),
        min_pages: Some(20),
        max_pages: None,
        telegraph: false,
    };
    let key = EhTaskKey::new("female:elf", 0, &filter);
    let task_value = key.to_task_value();

    let task = repo
        .get_or_create_task(TaskType::Ehentai, task_value.clone(), None)
        .await
        .unwrap();

    assert_eq!(task.r#type, TaskType::Ehentai);
    assert_eq!(task.value, task_value);

    // Retrieve by type+value
    let found = repo
        .get_task_by_type_value(TaskType::Ehentai, &task_value)
        .await
        .unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().id, task.id);

    // Parse back
    let parsed = EhTaskKey::parse(&task.value).expect("should parse");
    assert_eq!(parsed.query, "female:elf");
    assert_eq!(parsed.category_bitmask, 0);
    assert_eq!(parsed.filter_sig, "r4p20");
}

#[tokio::test]
async fn test_eh_download_queue_rate_limit_window() {
    let repo = tests_helpers::setup_test_db().await.unwrap();

    // Complete 3 downloads through the full pipeline
    for i in 1..=3i64 {
        let m = repo
            .enqueue_eh_download(-100, i, "tok", "T", false, "direct")
            .await
            .unwrap();
        let c = repo.get_next_pending_eh_download().await.unwrap().unwrap();
        assert_eq!(c.id, m.id);
        repo.mark_eh_download_downloaded(m.id, i * 1000, &format!("/tmp/{}.zip", i), 0)
            .await
            .unwrap();
        let p = repo.get_next_for_publish().await.unwrap().unwrap();
        assert_eq!(p.id, m.id);
        repo.mark_eh_download_done(m.id, i * 1000).await.unwrap();
    }

    // 24h window should include all
    let bytes_24h = repo.get_eh_downloaded_bytes_in_window(24).await.unwrap();
    assert_eq!(bytes_24h, 6000); // 1000 + 2000 + 3000

    // 1h window should also include all (completed_at is ~now)
    let bytes_1h = repo.get_eh_downloaded_bytes_in_window(1).await.unwrap();
    assert_eq!(bytes_1h, 6000);
}
