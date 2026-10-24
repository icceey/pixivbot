use anyhow::{Context, Result};
use sea_orm::prelude::DateTime;
use sea_orm::{ConnectionTrait, DatabaseTransaction, Statement};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EhPushSurface {
    Archive,
    Telegraph,
}

pub async fn record_eh_push_in_txn(
    txn: &DatabaseTransaction,
    chat_id: i64,
    gid: i64,
    surface: EhPushSurface,
    sent_at: DateTime,
) -> Result<()> {
    let (archive_sent_at, telegraph_sent_at) = match surface {
        EhPushSurface::Archive => (Some(sent_at), None),
        EhPushSurface::Telegraph => (None, Some(sent_at)),
    };
    let statement = Statement::from_sql_and_values(
        txn.get_database_backend(),
        r#"
        INSERT INTO eh_gallery_push_ledger (
            chat_id, gid, archive_sent_at, telegraph_sent_at, updated_at
        ) VALUES (?, ?, ?, ?, ?)
        ON CONFLICT(chat_id, gid) DO UPDATE SET
            archive_sent_at = COALESCE(excluded.archive_sent_at, archive_sent_at),
            telegraph_sent_at = COALESCE(excluded.telegraph_sent_at, telegraph_sent_at),
            updated_at = excluded.updated_at
        "#,
        vec![
            chat_id.into(),
            gid.into(),
            archive_sent_at.into(),
            telegraph_sent_at.into(),
            sent_at.into(),
        ],
    );
    txn.execute(statement)
        .await
        .context("Failed to record EH push ledger")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::db::entities::{eh_download_queue, eh_gallery_push_ledger};
    use crate::db::repo::eh_download_queue::{SOURCE_DIRECT, STATUS_PUBLISHING};
    use crate::db::repo::eh_gallery_jobs::EhGalleryVariant;
    use crate::db::repo::tests_helpers::setup_test_db;
    use crate::db::repo::Repo;
    use chrono::{Duration, Local};
    use sea_orm::{
        sea_query::Expr, ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter,
    };

    async fn seed_publishing_delivery(repo: &Repo, gid: i64) -> eh_download_queue::Model {
        let delivery = repo
            .enqueue_eh_download(
                -100,
                gid,
                "push-ledger-token",
                "Push ledger gallery",
                false,
                SOURCE_DIRECT,
                &EhGalleryVariant::archive("1280x"),
                None,
                true,
            )
            .await
            .unwrap()
            .expect("delivery should be enqueued");
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::Status,
                Expr::value(STATUS_PUBLISHING),
            )
            .filter(eh_download_queue::Column::Id.eq(delivery.id))
            .exec(repo.db())
            .await
            .unwrap();

        eh_download_queue::Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap()
    }

    async fn ledger_for(
        repo: &Repo,
        chat_id: i64,
        gid: i64,
    ) -> Option<eh_gallery_push_ledger::Model> {
        eh_gallery_push_ledger::Entity::find()
            .filter(eh_gallery_push_ledger::Column::ChatId.eq(chat_id))
            .filter(eh_gallery_push_ledger::Column::Gid.eq(gid))
            .one(repo.db())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn ledger_write_failure_fails_marker_transaction() {
        let repo = setup_test_db().await.unwrap();
        let delivery = seed_publishing_delivery(&repo, 402).await;
        repo.db()
            .execute_unprepared(
                "CREATE TRIGGER fail_eh_push_ledger_insert \
                 BEFORE INSERT ON eh_gallery_push_ledger \
                 BEGIN SELECT RAISE(FAIL, 'injected push ledger failure'); END",
            )
            .await
            .unwrap();

        let error = repo
            .mark_eh_archive_delivery_sent(delivery.id)
            .await
            .expect_err("ledger failure must fail the production archive marker");
        assert!(error.to_string().contains("push ledger"));
        let delivery_after_failure = eh_download_queue::Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert!(delivery_after_failure.archive_sent_at.is_none());
        assert!(ledger_for(&repo, delivery.chat_id, delivery.gid)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn ledger_upsert_is_idempotent() {
        let repo = setup_test_db().await.unwrap();
        let delivery = seed_publishing_delivery(&repo, 403).await;

        repo.mark_eh_archive_delivery_sent(delivery.id)
            .await
            .unwrap();
        let first = ledger_for(&repo, delivery.chat_id, delivery.gid)
            .await
            .expect("first production archive marker must write the push ledger");
        let old_timestamp = Local::now().naive_local() - Duration::minutes(1);
        eh_gallery_push_ledger::Entity::update_many()
            .col_expr(
                eh_gallery_push_ledger::Column::ArchiveSentAt,
                Expr::value(Some(old_timestamp)),
            )
            .col_expr(
                eh_gallery_push_ledger::Column::UpdatedAt,
                Expr::value(old_timestamp),
            )
            .filter(eh_gallery_push_ledger::Column::Id.eq(first.id))
            .exec(repo.db())
            .await
            .unwrap();
        eh_download_queue::Entity::update_many()
            .col_expr(
                eh_download_queue::Column::ArchiveSentAt,
                Expr::value(None::<chrono::NaiveDateTime>),
            )
            .filter(eh_download_queue::Column::Id.eq(delivery.id))
            .filter(eh_download_queue::Column::Status.eq(STATUS_PUBLISHING))
            .exec(repo.db())
            .await
            .unwrap();

        repo.mark_eh_archive_delivery_sent(delivery.id)
            .await
            .unwrap();
        let latest = ledger_for(&repo, delivery.chat_id, delivery.gid)
            .await
            .expect("second production archive marker must retain the push ledger row");
        let delivery_after_second_send = eh_download_queue::Entity::find_by_id(delivery.id)
            .one(repo.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.id, first.id);
        assert_eq!(
            eh_gallery_push_ledger::Entity::find()
                .filter(eh_gallery_push_ledger::Column::ChatId.eq(delivery.chat_id))
                .filter(eh_gallery_push_ledger::Column::Gid.eq(delivery.gid))
                .count(repo.db())
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            latest.archive_sent_at,
            delivery_after_second_send.archive_sent_at
        );
        assert!(latest.archive_sent_at.unwrap() > old_timestamp);
        assert!(latest.updated_at > old_timestamp);
        assert!(latest.telegraph_sent_at.is_none());
    }
}
