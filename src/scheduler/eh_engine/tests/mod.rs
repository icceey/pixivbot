mod budget;
mod cleanup;
mod collect;
mod download;
mod publish;
mod reuse;
mod rewrite;
mod support;
mod upgrade;
mod upload;

use super::cleanup::run_eh_job_cleanup_maintenance_once;
use super::download::archive_artifacts_for_job;
use super::publish::{EhPublishCompletionHook, EhPublishSendHook};
use super::upload::collect_uploadable_zip_entry_names;
use super::*;
use crate::bot::notifier::Notifier;
use crate::cache::FileCacheManager;
use crate::config::EhentaiConfig;
use crate::db::entities::{
    eh_download_completions, eh_download_queue, eh_gallery_jobs, eh_gallery_push_ledger,
    eh_gallery_results, eh_gp_spend_attempts, tasks,
};
use crate::db::repo::eh_download_queue::{
    SOURCE_DIRECT, SOURCE_SUBSCRIPTION, STATUS_CANCELED, STATUS_DONE, STATUS_DOWNLOADED,
    STATUS_FAILED, STATUS_PENDING, STATUS_PUBLISHING,
};
use crate::db::repo::eh_gallery_jobs::{
    EhCleanupFinalizeOutcome, EhEnqueueChatLockHook, EhGalleryVariant, EhJobUploadFailureOutcome,
    EhMissingZipResetOutcome, BACKGROUND_STATUS_PENDING, BACKGROUND_STATUS_RUNNING,
    CLEANUP_STATUS_FAILED, CLEANUP_STATUS_NONE, CLEANUP_STATUS_PENDING, DELIVERY_STATUS_DONE,
    DELIVERY_STATUS_FAILED, DELIVERY_STATUS_PUBLISHING, DELIVERY_STATUS_WAITING,
    DOWNLOAD_MODE_ARCHIVE, EH_ENQUEUE_CHAT_LOCK_HOOK, JOB_STATUS_DOWNLOADED,
    JOB_STATUS_DOWNLOADING, JOB_STATUS_FAILED, JOB_STATUS_PENDING, JOB_STATUS_RETIRED,
    TELEGRAPH_REWRITE_STATUS_FAILED, TELEGRAPH_REWRITE_STATUS_PENDING, TELEGRAPH_STATUS_FAILED,
    TELEGRAPH_STATUS_NOT_REQUIRED, TELEGRAPH_STATUS_PENDING, TELEGRAPH_STATUS_READY,
    TELEGRAPH_STATUS_UPLOADING,
};
use crate::db::repo::{tests_helpers, Repo};
use crate::db::types::{EhFilter, EhPendingGallery, EhTagState, SubscriptionState, TaskType};
use crate::pixiv::downloader::Downloader;
use crate::scheduler::helpers::eh_tag_subscription_state;
use chrono::Local;
use eh_client::{
    ArchiveArtifacts, EhClient, EhClientBuilder, EhCookies, EhGallery, ImageUploadInput,
    ImageUploader, IpfS3PreviewRewriteConfig, PixiUploader, TelegraphClient, TelegraphImageUrlPair,
    ZipArchiveUploadInput,
};
use reqwest::Client;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, PaginatorTrait,
    QueryFilter, Set, Statement,
};
use std::{io::Write, sync::Arc, time::Duration};
use teloxide::{requests::RequesterExt, Bot};
use wiremock::matchers::{body_string_contains, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use support::*;

use EhDownloadQueue::{Background, Main};
