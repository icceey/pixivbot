use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use sea_orm::{
    sea_query::OnConflict, ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection,
    EntityTrait, FromQueryResult, IntoActiveModel, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect, Set, Statement,
};

use super::entities::{chats, messages, subscriptions, tasks, users};
use crate::db::types::{SubscriptionState, TagFilter, Tags, TaskType, UserRole};

pub struct Repo {
    db: DatabaseConnection,
}

impl Repo {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn ping(&self) -> Result<()> {
        self.db.ping().await.context("Database ping failed")
    }

    // ==================== Users ====================

    /// Create or update a user (atomic upsert)
    /// On conflict: only updates username, preserves existing role
    pub async fn upsert_user(
        &self,
        user_id: i64,
        username: Option<String>,
        role: UserRole,
    ) -> Result<users::Model> {
        let now = Local::now().naive_local();

        let new_user = users::ActiveModel {
            id: Set(user_id),
            username: Set(username.clone()),
            role: Set(role),
            created_at: Set(now),
        };

        // INSERT ... ON CONFLICT(id) DO UPDATE SET username = excluded.username
        // This preserves the existing role when updating
        users::Entity::insert(new_user)
            .on_conflict(
                OnConflict::column(users::Column::Id)
                    .update_column(users::Column::Username)
                    .to_owned(),
            )
            .exec(&self.db)
            .await
            .context("Failed to upsert user")?;

        // Fetch the result (needed because exec_with_returning is not available for SQLite with ON CONFLICT)
        users::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .context("Failed to fetch upserted user")?
            .ok_or_else(|| anyhow::anyhow!("User {} not found after upsert", user_id))
    }

    pub async fn get_user(&self, user_id: i64) -> Result<Option<users::Model>> {
        users::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .context("Failed to get user")
    }

    /// 获取所有管理员用户（包括 Admin 和 Owner）
    pub async fn get_admin_users(&self) -> Result<Vec<users::Model>> {
        users::Entity::find()
            .filter(users::Column::Role.is_in([UserRole::Admin, UserRole::Owner]))
            .all(&self.db)
            .await
            .context("Failed to get admin users")
    }

    /// Check if any owner exists in the database
    #[allow(dead_code)]
    pub async fn has_owner(&self) -> Result<bool> {
        let count = users::Entity::find()
            .filter(users::Column::Role.eq(UserRole::Owner))
            .count(&self.db)
            .await
            .context("Failed to check for owner users")?;
        Ok(count > 0)
    }

    /// Atomically create first owner if no owner exists (using database transaction)
    /// Returns the created/existing user with their actual role
    pub async fn create_user_with_auto_owner(
        &self,
        user_id: i64,
        username: Option<String>,
    ) -> Result<users::Model> {
        use sea_orm::TransactionTrait;

        let now = Local::now().naive_local();

        // Start a transaction for atomic check-and-create
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin transaction")?;

        // Check if any owner exists (within transaction)
        let owner_count = users::Entity::find()
            .filter(users::Column::Role.eq(UserRole::Owner))
            .count(&txn)
            .await
            .context("Failed to check for owner users")?;

        // Determine role: Owner if no owner exists, otherwise User
        let role = if owner_count == 0 {
            UserRole::Owner
        } else {
            UserRole::User
        };

        // Create the user with the determined role
        let new_user = users::ActiveModel {
            id: Set(user_id),
            username: Set(username.clone()),
            role: Set(role),
            created_at: Set(now),
        };

        // INSERT ... ON CONFLICT(id) DO UPDATE SET username = excluded.username
        // This preserves the existing role when updating
        users::Entity::insert(new_user)
            .on_conflict(
                OnConflict::column(users::Column::Id)
                    .update_column(users::Column::Username)
                    .to_owned(),
            )
            .exec(&txn)
            .await
            .context("Failed to upsert user")?;

        // Fetch the result
        let user = users::Entity::find_by_id(user_id)
            .one(&txn)
            .await
            .context("Failed to fetch upserted user")?
            .ok_or_else(|| anyhow::anyhow!("User {} not found after upsert", user_id))?;

        // Commit the transaction
        txn.commit().await.context("Failed to commit transaction")?;

        Ok(user)
    }

    /// Set user role
    pub async fn set_user_role(&self, user_id: i64, role: UserRole) -> Result<users::Model> {
        let user = users::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .context("Failed to query user")?
            .ok_or_else(|| anyhow::anyhow!("User {} not found", user_id))?;

        let mut active: users::ActiveModel = user.into_active_model();
        active.role = Set(role);
        active
            .update(&self.db)
            .await
            .context("Failed to update user role")
    }

    // ==================== Chats ====================

    /// Create or update a chat (atomic upsert)
    /// On conflict: only updates type and title, preserves other settings
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
            blur_sensitive_tags: Set(true), // Default to enabled
            excluded_tags: Set(Tags::default()),
            sensitive_tags: Set(default_sensitive_tags),
            created_at: Set(now),
            allow_without_mention: Set(false), // Default to disabled
        };

        // INSERT ... ON CONFLICT(id) DO UPDATE SET type = excluded.type, title = excluded.title
        // This preserves enabled, blur_sensitive_tags, excluded_tags, sensitive_tags, allow_without_mention when updating
        chats::Entity::insert(new_chat)
            .on_conflict(
                OnConflict::column(chats::Column::Id)
                    .update_columns([chats::Column::Type, chats::Column::Title])
                    .to_owned(),
            )
            .exec(&self.db)
            .await
            .context("Failed to upsert chat")?;

        // Fetch the result
        chats::Entity::find_by_id(chat_id)
            .one(&self.db)
            .await
            .context("Failed to fetch upserted chat")?
            .ok_or_else(|| anyhow::anyhow!("Chat {} not found after upsert", chat_id))
    }

    /// Enable or disable a chat (atomic upsert - creates if not exists)
    pub async fn set_chat_enabled(&self, chat_id: i64, enabled: bool) -> Result<chats::Model> {
        let now = Local::now().naive_local();

        let new_chat = chats::ActiveModel {
            id: Set(chat_id),
            r#type: Set("unknown".to_string()), // Unknown type for manually added chats
            title: Set(None),
            enabled: Set(enabled),
            blur_sensitive_tags: Set(true), // Default to enabled
            excluded_tags: Set(Tags::default()),
            sensitive_tags: Set(Tags::default()),
            created_at: Set(now),
            allow_without_mention: Set(false), // Default to disabled
        };

        // INSERT ... ON CONFLICT(id) DO UPDATE SET enabled = excluded.enabled
        chats::Entity::insert(new_chat)
            .on_conflict(
                OnConflict::column(chats::Column::Id)
                    .update_column(chats::Column::Enabled)
                    .to_owned(),
            )
            .exec(&self.db)
            .await
            .context("Failed to upsert chat enabled status")?;

        // Fetch the result
        chats::Entity::find_by_id(chat_id)
            .one(&self.db)
            .await
            .context("Failed to fetch chat")?
            .ok_or_else(|| anyhow::anyhow!("Chat {} not found after upsert", chat_id))
    }

    /// Set allow_without_mention for a chat
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

    /// Set blur_sensitive_tags for a chat
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

    /// Set excluded tags for a chat
    pub async fn set_excluded_tags(&self, chat_id: i64, tags: Tags) -> Result<chats::Model> {
        let chat = chats::Entity::find_by_id(chat_id)
            .one(&self.db)
            .await
            .context("Failed to query chat")?
            .ok_or_else(|| anyhow::anyhow!("Chat {} not found", chat_id))?;

        let mut active: chats::ActiveModel = chat.into_active_model();
        active.excluded_tags = Set(tags);
        active
            .update(&self.db)
            .await
            .context("Failed to update excluded_tags")
    }

    /// Set sensitive tags for a chat
    pub async fn set_sensitive_tags(&self, chat_id: i64, tags: Tags) -> Result<chats::Model> {
        let chat = chats::Entity::find_by_id(chat_id)
            .one(&self.db)
            .await
            .context("Failed to query chat")?
            .ok_or_else(|| anyhow::anyhow!("Chat {} not found", chat_id))?;

        let mut active: chats::ActiveModel = chat.into_active_model();
        active.sensitive_tags = Set(tags);
        active
            .update(&self.db)
            .await
            .context("Failed to update sensitive_tags")
    }

    pub async fn get_chat(&self, chat_id: i64) -> Result<Option<chats::Model>> {
        chats::Entity::find_by_id(chat_id)
            .one(&self.db)
            .await
            .context("Failed to get chat")
    }

    /// Migrate chat from old_chat_id to new_chat_id
    /// This updates the chat's primary key and manually updates all related tables
    /// (subscriptions and messages) since SQLite doesn't support CASCADE on primary key changes.
    /// This function is idempotent - calling it multiple times with the same parameters is safe.
    pub async fn migrate_chat(&self, old_chat_id: i64, new_chat_id: i64) -> Result<()> {
        use sea_orm::TransactionTrait;

        // Get the old chat record
        let old_chat = match self.get_chat(old_chat_id).await? {
            Some(chat) => chat,
            None => {
                // If the old chat doesn't exist but the new chat already does,
                // treat this as a successful no-op to keep the operation idempotent.
                if self.get_chat(new_chat_id).await?.is_some() {
                    return Ok(());
                }
                return Err(anyhow::Error::msg(format!(
                    "Old chat {} not found",
                    old_chat_id
                )));
            }
        };

        // Start a transaction to ensure atomicity
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin transaction")?;

        // Update chat ID in chats table
        // Since we can't update primary keys directly in SQLite, we need to:
        // 1. Insert a new chat with the new ID
        // 2. Manually update all foreign key references (CASCADE doesn't work for PK changes)
        // 3. Delete the old chat
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

        // Insert new chat (or update if it already exists)
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
                    ])
                    .to_owned(),
            )
            .exec(&txn)
            .await
            .context("Failed to insert new chat")?;

        // Manually update subscriptions since CASCADE doesn't work for primary key changes
        let update_subscriptions = Statement::from_sql_and_values(
            self.db.get_database_backend(),
            "UPDATE subscriptions SET chat_id = ? WHERE chat_id = ?",
            vec![new_chat_id.into(), old_chat_id.into()],
        );

        txn.execute(update_subscriptions)
            .await
            .context("Failed to update subscriptions")?;

        // Manually update messages
        let update_messages = Statement::from_sql_and_values(
            self.db.get_database_backend(),
            "UPDATE messages SET chat_id = ? WHERE chat_id = ?",
            vec![new_chat_id.into(), old_chat_id.into()],
        );

        txn.execute(update_messages)
            .await
            .context("Failed to update messages")?;

        // Delete the old chat record
        chats::Entity::delete_by_id(old_chat_id)
            .exec(&txn)
            .await
            .context("Failed to delete old chat")?;

        // Commit the transaction
        txn.commit().await.context("Failed to commit transaction")?;

        Ok(())
    }

    // ==================== Tasks ====================

    /// Find task by type and value
    pub async fn get_task_by_type_value(
        &self,
        task_type: TaskType,
        value: &str,
    ) -> Result<Option<tasks::Model>> {
        tasks::Entity::find()
            .filter(tasks::Column::Type.eq(task_type))
            .filter(tasks::Column::Value.eq(value))
            .one(&self.db)
            .await
            .context("Failed to find task by type and value")
    }

    /// Get or create a task (atomic upsert for subscription flow)
    /// On conflict: preserves existing task, optionally updates author_name if provided
    pub async fn get_or_create_task(
        &self,
        task_type: TaskType,
        value: String,
        author_name: Option<String>,
    ) -> Result<tasks::Model> {
        let next_poll = Local::now() + chrono::Duration::seconds(60); // Poll in 1 minute

        let new_task = tasks::ActiveModel {
            r#type: Set(task_type),
            value: Set(value.clone()),
            next_poll_at: Set(next_poll.naive_local()),
            last_polled_at: Set(None),
            author_name: Set(author_name.clone()),
            ..Default::default()
        };

        // INSERT ... ON CONFLICT(type, value) DO UPDATE SET author_name = excluded.author_name
        // This ensures the upsert always succeeds (either insert or update)
        // The unique constraint is on (type, value) composite index
        let conflict_handler = if author_name.is_some() {
            // Update author_name when provided
            OnConflict::columns([tasks::Column::Type, tasks::Column::Value])
                .update_column(tasks::Column::AuthorName)
                .to_owned()
        } else {
            // When author_name is None, we still need to update something for the upsert
            // to succeed without error. Update value to itself (no-op but valid SQL).
            OnConflict::columns([tasks::Column::Type, tasks::Column::Value])
                .update_column(tasks::Column::Value)
                .to_owned()
        };

        // Use exec_without_returning to avoid "None of the records are inserted" error
        // when ON CONFLICT triggers an update instead of an insert
        tasks::Entity::insert(new_task)
            .on_conflict(conflict_handler)
            .exec_without_returning(&self.db)
            .await
            .context("Failed to upsert task")?;

        // Fetch the result by type and value
        self.get_task_by_type_value(task_type, &value)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Task with value {} not found after upsert", value))
    }

    /// Get pending tasks filtered by type
    pub async fn get_pending_tasks_by_type(
        &self,
        task_type: TaskType,
        limit: u64,
    ) -> Result<Vec<tasks::Model>> {
        let now = Local::now().naive_local();

        tasks::Entity::find()
            .filter(tasks::Column::NextPollAt.lte(now))
            .filter(tasks::Column::Type.eq(task_type))
            .order_by_asc(tasks::Column::NextPollAt)
            .limit(limit)
            .all(&self.db)
            .await
            .context("Failed to get pending tasks by type")
    }

    /// Get all tasks of a specific type (regardless of next_poll_at)
    pub async fn get_all_tasks_by_type(&self, task_type: TaskType) -> Result<Vec<tasks::Model>> {
        tasks::Entity::find()
            .filter(tasks::Column::Type.eq(task_type))
            .order_by_asc(tasks::Column::Id)
            .all(&self.db)
            .await
            .context("Failed to get all tasks by type")
    }

    /// Update task after polling
    pub async fn update_task_after_poll(
        &self,
        task_id: i32,
        next_poll_at: DateTime<Local>,
    ) -> Result<tasks::Model> {
        let task = tasks::Entity::find_by_id(task_id)
            .one(&self.db)
            .await
            .context("Failed to query task")?
            .ok_or_else(|| anyhow::anyhow!("Task {} not found", task_id))?;

        let now = Local::now().naive_local();
        let mut active: tasks::ActiveModel = task.into_active_model();
        active.next_poll_at = Set(next_poll_at.naive_local());
        active.last_polled_at = Set(Some(now));

        active
            .update(&self.db)
            .await
            .context("Failed to update task after poll")
    }

    /// Update the author_name field of a task
    pub async fn update_task_author_name(
        &self,
        task_id: i32,
        author_name: Option<String>,
    ) -> Result<tasks::Model> {
        let task = tasks::Entity::find_by_id(task_id)
            .one(&self.db)
            .await
            .context("Failed to query task")?
            .ok_or_else(|| anyhow::anyhow!("Task {} not found", task_id))?;

        let mut active: tasks::ActiveModel = task.into_active_model();
        active.author_name = Set(author_name);

        active
            .update(&self.db)
            .await
            .context("Failed to update task author_name")
    }

    /// Delete a task (and cascade delete subscriptions)
    pub async fn delete_task(&self, task_id: i32) -> Result<()> {
        tasks::Entity::delete_by_id(task_id)
            .exec(&self.db)
            .await
            .context("Failed to delete task")?;
        Ok(())
    }

    // ==================== Subscriptions ====================

    /// Get or create subscription (atomic upsert - updates filter_tags if exists)
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

        // INSERT ... ON CONFLICT(chat_id, task_id) DO UPDATE SET filter_tags = excluded.filter_tags
        subscriptions::Entity::insert(new_sub)
            .on_conflict(
                OnConflict::columns([subscriptions::Column::ChatId, subscriptions::Column::TaskId])
                    .update_column(subscriptions::Column::FilterTags)
                    .to_owned(),
            )
            .exec(&self.db)
            .await
            .context("Failed to upsert subscription")?;

        // Fetch the result
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

    /// List all subscriptions for a chat
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

    /// List all subscriptions for a task
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

    /// Delete a subscription by id
    pub async fn delete_subscription(&self, sub_id: i32) -> Result<()> {
        subscriptions::Entity::delete_by_id(sub_id)
            .exec(&self.db)
            .await
            .context("Failed to delete subscription")?;
        Ok(())
    }

    /// Delete subscription by chat and task
    pub async fn delete_subscription_by_chat_task(&self, chat_id: i64, task_id: i32) -> Result<()> {
        subscriptions::Entity::delete_many()
            .filter(subscriptions::Column::ChatId.eq(chat_id))
            .filter(subscriptions::Column::TaskId.eq(task_id))
            .exec(&self.db)
            .await
            .context("Failed to delete subscription by chat and task")?;
        Ok(())
    }

    /// Count subscriptions for a task
    pub async fn count_subscriptions_for_task(&self, task_id: i32) -> Result<u64> {
        subscriptions::Entity::find()
            .filter(subscriptions::Column::TaskId.eq(task_id))
            .count(&self.db)
            .await
            .context("Failed to count subscriptions for task")
    }

    /// Update subscription's latest_data (push state)
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

    // ==================== Statistics ====================

    /// Count all admin users (Admin + Owner)
    pub async fn count_admin_users(&self) -> Result<u64> {
        users::Entity::find()
            .filter(users::Column::Role.is_in([UserRole::Admin, UserRole::Owner]))
            .count(&self.db)
            .await
            .context("Failed to count admin users")
    }

    /// Count enabled chats (including admin chats which are always considered enabled)
    pub async fn count_enabled_chats(&self) -> Result<u64> {
        #[derive(FromQueryResult)]
        struct CountResult {
            count: i64,
        }

        // Count chats that are either:
        // 1. Explicitly enabled (enabled = true)
        // 2. Belong to an admin/owner user (who are always enabled)
        // Using DISTINCT to avoid double counting admins with enabled=true
        let result: Option<CountResult> =
            CountResult::find_by_statement(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                r#"
                SELECT COUNT(DISTINCT c.id) as count 
                FROM chats c
                LEFT JOIN users u ON c.id = u.id
                WHERE c.enabled = true OR u.role IN ('admin', 'owner')
            "#,
                [],
            ))
            .one(&self.db)
            .await
            .context("Failed to count enabled chats")?;

        Ok(result.map(|r| r.count as u64).unwrap_or(0))
    }

    /// Count all subscriptions
    pub async fn count_all_subscriptions(&self) -> Result<u64> {
        subscriptions::Entity::find()
            .count(&self.db)
            .await
            .context("Failed to count all subscriptions")
    }

    /// Count all tasks
    pub async fn count_all_tasks(&self) -> Result<u64> {
        tasks::Entity::find()
            .count(&self.db)
            .await
            .context("Failed to count all tasks")
    }

    // ==================== Messages ====================

    /// Save a sent message record
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

    /// Find a message by chat_id and message_id, return with subscription and task info
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
                // Get subscription with task
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

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{Database, DbBackend, Statement};

    async fn setup_test_db() -> Result<Repo> {
        // Create an in-memory SQLite database for testing
        let db = Database::connect("sqlite::memory:").await?;

        // Run migrations to set up the schema
        // Create tables directly since we can't use migrations in tests
        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            r#"
            CREATE TABLE users (
                id INTEGER PRIMARY KEY NOT NULL,
                username TEXT,
                role TEXT NOT NULL DEFAULT 'user',
                created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        ))
        .await?;

        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            r#"
            CREATE TABLE chats (
                id INTEGER PRIMARY KEY NOT NULL,
                type TEXT NOT NULL,
                title TEXT,
                enabled BOOLEAN NOT NULL DEFAULT 0,
                blur_sensitive_tags BOOLEAN NOT NULL DEFAULT 1,
                excluded_tags TEXT NOT NULL DEFAULT '[]',
                sensitive_tags TEXT NOT NULL DEFAULT '[]',
                created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                allow_without_mention BOOLEAN NOT NULL DEFAULT 0
            )
            "#,
        ))
        .await?;

        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            r#"
            CREATE TABLE tasks (
                id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                type TEXT NOT NULL,
                value TEXT NOT NULL,
                author_name TEXT,
                next_poll_at TIMESTAMP NOT NULL,
                last_polled_at TIMESTAMP,
                UNIQUE(type, value)
            )
            "#,
        ))
        .await?;

        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            r#"
            CREATE TABLE subscriptions (
                id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                chat_id INTEGER NOT NULL,
                task_id INTEGER NOT NULL,
                filter_tags TEXT NOT NULL DEFAULT '{"include":[],"exclude":[]}',
                latest_data TEXT,
                created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (chat_id) REFERENCES chats(id) ON DELETE CASCADE ON UPDATE CASCADE,
                FOREIGN KEY (task_id) REFERENCES tasks(id) ON DELETE CASCADE ON UPDATE CASCADE,
                UNIQUE(chat_id, task_id)
            )
            "#,
        ))
        .await?;

        db.execute(Statement::from_string(
            DbBackend::Sqlite,
            r#"
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                chat_id INTEGER NOT NULL,
                message_id INTEGER NOT NULL,
                subscription_id INTEGER NOT NULL,
                illust_id INTEGER,
                created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
            )
            "#,
        ))
        .await?;

        Ok(Repo::new(db))
    }

    #[tokio::test]
    async fn test_migrate_chat_success() {
        let repo = setup_test_db().await.unwrap();

        // Create a test chat with settings
        let old_chat_id = -888888;
        let new_chat_id = -1009999999999;

        let chat = repo
            .upsert_chat(
                old_chat_id,
                "group".to_string(),
                Some("Test Group".to_string()),
                true,
                Tags::from(vec!["nsfw".to_string()]),
            )
            .await
            .unwrap();

        assert_eq!(chat.id, old_chat_id);
        assert!(chat.enabled);
        assert_eq!(chat.title, Some("Test Group".to_string()));

        // Create a task and subscription
        let task = repo
            .get_or_create_task(
                crate::db::types::TaskType::Author,
                "12345".to_string(),
                Some("TestAuthor".to_string()),
            )
            .await
            .unwrap();

        let sub = repo
            .upsert_subscription(old_chat_id, task.id, crate::db::types::TagFilter::default())
            .await
            .unwrap();

        assert_eq!(sub.chat_id, old_chat_id);

        // Create a message
        repo.save_message(old_chat_id, 12345, sub.id, Some(67890))
            .await
            .unwrap();

        // Perform migration
        repo.migrate_chat(old_chat_id, new_chat_id).await.unwrap();

        // Verify old chat is gone
        let old_chat = repo.get_chat(old_chat_id).await.unwrap();
        assert!(old_chat.is_none());

        // Verify new chat exists with same settings
        let new_chat = repo.get_chat(new_chat_id).await.unwrap().unwrap();
        assert_eq!(new_chat.id, new_chat_id);
        assert!(new_chat.enabled);
        assert_eq!(new_chat.title, Some("Test Group".to_string()));
        assert_eq!(new_chat.r#type, "group");

        // Verify subscriptions were updated
        let subs = repo.list_subscriptions_by_chat(new_chat_id).await.unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].0.chat_id, new_chat_id);

        // Verify no subscriptions remain for old chat
        let old_subs = repo.list_subscriptions_by_chat(old_chat_id).await.unwrap();
        assert_eq!(old_subs.len(), 0);
    }

    #[tokio::test]
    async fn test_migrate_chat_idempotent() {
        let repo = setup_test_db().await.unwrap();

        let old_chat_id = -888888;
        let new_chat_id = -1009999999999;

        // Create old chat
        repo.upsert_chat(
            old_chat_id,
            "group".to_string(),
            Some("Test Group".to_string()),
            true,
            Tags::default(),
        )
        .await
        .unwrap();

        // First migration should succeed
        repo.migrate_chat(old_chat_id, new_chat_id).await.unwrap();

        // Second migration should be a no-op and not error
        let result = repo.migrate_chat(old_chat_id, new_chat_id).await;
        assert!(result.is_ok());

        // New chat should still exist
        let new_chat = repo.get_chat(new_chat_id).await.unwrap();
        assert!(new_chat.is_some());
    }

    #[tokio::test]
    async fn test_migrate_chat_old_not_found() {
        let repo = setup_test_db().await.unwrap();

        let old_chat_id = -888888;
        let new_chat_id = -1009999999999;

        // Try to migrate non-existent chat
        let result = repo.migrate_chat(old_chat_id, new_chat_id).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_migrate_chat_with_preexisting_new_chat() {
        let repo = setup_test_db().await.unwrap();

        let old_chat_id = -888888;
        let new_chat_id = -1009999999999;

        // Create both old and new chats
        repo.upsert_chat(
            old_chat_id,
            "group".to_string(),
            Some("Old Group".to_string()),
            true,
            Tags::default(),
        )
        .await
        .unwrap();

        repo.upsert_chat(
            new_chat_id,
            "supergroup".to_string(),
            Some("New Supergroup".to_string()),
            false,
            Tags::default(),
        )
        .await
        .unwrap();

        // Migration should update the new chat with old chat's settings
        repo.migrate_chat(old_chat_id, new_chat_id).await.unwrap();

        // Verify new chat was updated with old chat's settings
        let new_chat = repo.get_chat(new_chat_id).await.unwrap().unwrap();
        assert!(new_chat.enabled); // From old chat
        assert_eq!(new_chat.title, Some("Old Group".to_string())); // From old chat
    }

    #[tokio::test]
    async fn test_has_owner_empty_database() {
        let repo = setup_test_db().await.unwrap();

        // Fresh database should have no owner
        let has_owner = repo.has_owner().await.unwrap();
        assert!(!has_owner);
    }

    #[tokio::test]
    async fn test_has_owner_only_non_owner_users() {
        let repo = setup_test_db().await.unwrap();

        // Create regular user
        repo.upsert_user(12345, Some("user1".to_string()), UserRole::User)
            .await
            .unwrap();

        // Create admin user
        repo.upsert_user(67890, Some("admin1".to_string()), UserRole::Admin)
            .await
            .unwrap();

        // Should return false when only non-owner users exist
        let has_owner = repo.has_owner().await.unwrap();
        assert!(!has_owner);
    }

    #[tokio::test]
    async fn test_has_owner_with_owner() {
        let repo = setup_test_db().await.unwrap();

        // Create regular user
        repo.upsert_user(12345, Some("user1".to_string()), UserRole::User)
            .await
            .unwrap();

        // Create owner user
        repo.upsert_user(99999, Some("owner1".to_string()), UserRole::Owner)
            .await
            .unwrap();

        // Should return true when at least one owner exists
        let has_owner = repo.has_owner().await.unwrap();
        assert!(has_owner);
    }

    #[tokio::test]
    async fn test_has_owner_multiple_owners() {
        let repo = setup_test_db().await.unwrap();

        // Create first owner
        repo.upsert_user(11111, Some("owner1".to_string()), UserRole::Owner)
            .await
            .unwrap();

        // Create second owner
        repo.upsert_user(22222, Some("owner2".to_string()), UserRole::Owner)
            .await
            .unwrap();

        // Should return true when multiple owners exist (edge case)
        let has_owner = repo.has_owner().await.unwrap();
        assert!(has_owner);
    }

    #[tokio::test]
    async fn test_create_user_with_auto_owner_first_user() {
        let repo = setup_test_db().await.unwrap();

        // First user should become owner
        let user1 = repo
            .create_user_with_auto_owner(11111, Some("user1".to_string()))
            .await
            .unwrap();

        assert_eq!(user1.id, 11111);
        assert_eq!(user1.role, UserRole::Owner);

        // Second user should be regular user
        let user2 = repo
            .create_user_with_auto_owner(22222, Some("user2".to_string()))
            .await
            .unwrap();

        assert_eq!(user2.id, 22222);
        assert_eq!(user2.role, UserRole::User);
    }

    #[tokio::test]
    async fn test_create_user_with_auto_owner_concurrent() {
        use std::sync::Arc;
        use tokio::task::JoinSet;

        let repo = Arc::new(setup_test_db().await.unwrap());

        // Simulate concurrent user creation
        let mut set = JoinSet::new();
        for i in 1..=10 {
            let repo_clone = Arc::clone(&repo);
            set.spawn(async move {
                repo_clone
                    .create_user_with_auto_owner(i, Some(format!("user{}", i)))
                    .await
            });
        }

        // Collect all results
        let mut owners = 0;
        let mut users = 0;
        while let Some(result) = set.join_next().await {
            let user = result.unwrap().unwrap();
            match user.role {
                UserRole::Owner => owners += 1,
                UserRole::User => users += 1,
                _ => {}
            }
        }

        // Verify exactly one owner was created despite concurrent attempts
        assert_eq!(owners, 1, "Exactly one owner should be created");
        assert_eq!(users, 9, "Nine regular users should be created");
    }

    #[tokio::test]
    async fn test_create_user_with_auto_owner_preserves_existing_owner() {
        let repo = setup_test_db().await.unwrap();

        // Create an owner user
        let owner = repo
            .upsert_user(11111, Some("owner".to_string()), UserRole::Owner)
            .await
            .unwrap();
        assert_eq!(owner.role, UserRole::Owner);

        // Create another user to ensure owner exists
        let user = repo
            .upsert_user(22222, Some("user".to_string()), UserRole::User)
            .await
            .unwrap();
        assert_eq!(user.role, UserRole::User);

        // Call create_user_with_auto_owner on the existing owner
        // This should preserve their Owner role (ON CONFLICT only updates username)
        let owner_updated = repo
            .create_user_with_auto_owner(11111, Some("owner_updated".to_string()))
            .await
            .unwrap();

        // Verify the owner role is preserved (not downgraded to User)
        assert_eq!(owner_updated.id, 11111);
        assert_eq!(owner_updated.role, UserRole::Owner);
        assert_eq!(owner_updated.username, Some("owner_updated".to_string()));
    }
}
