//! E-Hentai queue snapshots and subscription filter updates.

use crate::db::repo::eh_gallery_jobs::{
    EhGalleryVariant, CLEANUP_STATUS_FAILED, JOB_STATUS_DOWNLOADED, JOB_STATUS_DOWNLOADING,
    JOB_STATUS_PENDING, JOB_STATUS_RETIRED, TELEGRAPH_STATUS_READY, TELEGRAPH_STATUS_UPLOADING,
};
use crate::db::repo::tests_helpers;
use crate::db::types::{EhFilter, EhTagState, SubscriptionState, TagFilter, TaskType};
use crate::db::{
    entities::{eh_download_queue, eh_gallery_jobs},
    repo::eh_download_queue::*,
};
use chrono::{Duration, NaiveDate};
use sea_orm::{ActiveModelTrait, EntityTrait, Set};

#[tokio::test]
async fn test_eh_queue_status_snapshot_scopes_orders_and_selects_recent_terminal() {
    const CURRENT_CHAT_ID: i64 = -100;
    const FOREIGN_CHAT_ID: i64 = -200;

    let repo = tests_helpers::setup_test_db().await.unwrap();
    let base = NaiveDate::from_ymd_opt(2026, 7, 21)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();

    for (
        gid,
        telegraph,
        job_status,
        telegraph_status,
        background_download_status,
        delivery_status,
        created_at_seconds,
    ) in [
        (
            101,
            false,
            JOB_STATUS_PENDING,
            None,
            None,
            STATUS_WAITING,
            1,
        ),
        (
            102,
            false,
            JOB_STATUS_DOWNLOADING,
            None,
            None,
            STATUS_WAITING,
            2,
        ),
        (
            103,
            false,
            JOB_STATUS_DOWNLOADED,
            None,
            None,
            STATUS_WAITING,
            3,
        ),
        (
            104,
            true,
            JOB_STATUS_DOWNLOADED,
            Some(TELEGRAPH_STATUS_UPLOADING),
            None,
            STATUS_WAITING,
            4,
        ),
        (
            105,
            true,
            JOB_STATUS_DOWNLOADED,
            Some(TELEGRAPH_STATUS_READY),
            None,
            STATUS_WAITING,
            5,
        ),
        (
            106,
            false,
            JOB_STATUS_PENDING,
            None,
            None,
            STATUS_PUBLISHING,
            6,
        ),
        (
            107,
            false,
            JOB_STATUS_PENDING,
            None,
            Some(BACKGROUND_STATUS_PENDING),
            STATUS_WAITING,
            7,
        ),
        (
            108,
            false,
            JOB_STATUS_PENDING,
            None,
            Some(BACKGROUND_STATUS_RUNNING),
            STATUS_WAITING,
            8,
        ),
    ] {
        let title = format!("Gallery {gid}");
        let model = repo
            .enqueue_eh_download(
                CURRENT_CHAT_ID,
                gid,
                "token",
                &title,
                telegraph,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let job_id = model.job_id.unwrap();
        let mut delivery: eh_download_queue::ActiveModel = model.into();
        delivery.status = Set(delivery_status.to_string());
        delivery.created_at = Set(base + Duration::seconds(created_at_seconds));
        delivery.update(repo.db()).await.unwrap();

        let mut job: eh_gallery_jobs::ActiveModel = eh_gallery_jobs::Entity::find_by_id(job_id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap()
            .into();
        job.status = Set(job_status.to_string());
        if let Some(telegraph_status) = telegraph_status {
            job.telegraph_status = Set(telegraph_status.to_string());
        }
        job.background_download_status = Set(background_download_status.map(str::to_owned));
        job.update(repo.db()).await.unwrap();
    }

    for (gid, status, created_at_seconds, completed_at_seconds, error) in [
        (201, STATUS_DONE, 20, 90, None),
        (202, STATUS_CANCELED, 25, 80, None),
        (203, STATUS_FAILED, 30, 10, Some("internal database secret")),
    ] {
        let title = format!("Gallery {gid}");
        let model = repo
            .enqueue_eh_download(
                CURRENT_CHAT_ID,
                gid,
                "token",
                &title,
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        let mut active: eh_download_queue::ActiveModel = model.into();
        active.status = Set(status.to_string());
        active.created_at = Set(base + Duration::seconds(created_at_seconds));
        active.completed_at = Set(Some(base + Duration::seconds(completed_at_seconds)));
        active.error = Set(error.map(str::to_owned));
        active.update(repo.db()).await.unwrap();
    }

    let foreign_active = repo
        .enqueue_eh_download(
            FOREIGN_CHAT_ID,
            301,
            "token",
            "Foreign active",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let mut foreign_active: eh_download_queue::ActiveModel = foreign_active.into();
    foreign_active.created_at = Set(base);
    foreign_active.update(repo.db()).await.unwrap();

    let foreign_terminal = repo
        .enqueue_eh_download(
            FOREIGN_CHAT_ID,
            302,
            "token",
            "Foreign terminal",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let mut foreign_terminal: eh_download_queue::ActiveModel = foreign_terminal.into();
    foreign_terminal.status = Set(STATUS_DONE.to_string());
    foreign_terminal.created_at = Set(base + Duration::seconds(100));
    foreign_terminal.update(repo.db()).await.unwrap();

    let snapshot = repo.get_eh_queue_snapshot(CURRENT_CHAT_ID).await.unwrap();

    assert_eq!(
        snapshot
            .active
            .iter()
            .map(|item| item.gid)
            .collect::<Vec<_>>(),
        vec![101, 102, 103, 104, 105, 106, 107, 108]
    );
    assert_eq!(snapshot.active[0].title, "Gallery 101");
    assert_eq!(
        snapshot
            .active
            .iter()
            .map(|item| item.status.as_str())
            .collect::<Vec<_>>(),
        vec![
            STATUS_PENDING,
            STATUS_DOWNLOADING,
            STATUS_DOWNLOADED,
            STATUS_UPLOADING,
            STATUS_UPLOADED,
            STATUS_PUBLISHING,
            STATUS_PENDING,
            STATUS_PENDING,
        ]
    );
    assert_eq!(
        snapshot.active[6].background_download_status.as_deref(),
        Some(BACKGROUND_STATUS_PENDING)
    );
    assert_eq!(
        snapshot.active[7].background_download_status.as_deref(),
        Some(BACKGROUND_STATUS_RUNNING)
    );

    let recent_terminal = snapshot.recent_terminal.as_ref().unwrap();
    assert_eq!(recent_terminal.gid, 203);
    assert_eq!(recent_terminal.title, "Gallery 203");
    assert_eq!(recent_terminal.status, STATUS_FAILED);
    assert!(!snapshot.active.iter().any(|item| item.gid == 301));
    assert_ne!(recent_terminal.gid, 302);
    assert!(!format!("{snapshot:?}").contains("internal database secret"));
}

#[tokio::test]
async fn estatus_joins_active_job_state_and_preserves_unbound_terminal_history() {
    const CHAT_ID: i64 = -100;

    let repo = tests_helpers::setup_test_db().await.unwrap();

    let downloading = repo
        .enqueue_eh_download(
            CHAT_ID,
            401,
            "token-401",
            "Downloading",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let mut downloading_job: eh_gallery_jobs::ActiveModel =
        eh_gallery_jobs::Entity::find_by_id(downloading.job_id.unwrap())
            .one(repo.db())
            .await
            .unwrap()
            .unwrap()
            .into();
    downloading_job.status = Set(JOB_STATUS_DOWNLOADING.to_string());
    downloading_job.zip_path = Set(Some("C:/secret-path/private.zip".to_string()));
    downloading_job.error = Set(Some("password=do-not-show".to_string()));
    downloading_job.cleanup_error = Set(Some("provider abort internal detail".to_string()));
    downloading_job.update(repo.db()).await.unwrap();

    let uploading = repo
        .enqueue_eh_download(
            CHAT_ID,
            402,
            "token-402",
            "Uploading",
            true,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let mut uploading_job: eh_gallery_jobs::ActiveModel =
        eh_gallery_jobs::Entity::find_by_id(uploading.job_id.unwrap())
            .one(repo.db())
            .await
            .unwrap()
            .unwrap()
            .into();
    uploading_job.status = Set(JOB_STATUS_DOWNLOADED.to_string());
    uploading_job.telegraph_status = Set(TELEGRAPH_STATUS_UPLOADING.to_string());
    uploading_job.update(repo.db()).await.unwrap();

    let publishing = repo
        .enqueue_eh_download(
            CHAT_ID,
            403,
            "token-403",
            "Publishing",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let mut publishing_delivery: eh_download_queue::ActiveModel = publishing.into();
    publishing_delivery.status = Set(STATUS_PUBLISHING.to_string());
    publishing_delivery.update(repo.db()).await.unwrap();

    let cleanup_pending = repo
        .enqueue_eh_download(
            CHAT_ID,
            404,
            "token-404",
            "Cleanup pending",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let mut cleanup_job: eh_gallery_jobs::ActiveModel =
        eh_gallery_jobs::Entity::find_by_id(cleanup_pending.job_id.unwrap())
            .one(repo.db())
            .await
            .unwrap()
            .unwrap()
            .into();
    cleanup_job.status = Set(JOB_STATUS_RETIRED.to_string());
    cleanup_job.cleanup_status = Set(CLEANUP_STATUS_FAILED.to_string());
    cleanup_job.update(repo.db()).await.unwrap();

    let terminal = repo
        .enqueue_eh_download(
            CHAT_ID,
            405,
            "token-405",
            "Legacy failed",
            false,
            SOURCE_DIRECT,
            &EhGalleryVariant::archive("1280x"),
            None,
            true,
        )
        .await
        .unwrap()
        .expect("delivery should be enqueued");
    let mut terminal_delivery: eh_download_queue::ActiveModel = terminal.into();
    terminal_delivery.job_id = Set(None);
    terminal_delivery.status = Set(STATUS_FAILED.to_string());
    terminal_delivery.error = Set(Some("password from legacy error".to_string()));
    terminal_delivery.update(repo.db()).await.unwrap();

    let snapshot = repo.get_eh_queue_snapshot(CHAT_ID).await.unwrap();

    assert_eq!(
        snapshot
            .active
            .iter()
            .map(|item| (item.gid, item.status.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (401, STATUS_DOWNLOADING),
            (402, STATUS_UPLOADING),
            (403, STATUS_PUBLISHING),
            (404, STATUS_PENDING),
        ]
    );
    assert_eq!(snapshot.recent_terminal.as_ref().unwrap().gid, 405);

    let output = format!("{snapshot:?}");
    for secret in ["password", "C:/secret-path", "provider abort"] {
        assert!(!output.contains(secret), "status output leaked {secret}");
    }
}

#[tokio::test]
async fn eh_subscription_upsert_updates_filters_without_replacing_subscription() {
    let repo = tests_helpers::setup_test_db().await.unwrap();
    repo.upsert_chat(-100, "private".into(), None, true, Default::default())
        .await
        .unwrap();
    let task = repo
        .get_or_create_task(TaskType::Ehentai, "eh:artist:wlop".into(), None)
        .await
        .unwrap();
    let initial = repo
        .upsert_eh_subscription(-100, task.id, TagFilter::default(), None)
        .await
        .unwrap();
    let progress = Some(SubscriptionState::EhTag(EhTagState {
        latest_posted_ts: 500,
        pushed_gids: vec![41],
        pending_galleries: vec![],
        pending_high_water_ts: 0,
    }));
    repo.update_subscription_latest_data(initial.id, progress.clone())
        .await
        .unwrap();
    for filter in [
        Some(EhFilter {
            min_rating: Some(3),
            ..Default::default()
        }),
        Some(EhFilter {
            min_rating: Some(4),
            min_pages: Some(20),
            telegraph: true,
            ..Default::default()
        }),
        None,
    ] {
        let sub = repo
            .upsert_eh_subscription(-100, task.id, TagFilter::default(), filter.clone())
            .await
            .unwrap();
        assert_eq!(sub.id, initial.id);
        assert_eq!(sub.chat_id, -100);
        assert_eq!(sub.task_id, task.id);
        assert_eq!(sub.eh_filter, filter);
        assert_eq!(sub.latest_data, progress);
    }
}
