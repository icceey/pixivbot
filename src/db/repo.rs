use chrono::{DateTime, Local};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, IntoActiveModel,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, Set,
};
use serde_json::Value as JsonValue;

use super::entities::role::UserRole;
use super::entities::{chats, subscriptions, tasks, users};

pub struct Repo {
    db: DatabaseConnection,
}

impl Repo {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn ping(&self) -> Result<(), DbErr> {
        self.db.ping().await
    }

    // ==================== Users ====================

    /// Create or update a user
    pub async fn upsert_user(
        &self,
        user_id: i64,
        username: Option<String>,
        role: UserRole,
    ) -> Result<users::Model, DbErr> {
        let now = Local::now().naive_local();

        // Try to find existing user
        if let Some(existing) = users::Entity::find_by_id(user_id).one(&self.db).await? {
            // Update existing (only update username, don't change role)
            let mut active: users::ActiveModel = existing.into_active_model();
            active.username = Set(username);
            active.update(&self.db).await
        } else {
            // Create new user with specified role
            let new_user = users::ActiveModel {
                id: Set(user_id),
                username: Set(username),
                role: Set(role),
                created_at: Set(now),
            };
            new_user.insert(&self.db).await
        }
    }

    pub async fn get_user(&self, user_id: i64) -> Result<Option<users::Model>, DbErr> {
        users::Entity::find_by_id(user_id).one(&self.db).await
    }

    /// 获取所有管理员用户（包括 Admin 和 Owner）
    pub async fn get_admin_users(&self) -> Result<Vec<users::Model>, DbErr> {
        users::Entity::find()
            .filter(users::Column::Role.is_in([UserRole::Admin, UserRole::Owner]))
            .all(&self.db)
            .await
    }

    /// Set user role
    pub async fn set_user_role(&self, user_id: i64, role: UserRole) -> Result<users::Model, DbErr> {
        let user = users::Entity::find_by_id(user_id)
            .one(&self.db)
            .await?
            .ok_or(DbErr::RecordNotFound(format!("User {} not found", user_id)))?;

        let mut active: users::ActiveModel = user.into_active_model();
        active.role = Set(role);
        active.update(&self.db).await
    }

    // ==================== Chats ====================

    /// Create or update a chat
    pub async fn upsert_chat(
        &self,
        chat_id: i64,
        chat_type: String,
        title: Option<String>,
        default_enabled: bool,
    ) -> Result<chats::Model, DbErr> {
        let now = Local::now().naive_local();

        if let Some(existing) = chats::Entity::find_by_id(chat_id).one(&self.db).await? {
            // Update existing (keep enabled status)
            let mut active: chats::ActiveModel = existing.into_active_model();
            active.r#type = Set(chat_type);
            active.title = Set(title);
            active.update(&self.db).await
        } else {
            // Create new with default enabled status and blur enabled by default
            let new_chat = chats::ActiveModel {
                id: Set(chat_id),
                r#type: Set(chat_type),
                title: Set(title),
                enabled: Set(default_enabled),
                blur_sensitive_tags: Set(true), // Default to enabled
                excluded_tags: Set(None),
                created_at: Set(now),
            };
            new_chat.insert(&self.db).await
        }
    }

    /// Enable or disable a chat
    pub async fn set_chat_enabled(
        &self,
        chat_id: i64,
        enabled: bool,
    ) -> Result<chats::Model, DbErr> {
        let chat = chats::Entity::find_by_id(chat_id)
            .one(&self.db)
            .await?
            .ok_or(DbErr::RecordNotFound(format!("Chat {} not found", chat_id)))?;

        let mut active: chats::ActiveModel = chat.into_active_model();
        active.enabled = Set(enabled);
        active.update(&self.db).await
    }

    /// Set blur_sensitive_tags for a chat
    pub async fn set_blur_sensitive_tags(
        &self,
        chat_id: i64,
        blur: bool,
    ) -> Result<chats::Model, DbErr> {
        let chat = chats::Entity::find_by_id(chat_id)
            .one(&self.db)
            .await?
            .ok_or(DbErr::RecordNotFound(format!("Chat {} not found", chat_id)))?;

        let mut active: chats::ActiveModel = chat.into_active_model();
        active.blur_sensitive_tags = Set(blur);
        active.update(&self.db).await
    }

    /// Set excluded tags for a chat
    pub async fn set_excluded_tags(
        &self,
        chat_id: i64,
        tags: Option<JsonValue>,
    ) -> Result<chats::Model, DbErr> {
        let chat = chats::Entity::find_by_id(chat_id)
            .one(&self.db)
            .await?
            .ok_or(DbErr::RecordNotFound(format!("Chat {} not found", chat_id)))?;

        let mut active: chats::ActiveModel = chat.into_active_model();
        active.excluded_tags = Set(tags);
        active.update(&self.db).await
    }

    pub async fn get_chat(&self, chat_id: i64) -> Result<Option<chats::Model>, DbErr> {
        chats::Entity::find_by_id(chat_id).one(&self.db).await
    }

    // ==================== Tasks ====================

    /// Create a new task
    pub async fn create_task(
        &self,
        task_type: String,
        value: String,
        next_poll_at: DateTime<Local>,
        created_by: i64,
        author_name: Option<String>,
    ) -> Result<tasks::Model, DbErr> {
        let new_task = tasks::ActiveModel {
            r#type: Set(task_type),
            value: Set(value),
            next_poll_at: Set(next_poll_at.naive_local()),
            last_polled_at: Set(None),
            created_by: Set(created_by),
            author_name: Set(author_name),
            ..Default::default()
        };

        new_task.insert(&self.db).await
    }

    /// Find task by type and value
    pub async fn get_task_by_type_value(
        &self,
        task_type: &str,
        value: &str,
    ) -> Result<Option<tasks::Model>, DbErr> {
        tasks::Entity::find()
            .filter(tasks::Column::Type.eq(task_type))
            .filter(tasks::Column::Value.eq(value))
            .one(&self.db)
            .await
    }

    /// Get or create a task (for subscription flow)
    pub async fn get_or_create_task(
        &self,
        task_type: String,
        value: String,
        created_by: i64,
        author_name: Option<String>,
    ) -> Result<tasks::Model, DbErr> {
        if let Some(existing) = self.get_task_by_type_value(&task_type, &value).await? {
            Ok(existing)
        } else {
            let next_poll = Local::now() + chrono::Duration::seconds(60); // Poll in 1 minute
            self.create_task(task_type, value, next_poll, created_by, author_name)
                .await
        }
    }

    /// Get tasks that need to be polled (next_poll_at <= now)
    pub async fn get_pending_tasks(&self, limit: u64) -> Result<Vec<tasks::Model>, DbErr> {
        let now = Local::now().naive_local();

        tasks::Entity::find()
            .filter(tasks::Column::NextPollAt.lte(now))
            .order_by_asc(tasks::Column::NextPollAt)
            .limit(limit)
            .all(&self.db)
            .await
    }

    /// Update task after polling
    pub async fn update_task_after_poll(
        &self,
        task_id: i32,
        next_poll_at: DateTime<Local>,
    ) -> Result<tasks::Model, DbErr> {
        let task = tasks::Entity::find_by_id(task_id)
            .one(&self.db)
            .await?
            .ok_or(DbErr::RecordNotFound(format!("Task {} not found", task_id)))?;

        let now = Local::now().naive_local();
        let mut active: tasks::ActiveModel = task.into_active_model();
        active.next_poll_at = Set(next_poll_at.naive_local());
        active.last_polled_at = Set(Some(now));

        active.update(&self.db).await
    }

    /// Delete a task (and cascade delete subscriptions)
    pub async fn delete_task(&self, task_id: i32) -> Result<(), DbErr> {
        tasks::Entity::delete_by_id(task_id).exec(&self.db).await?;
        Ok(())
    }

    // ==================== Subscriptions ====================

    /// Create a subscription
    pub async fn create_subscription(
        &self,
        chat_id: i64,
        task_id: i32,
        filter_tags: Option<JsonValue>,
    ) -> Result<subscriptions::Model, DbErr> {
        let now = Local::now().naive_local();

        let new_sub = subscriptions::ActiveModel {
            chat_id: Set(chat_id),
            task_id: Set(task_id),
            filter_tags: Set(filter_tags),
            created_at: Set(now),
            ..Default::default()
        };

        new_sub.insert(&self.db).await
    }

    /// Get or create subscription (upsert filter_tags if exists)
    pub async fn upsert_subscription(
        &self,
        chat_id: i64,
        task_id: i32,
        filter_tags: Option<JsonValue>,
    ) -> Result<subscriptions::Model, DbErr> {
        // Check if subscription exists
        if let Some(existing) = subscriptions::Entity::find()
            .filter(subscriptions::Column::ChatId.eq(chat_id))
            .filter(subscriptions::Column::TaskId.eq(task_id))
            .one(&self.db)
            .await?
        {
            // Update filter_tags
            let mut active: subscriptions::ActiveModel = existing.into_active_model();
            active.filter_tags = Set(filter_tags);
            active.update(&self.db).await
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
    ) -> Result<Vec<(subscriptions::Model, tasks::Model)>, DbErr> {
        subscriptions::Entity::find()
            .filter(subscriptions::Column::ChatId.eq(chat_id))
            .find_also_related(tasks::Entity)
            .all(&self.db)
            .await
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
    ) -> Result<Vec<subscriptions::Model>, DbErr> {
        subscriptions::Entity::find()
            .filter(subscriptions::Column::TaskId.eq(task_id))
            .all(&self.db)
            .await
    }

    /// Delete a subscription by id
    #[allow(dead_code)]
    pub async fn delete_subscription(&self, sub_id: i32) -> Result<(), DbErr> {
        subscriptions::Entity::delete_by_id(sub_id)
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// Delete subscription by chat and task
    pub async fn delete_subscription_by_chat_task(
        &self,
        chat_id: i64,
        task_id: i32,
    ) -> Result<(), DbErr> {
        subscriptions::Entity::delete_many()
            .filter(subscriptions::Column::ChatId.eq(chat_id))
            .filter(subscriptions::Column::TaskId.eq(task_id))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// Count subscriptions for a task
    pub async fn count_subscriptions_for_task(&self, task_id: i32) -> Result<u64, DbErr> {
        subscriptions::Entity::find()
            .filter(subscriptions::Column::TaskId.eq(task_id))
            .count(&self.db)
            .await
    }

    /// Update subscription's latest_data (push state)
    pub async fn update_subscription_latest_data(
        &self,
        subscription_id: i32,
        latest_data: Option<JsonValue>,
    ) -> Result<subscriptions::Model, DbErr> {
        let subscription = subscriptions::Entity::find_by_id(subscription_id)
            .one(&self.db)
            .await?
            .ok_or(DbErr::RecordNotFound(format!(
                "Subscription {} not found",
                subscription_id
            )))?;

        let mut active: subscriptions::ActiveModel = subscription.into_active_model();
        active.latest_data = Set(latest_data);
        active.update(&self.db).await
    }

    // ==================== Statistics ====================

    /// Count all admin users (Admin + Owner)
    pub async fn count_admin_users(&self) -> Result<u64, DbErr> {
        users::Entity::find()
            .filter(users::Column::Role.is_in([UserRole::Admin, UserRole::Owner]))
            .count(&self.db)
            .await
    }

    /// Count enabled chats
    pub async fn count_enabled_chats(&self) -> Result<u64, DbErr> {
        chats::Entity::find()
            .filter(chats::Column::Enabled.eq(true))
            .count(&self.db)
            .await
    }

    /// Count all subscriptions
    pub async fn count_all_subscriptions(&self) -> Result<u64, DbErr> {
        subscriptions::Entity::find().count(&self.db).await
    }

    /// Count all tasks
    pub async fn count_all_tasks(&self) -> Result<u64, DbErr> {
        tasks::Entity::find().count(&self.db).await
    }
}
