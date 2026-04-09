use super::Repo;
use crate::db::entities::{messages, subscriptions, tasks};
use anyhow::{Context, Result};
use chrono::Local;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

impl Repo {
    pub async fn save_message(
        &self,
        chat_id: i64,
        message_id: i32,
        subscription_id: i32,
        illust_id: Option<i64>,
    ) -> Result<messages::Model> {
        let now = Local::now().naive_local();

        let new_message = messages::ActiveModel {
            chat_id: Set(chat_id),
            message_id: Set(message_id),
            subscription_id: Set(subscription_id),
            illust_id: Set(illust_id),
            created_at: Set(now),
            ..Default::default()
        };

        new_message
            .insert(&self.db)
            .await
            .context("Failed to save message")
    }

    pub async fn get_message_with_subscription(
        &self,
        chat_id: i64,
        message_id: i32,
    ) -> Result<
        Option<(
            messages::Model,
            Option<(subscriptions::Model, Option<tasks::Model>)>,
        )>,
    > {
        let message = messages::Entity::find()
            .filter(messages::Column::ChatId.eq(chat_id))
            .filter(messages::Column::MessageId.eq(message_id))
            .one(&self.db)
            .await
            .context("Failed to get message")?;

        match message {
            Some(msg) => {
                let sub_with_task = subscriptions::Entity::find_by_id(msg.subscription_id)
                    .find_also_related(tasks::Entity)
                    .one(&self.db)
                    .await
                    .context("Failed to get subscription")?;
                Ok(Some((msg, sub_with_task)))
            }
            None => Ok(None),
        }
    }
}
