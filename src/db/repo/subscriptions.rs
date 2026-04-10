use super::Repo;
use crate::db::entities::{subscriptions, tasks};
use crate::db::types::{BooruFilter, SubscriptionState, TagFilter};
use anyhow::{Context, Result};
use chrono::Local;
use sea_orm::{
    sea_query::OnConflict, ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel,
    PaginatorTrait, QueryFilter, Set,
};

impl Repo {
    pub async fn upsert_subscription(
        &self,
        chat_id: i64,
        task_id: i32,
        filter_tags: TagFilter,
    ) -> Result<subscriptions::Model> {
        let now = Local::now().naive_local();

        let new_sub = subscriptions::ActiveModel {
            chat_id: Set(chat_id),
            task_id: Set(task_id),
            filter_tags: Set(filter_tags),
            created_at: Set(now),
            ..Default::default()
        };

        subscriptions::Entity::insert(new_sub)
            .on_conflict(
                OnConflict::columns([subscriptions::Column::ChatId, subscriptions::Column::TaskId])
                    .update_column(subscriptions::Column::FilterTags)
                    .to_owned(),
            )
            .exec(&self.db)
            .await
            .context("Failed to upsert subscription")?;

        subscriptions::Entity::find()
            .filter(subscriptions::Column::ChatId.eq(chat_id))
            .filter(subscriptions::Column::TaskId.eq(task_id))
            .one(&self.db)
            .await
            .context("Failed to fetch upserted subscription")?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Subscription for chat {} task {} not found after upsert",
                    chat_id,
                    task_id
                )
            })
    }

    pub async fn list_subscriptions_by_chat(
        &self,
        chat_id: i64,
    ) -> Result<Vec<(subscriptions::Model, tasks::Model)>> {
        subscriptions::Entity::find()
            .filter(subscriptions::Column::ChatId.eq(chat_id))
            .find_also_related(tasks::Entity)
            .all(&self.db)
            .await
            .context("Failed to list subscriptions by chat")
            .map(|results| {
                results
                    .into_iter()
                    .filter_map(|(sub, task)| task.map(|t| (sub, t)))
                    .collect()
            })
    }

    pub async fn list_subscriptions_by_task(
        &self,
        task_id: i32,
    ) -> Result<Vec<subscriptions::Model>> {
        subscriptions::Entity::find()
            .filter(subscriptions::Column::TaskId.eq(task_id))
            .all(&self.db)
            .await
            .context("Failed to list subscriptions by task")
    }

    pub async fn delete_subscription(&self, sub_id: i32) -> Result<()> {
        subscriptions::Entity::delete_by_id(sub_id)
            .exec(&self.db)
            .await
            .context("Failed to delete subscription")?;
        Ok(())
    }

    pub async fn delete_subscription_by_chat_task(&self, chat_id: i64, task_id: i32) -> Result<()> {
        subscriptions::Entity::delete_many()
            .filter(subscriptions::Column::ChatId.eq(chat_id))
            .filter(subscriptions::Column::TaskId.eq(task_id))
            .exec(&self.db)
            .await
            .context("Failed to delete subscription by chat and task")?;
        Ok(())
    }

    pub async fn count_subscriptions_for_task(&self, task_id: i32) -> Result<u64> {
        subscriptions::Entity::find()
            .filter(subscriptions::Column::TaskId.eq(task_id))
            .count(&self.db)
            .await
            .context("Failed to count subscriptions for task")
    }

    pub async fn update_subscription_latest_data(
        &self,
        subscription_id: i32,
        latest_data: Option<SubscriptionState>,
    ) -> Result<subscriptions::Model> {
        let subscription = subscriptions::Entity::find_by_id(subscription_id)
            .one(&self.db)
            .await
            .context("Failed to query subscription")?
            .ok_or_else(|| anyhow::anyhow!("Subscription {} not found", subscription_id))?;

        let mut active: subscriptions::ActiveModel = subscription.into_active_model();
        active.latest_data = Set(latest_data);
        active
            .update(&self.db)
            .await
            .context("Failed to update subscription latest_data")
    }

    pub async fn upsert_booru_subscription(
        &self,
        chat_id: i64,
        task_id: i32,
        filter_tags: TagFilter,
        booru_filter: Option<BooruFilter>,
    ) -> Result<subscriptions::Model> {
        let now = Local::now().naive_local();

        let new_sub = subscriptions::ActiveModel {
            chat_id: Set(chat_id),
            task_id: Set(task_id),
            filter_tags: Set(filter_tags),
            booru_filter: Set(booru_filter),
            created_at: Set(now),
            ..Default::default()
        };

        subscriptions::Entity::insert(new_sub)
            .on_conflict(
                OnConflict::columns([subscriptions::Column::ChatId, subscriptions::Column::TaskId])
                    .update_columns([
                        subscriptions::Column::FilterTags,
                        subscriptions::Column::BooruFilter,
                    ])
                    .to_owned(),
            )
            .exec(&self.db)
            .await
            .context("Failed to upsert booru subscription")?;

        subscriptions::Entity::find()
            .filter(subscriptions::Column::ChatId.eq(chat_id))
            .filter(subscriptions::Column::TaskId.eq(task_id))
            .one(&self.db)
            .await
            .context("Failed to fetch upserted booru subscription")?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Booru subscription for chat {} task {} not found after upsert",
                    chat_id,
                    task_id
                )
            })
    }
}
