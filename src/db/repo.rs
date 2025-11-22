use sea_orm::{DbErr, EntityTrait};

pub mod users {
    pub use crate::db::entities::user::*;
    use crate::db::entities::user;
    
    use sea_orm::*;
    
    pub async fn find_by_id(db: &DatabaseConnection, id: i64) -> Result<Option<user::Model>, DbErr> {
        user::Entity::find_by_id(id).one(db).await
    }
    
    pub async fn create_if_not_exists(
        db: &DatabaseConnection,
        id: i64,
        username: Option<String>,
        is_admin: bool,
    ) -> Result<user::Model, DbErr> {
        let existing = find_by_id(db, id).await?;
        if let Some(user) = existing {
            return Ok(user);
        }
        
        let new_user = user::ActiveModel {
            id: Set(id),
            username: Set(username),
            is_admin: Set(is_admin),
            created_at: Set(chrono::Utc::now()),
        };
        
        let user = new_user.insert(db).await?;
        Ok(user)
    }
}

pub mod chats {
    pub use crate::db::entities::chat::*;
    use crate::db::entities::chat;
    
    use sea_orm::*;
    
    pub async fn find_by_id(db: &DatabaseConnection, id: i64) -> Result<Option<chat::Model>, DbErr> {
        chat::Entity::find_by_id(id).one(db).await
    }
    
    pub async fn create_if_not_exists(
        db: &DatabaseConnection,
        id: i64,
        chat_type: &str,
        title: Option<String>,
    ) -> Result<chat::Model, DbErr> {
        let existing = find_by_id(db, id).await?;
        if let Some(chat) = existing {
            return Ok(chat);
        }
        
        let new_chat = chat::ActiveModel {
            id: Set(id),
            r#type: Set(chat_type.to_string()),
            title: Set(title),
            created_at: Set(chrono::Utc::now()),
        };
        
        let chat = new_chat.insert(db).await?;
        Ok(chat)
    }
}

pub mod tasks {
    pub use crate::db::entities::task::*;
    use crate::db::entities::task;
    
    use sea_orm::*;
    
    pub async fn find_by_id(db: &DatabaseConnection, id: i32) -> Result<Option<task::Model>, DbErr> {
        task::Entity::find_by_id(id).one(db).await
    }
    
    pub async fn find_by_type_and_value(db: &DatabaseConnection, task_type: &str, value: &str) -> Result<Option<task::Model>, DbErr> {
        task::Entity::find()
            .filter(task::Column::Type.eq(task_type))
            .filter(task::Column::Value.eq(value))
            .one(db).await
    }
    
    pub async fn find_ready_tasks(db: &DatabaseConnection) -> Result<Vec<task::Model>, DbErr> {
        task::Entity::find()
            .filter(task::Column::NextPollAt.lte(chrono::Utc::now()))
            .order_by_asc(task::Column::NextPollAt)
            .limit(1) // Only one at a time for serial execution
            .all(db).await
    }
    
    pub async fn create(
        db: &DatabaseConnection,
        task_type: &str,
        value: &str,
        interval_sec: i32,
        created_by: Option<i64>,
    ) -> Result<task::Model, DbErr> {
        let new_task = task::ActiveModel {
            id: NotSet,
            r#type: Set(task_type.to_string()),
            value: Set(value.to_string()),
            interval_sec: Set(interval_sec),
            next_poll_at: Set(chrono::Utc::now()), // Poll immediately
            last_polled_at: NotSet,
            latest_data: NotSet,
            created_by: Set(created_by),
            updated_by: Set(created_by),
        };
        
        let task = new_task.insert(db).await?;
        Ok(task)
    }
    
    pub async fn update_next_poll(db: &DatabaseConnection, id: i32, next_poll_at: chrono::DateTime<chrono::Utc>) -> Result<task::Model, DbErr> {
        let task = task::ActiveModel {
            id: Set(id),
            next_poll_at: Set(next_poll_at),
            last_polled_at: Set(Some(chrono::Utc::now())),
            ..Default::default()
        };
        
        let task = task.update(db).await?;
        Ok(task)
    }
    
    pub async fn update_latest_data(db: &DatabaseConnection, id: i32, latest_data: serde_json::Value, updated_by: Option<i64>) -> Result<task::Model, DbErr> {
        let task = task::ActiveModel {
            id: Set(id),
            latest_data: Set(Some(latest_data)),
            updated_by: Set(updated_by),
            ..Default::default()
        };
        
        let task = task.update(db).await?;
        Ok(task)
    }
}

pub mod subscriptions {
    pub use crate::db::entities::subscription::*;
    use crate::db::entities::subscription;
    
    use sea_orm::*;
    
    pub async fn find_by_id(db: &DatabaseConnection, id: i32) -> Result<Option<subscription::Model>, DbErr> {
        subscription::Entity::find_by_id(id).one(db).await
    }
    
    pub async fn find_by_chat_and_task(db: &DatabaseConnection, chat_id: i64, task_id: i32) -> Result<Option<subscription::Model>, DbErr> {
        subscription::Entity::find()
            .filter(subscription::Column::ChatId.eq(chat_id))
            .filter(subscription::Column::TaskId.eq(task_id))
            .one(db).await
    }
    
    pub async fn find_by_chat(db: &DatabaseConnection, chat_id: i64) -> Result<Vec<subscription::Model>, DbErr> {
        subscription::Entity::find()
            .filter(subscription::Column::ChatId.eq(chat_id))
            .all(db).await
    }
    
    pub async fn find_by_task(db: &DatabaseConnection, task_id: i32) -> Result<Vec<subscription::Model>, DbErr> {
        subscription::Entity::find()
            .filter(subscription::Column::TaskId.eq(task_id))
            .all(db).await
    }
    
    pub async fn create_or_update(
        db: &DatabaseConnection,
        chat_id: i64,
        task_id: i32,
        filter_tags: Option<serde_json::Value>,
    ) -> Result<subscription::Model, DbErr> {
        // Check if subscription already exists
        let existing = find_by_chat_and_task(db, chat_id, task_id).await?;
        
        if let Some(sub) = existing {
            // Update existing subscription
            let updated_sub = subscription::ActiveModel {
                id: Set(sub.id),
                chat_id: Set(chat_id),
                task_id: Set(task_id),
                filter_tags: Set(filter_tags),
                ..Default::default()
            };
            
            let sub = updated_sub.update(db).await?;
            Ok(sub)
        } else {
            // Create new subscription
            let new_sub = subscription::ActiveModel {
                id: NotSet,
                chat_id: Set(chat_id),
                task_id: Set(task_id),
                filter_tags: Set(filter_tags),
                created_at: Set(chrono::Utc::now()),
            };
            
            let sub = new_sub.insert(db).await?;
            Ok(sub)
        }
    }
    
    pub async fn delete(db: &DatabaseConnection, id: i32) -> Result<DeleteResult, DbErr> {
        subscription::Entity::delete_by_id(id).exec(db).await
    }
}