use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait,
    FromQueryResult, IntoActiveModel, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, Set,
    Statement,
};

use super::entities::{chats, subscriptions, tasks, users};
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

    /// Create or update a user
    pub async fn upsert_user(
        &self,
        user_id: i64,
        username: Option<String>,
        role: UserRole,
    ) -> Result<users::Model> {
        let now = Local::now().naive_local();

        // Try to find existing user
        if let Some(existing) = users::Entity::find_by_id(user_id)
            .one(&self.db)
            .await
            .context("Failed to query user")?
        {
            // Update existing (only update username, don't change role)
            let mut active: users::ActiveModel = existing.into_active_model();
            active.username = Set(username);
            active
                .update(&self.db)
                .await
                .context("Failed to update user")
        } else {
            // Create new user with specified role
            let new_user = users::ActiveModel {
                id: Set(user_id),
                username: Set(username),
                role: Set(role),
                created_at: Set(now),
            };
            new_user
                .insert(&self.db)
                .await
                .context("Failed to insert new user")
        }
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

    /// Create or update a chat
    pub async fn upsert_chat(
        &self,
        chat_id: i64,
        chat_type: String,
        title: Option<String>,
        default_enabled: bool,
        default_sensitive_tags: Tags,
    ) -> Result<chats::Model> {
        let now = Local::now().naive_local();

        if let Some(existing) = chats::Entity::find_by_id(chat_id)
            .one(&self.db)
            .await
            .context("Failed to query chat")?
        {
            // Update existing (keep enabled status)
            let mut active: chats::ActiveModel = existing.into_active_model();
            active.r#type = Set(chat_type);
            active.title = Set(title);
            active
                .update(&self.db)
                .await
                .context("Failed to update chat")
        } else {
            // Create new with default enabled status and blur enabled by default
            let new_chat = chats::ActiveModel {
                id: Set(chat_id),
                r#type: Set(chat_type),
                title: Set(title),
                enabled: Set(default_enabled),
                blur_sensitive_tags: Set(true), // Default to enabled
                excluded_tags: Set(Tags::default()),
                sensitive_tags: Set(default_sensitive_tags),
                created_at: Set(now),
            };
            new_chat
                .insert(&self.db)
                .await
                .context("Failed to insert new chat")
        }
    }

    /// Enable or disable a chat (creates if not exists)
    pub async fn set_chat_enabled(&self, chat_id: i64, enabled: bool) -> Result<chats::Model> {
        let now = Local::now().naive_local();

        if let Some(existing) = chats::Entity::find_by_id(chat_id)
            .one(&self.db)
            .await
            .context("Failed to query chat")?
        {
            // Update existing chat
            let mut active: chats::ActiveModel = existing.into_active_model();
            active.enabled = Set(enabled);
            active
                .update(&self.db)
                .await
                .context("Failed to update chat enabled status")
        } else {
            // Create new chat with specified enabled status
            let new_chat = chats::ActiveModel {
                id: Set(chat_id),
                r#type: Set("unknown".to_string()), // Unknown type for manually added chats
                title: Set(None),
                enabled: Set(enabled),
                blur_sensitive_tags: Set(true), // Default to enabled
                excluded_tags: Set(Tags::default()),
                sensitive_tags: Set(Tags::default()),
                created_at: Set(now),
            };
            new_chat
                .insert(&self.db)
                .await
                .context("Failed to insert new chat")
        }
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

    // ==================== Tasks ====================

    /// Create a new task
    pub async fn create_task(
        &self,
        task_type: TaskType,
        value: String,
        next_poll_at: DateTime<Local>,
        author_name: Option<String>,
    ) -> Result<tasks::Model> {
        let new_task = tasks::ActiveModel {
            r#type: Set(task_type),
            value: Set(value),
            next_poll_at: Set(next_poll_at.naive_local()),
            last_polled_at: Set(None),
            author_name: Set(author_name),
            ..Default::default()
        };

        new_task
            .insert(&self.db)
            .await
            .context("Failed to create task")
    }

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

    /// Get or create a task (for subscription flow)
    pub async fn get_or_create_task(
        &self,
        task_type: TaskType,
        value: String,
        author_name: Option<String>,
    ) -> Result<tasks::Model> {
        if let Some(existing) = self.get_task_by_type_value(task_type, &value).await? {
            Ok(existing)
        } else {
            let next_poll = Local::now() + chrono::Duration::seconds(60); // Poll in 1 minute
            self.create_task(task_type, value, next_poll, author_name)
                .await
        }
    }

    /// Get tasks that need to be polled (next_poll_at <= now)
    pub async fn get_pending_tasks(&self, limit: u64) -> Result<Vec<tasks::Model>> {
        let now = Local::now().naive_local();

        tasks::Entity::find()
            .filter(tasks::Column::NextPollAt.lte(now))
            .order_by_asc(tasks::Column::NextPollAt)
            .limit(limit)
            .all(&self.db)
            .await
            .context("Failed to get pending tasks")
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

    /// Delete a task (and cascade delete subscriptions)
    pub async fn delete_task(&self, task_id: i32) -> Result<()> {
        tasks::Entity::delete_by_id(task_id)
            .exec(&self.db)
            .await
            .context("Failed to delete task")?;
        Ok(())
    }

    // ==================== Subscriptions ====================

    /// Create a subscription
    pub async fn create_subscription(
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

        new_sub
            .insert(&self.db)
            .await
            .context("Failed to create subscription")
    }

    /// Get or create subscription (upsert filter_tags if exists)
    pub async fn upsert_subscription(
        &self,
        chat_id: i64,
        task_id: i32,
        filter_tags: TagFilter,
    ) -> Result<subscriptions::Model> {
        // Check if subscription exists
        if let Some(existing) = subscriptions::Entity::find()
            .filter(subscriptions::Column::ChatId.eq(chat_id))
            .filter(subscriptions::Column::TaskId.eq(task_id))
            .one(&self.db)
            .await
            .context("Failed to query subscription")?
        {
            // Update filter_tags
            let mut active: subscriptions::ActiveModel = existing.into_active_model();
            active.filter_tags = Set(filter_tags);
            active
                .update(&self.db)
                .await
                .context("Failed to update subscription")
        } else {
            // Create new
            self.create_subscription(chat_id, task_id, filter_tags)
                .await
        }
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
    #[allow(dead_code)]
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
}
