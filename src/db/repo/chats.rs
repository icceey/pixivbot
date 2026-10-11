use super::Repo;
use crate::db::entities::chats;
use crate::db::types::{TagEditMode, Tags};
use anyhow::{Context, Result};
use chrono::Local;
use sea_orm::{
    sea_query::{Expr, OnConflict},
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, IntoActiveModel, QueryFilter, Set,
    Statement,
};

impl Repo {
    pub async fn upsert_chat(
        &self,
        chat_id: i64,
        chat_type: String,
        title: Option<String>,
        default_enabled: bool,
        default_sensitive_tags: Tags,
    ) -> Result<chats::Model> {
        let now = Local::now().naive_local();

        let new_chat = chats::ActiveModel {
            id: Set(chat_id),
            r#type: Set(chat_type),
            title: Set(title),
            enabled: Set(default_enabled),
            blur_sensitive_tags: Set(true),
            excluded_tags: Set(Tags::default()),
            sensitive_tags: Set(default_sensitive_tags),
            created_at: Set(now),
            allow_without_mention: Set(false),
        };

        chats::Entity::insert(new_chat)
            .on_conflict(
                OnConflict::column(chats::Column::Id)
                    .update_columns([chats::Column::Type, chats::Column::Title])
                    .to_owned(),
            )
            .exec(&self.db)
            .await
            .context("Failed to upsert chat")?;

        chats::Entity::find_by_id(chat_id)
            .one(&self.db)
            .await
            .context("Failed to fetch upserted chat")?
            .ok_or_else(|| anyhow::anyhow!("Chat {} not found after upsert", chat_id))
    }

    pub async fn set_chat_enabled(&self, chat_id: i64, enabled: bool) -> Result<chats::Model> {
        let now = Local::now().naive_local();

        let new_chat = chats::ActiveModel {
            id: Set(chat_id),
            r#type: Set("unknown".to_string()),
            title: Set(None),
            enabled: Set(enabled),
            blur_sensitive_tags: Set(true),
            excluded_tags: Set(Tags::default()),
            sensitive_tags: Set(Tags::default()),
            created_at: Set(now),
            allow_without_mention: Set(false),
        };

        chats::Entity::insert(new_chat)
            .on_conflict(
                OnConflict::column(chats::Column::Id)
                    .update_column(chats::Column::Enabled)
                    .to_owned(),
            )
            .exec(&self.db)
            .await
            .context("Failed to upsert chat enabled status")?;

        chats::Entity::find_by_id(chat_id)
            .one(&self.db)
            .await
            .context("Failed to fetch chat")?
            .ok_or_else(|| anyhow::anyhow!("Chat {} not found after upsert", chat_id))
    }

    pub async fn set_allow_without_mention(
        &self,
        chat_id: i64,
        allow: bool,
    ) -> Result<chats::Model> {
        let chat = chats::Entity::find_by_id(chat_id)
            .one(&self.db)
            .await
            .context("Failed to query chat")?
            .ok_or_else(|| anyhow::anyhow!("Chat {} not found", chat_id))?;

        let mut active: chats::ActiveModel = chat.into_active_model();
        active.allow_without_mention = Set(allow);
        active
            .update(&self.db)
            .await
            .context("Failed to update allow_without_mention")
    }

    pub async fn set_blur_sensitive_tags(&self, chat_id: i64, blur: bool) -> Result<chats::Model> {
        let chat = chats::Entity::find_by_id(chat_id)
            .one(&self.db)
            .await
            .context("Failed to query chat")?
            .ok_or_else(|| anyhow::anyhow!("Chat {} not found", chat_id))?;

        let mut active: chats::ActiveModel = chat.into_active_model();
        active.blur_sensitive_tags = Set(blur);
        active
            .update(&self.db)
            .await
            .context("Failed to update blur_sensitive_tags")
    }

    /// Compare-and-swap the selected list so simultaneous editors do not lose
    /// each other's changes. Never use a snapshot captured when the panel opened.
    pub async fn edit_chat_tags(
        &self,
        chat_id: i64,
        is_sensitive: bool,
        mode: TagEditMode,
        input: Tags,
    ) -> Result<Tags> {
        let column = if is_sensitive {
            chats::Column::SensitiveTags
        } else {
            chats::Column::ExcludedTags
        };
        for _ in 0..16 {
            let chat = self.get_chat(chat_id).await?.context("Chat not found")?;
            let current = if is_sensitive {
                chat.sensitive_tags
            } else {
                chat.excluded_tags
            };
            let updated = match mode {
                TagEditMode::Append => {
                    let mut seen = std::collections::HashSet::new();
                    Tags(
                        current
                            .iter()
                            .chain(input.iter())
                            .filter(|tag| seen.insert((*tag).clone()))
                            .cloned()
                            .collect(),
                    )
                }
                TagEditMode::Remove => Tags(
                    current
                        .iter()
                        .filter(|tag| !input.contains(tag))
                        .cloned()
                        .collect(),
                ),
                TagEditMode::Replace => input.clone(),
            };
            let result = chats::Entity::update_many()
                .col_expr(column, Expr::value(updated.clone()))
                .filter(chats::Column::Id.eq(chat_id))
                .filter(column.eq(current))
                .exec(&self.db)
                .await
                .context("Failed to edit chat tags")?;
            if result.rows_affected == 1 {
                return Ok(updated);
            }
        }
        anyhow::bail!("Chat tags changed too frequently; retry the operation")
    }

    pub async fn get_chat(&self, chat_id: i64) -> Result<Option<chats::Model>> {
        chats::Entity::find_by_id(chat_id)
            .one(&self.db)
            .await
            .context("Failed to get chat")
    }

    pub async fn migrate_chat(&self, old_chat_id: i64, new_chat_id: i64) -> Result<()> {
        use sea_orm::TransactionTrait;

        let old_chat = match self.get_chat(old_chat_id).await? {
            Some(chat) => chat,
            None => {
                if self.get_chat(new_chat_id).await?.is_some() {
                    return Ok(());
                }
                return Err(anyhow::Error::msg(format!(
                    "Old chat {} not found",
                    old_chat_id
                )));
            }
        };

        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin transaction")?;

        let new_chat = chats::ActiveModel {
            id: Set(new_chat_id),
            r#type: Set(old_chat.r#type),
            title: Set(old_chat.title),
            enabled: Set(old_chat.enabled),
            blur_sensitive_tags: Set(old_chat.blur_sensitive_tags),
            excluded_tags: Set(old_chat.excluded_tags),
            sensitive_tags: Set(old_chat.sensitive_tags),
            created_at: Set(old_chat.created_at),
            allow_without_mention: Set(old_chat.allow_without_mention),
        };

        chats::Entity::insert(new_chat)
            .on_conflict(
                OnConflict::column(chats::Column::Id)
                    .update_columns([
                        chats::Column::Type,
                        chats::Column::Title,
                        chats::Column::Enabled,
                        chats::Column::BlurSensitiveTags,
                        chats::Column::ExcludedTags,
                        chats::Column::SensitiveTags,
                        chats::Column::AllowWithoutMention,
                    ])
                    .to_owned(),
            )
            .exec(&txn)
            .await
            .context("Failed to insert new chat")?;

        let update_subscriptions = Statement::from_sql_and_values(
            self.db.get_database_backend(),
            "UPDATE subscriptions SET chat_id = ? WHERE chat_id = ?",
            vec![new_chat_id.into(), old_chat_id.into()],
        );

        txn.execute(update_subscriptions)
            .await
            .context("Failed to update subscriptions")?;

        let update_messages = Statement::from_sql_and_values(
            self.db.get_database_backend(),
            "UPDATE messages SET chat_id = ? WHERE chat_id = ?",
            vec![new_chat_id.into(), old_chat_id.into()],
        );

        txn.execute(update_messages)
            .await
            .context("Failed to update messages")?;

        chats::Entity::delete_by_id(old_chat_id)
            .exec(&txn)
            .await
            .context("Failed to delete old chat")?;

        txn.commit().await.context("Failed to commit transaction")?;

        Ok(())
    }
}
