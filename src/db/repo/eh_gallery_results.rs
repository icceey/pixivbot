use crate::db::entities::{eh_gallery_jobs, eh_gallery_results};
use crate::db::repo::eh_gallery_jobs::EhGalleryVariant;
use anyhow::{Context, Result};
use chrono::Local;
use sea_orm::{
    sea_query::Expr, ColumnTrait, ConnectionTrait, DatabaseTransaction, EntityTrait, QueryFilter,
    Statement,
};

#[allow(clippy::too_many_arguments)]
pub async fn upsert_eh_gallery_result_in_txn(
    txn: &DatabaseTransaction,
    gid: i64,
    token: &str,
    variant: &EhGalleryVariant,
    fingerprint: &str,
    source_generation: i64,
    telegraph_url: &str,
    rewrite_data: Option<&str>,
    media_cids: Option<&str>,
) -> Result<()> {
    let now = Local::now().naive_local();
    let statement = Statement::from_sql_and_values(
        txn.get_database_backend(),
        r#"
        INSERT INTO eh_gallery_results (
            gid, token, download_mode, resolution,
            source_fingerprint, source_generation, telegraph_url,
            telegraph_rewrite_data, media_cids,
            created_at, updated_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(gid, token, download_mode, resolution) DO UPDATE SET
            source_fingerprint = excluded.source_fingerprint,
            source_generation = excluded.source_generation,
            telegraph_url = excluded.telegraph_url,
            telegraph_rewrite_data = excluded.telegraph_rewrite_data,
            media_cids = excluded.media_cids,
            updated_at = excluded.updated_at
        WHERE excluded.source_generation >= eh_gallery_results.source_generation
        "#,
        vec![
            gid.into(),
            token.to_string().into(),
            variant.download_mode.clone().into(),
            variant.resolution.clone().into(),
            fingerprint.to_string().into(),
            source_generation.into(),
            telegraph_url.to_string().into(),
            rewrite_data.map(str::to_string).into(),
            media_cids.map(str::to_string).into(),
            now.into(),
            now.into(),
        ],
    );
    txn.execute(statement)
        .await
        .context("Failed to upsert cached EH gallery result")?;
    Ok(())
}

pub async fn find_eh_gallery_result_in_txn(
    txn: &DatabaseTransaction,
    gid: i64,
    token: &str,
    variant: &EhGalleryVariant,
) -> Result<Option<eh_gallery_results::Model>> {
    eh_gallery_results::Entity::find()
        .filter(eh_gallery_results::Column::Gid.eq(gid))
        .filter(eh_gallery_results::Column::Token.eq(token))
        .filter(eh_gallery_results::Column::DownloadMode.eq(&variant.download_mode))
        .filter(eh_gallery_results::Column::Resolution.eq(&variant.resolution))
        .one(txn)
        .await
        .context("Failed to find cached EH gallery result")
}

pub async fn clear_rewritten_eh_gallery_result_in_txn(
    txn: &DatabaseTransaction,
    job: &eh_gallery_jobs::Model,
) -> Result<()> {
    let (Some(fingerprint), Some(telegraph_url), Some(rewrite_data)) = (
        job.source_fingerprint.as_deref(),
        job.telegraph_url.as_deref(),
        job.telegraph_rewrite_data.as_deref(),
    ) else {
        return Ok(());
    };
    eh_gallery_results::Entity::update_many()
        .col_expr(
            eh_gallery_results::Column::TelegraphRewriteData,
            Expr::value(None::<String>),
        )
        .filter(eh_gallery_results::Column::Gid.eq(job.gid))
        .filter(eh_gallery_results::Column::Token.eq(&job.token))
        .filter(eh_gallery_results::Column::DownloadMode.eq(&job.download_mode))
        .filter(eh_gallery_results::Column::Resolution.eq(&job.resolution))
        .filter(eh_gallery_results::Column::SourceFingerprint.eq(fingerprint))
        .filter(eh_gallery_results::Column::SourceGeneration.eq(job.source_generation))
        .filter(eh_gallery_results::Column::TelegraphUrl.eq(telegraph_url))
        .filter(eh_gallery_results::Column::TelegraphRewriteData.eq(rewrite_data))
        .exec(txn)
        .await
        .context("Failed to clear completed cached EH Telegraph rewrite")?;
    Ok(())
}
