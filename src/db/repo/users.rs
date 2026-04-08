use super::Repo;
use crate::db::entities::users;
use crate::db::types::UserRole;
use anyhow::{Context, Result};
use chrono::Local;
use sea_orm::{
    sea_query::OnConflict, ActiveModelTrait, EntityTrait, IntoActiveModel, PaginatorTrait,
    QueryFilter, Set,
};

impl Repo {
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

        users::Entity::insert(new_user)
            .on_conflict(
                OnConflict::column(users::Column::Id)
                    .update_column(users::Column::Username)
                    .to_owned(),
            )
            .exec(&self.db)
            .await
            .context("Failed to upsert user")?;

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

    pub async fn get_admin_users(&self) -> Result<Vec<users::Model>> {
        users::Entity::find()
            .filter(users::Column::Role.is_in([UserRole::Admin, UserRole::Owner]))
            .all(&self.db)
            .await
            .context("Failed to get admin users")
    }

    #[allow(dead_code)]
    pub async fn has_owner(&self) -> Result<bool> {
        let count = users::Entity::find()
            .filter(users::Column::Role.eq(UserRole::Owner))
            .count(&self.db)
            .await
            .context("Failed to check for owner users")?;
        Ok(count > 0)
    }

    pub async fn create_user_with_auto_owner(
        &self,
        user_id: i64,
        username: Option<String>,
    ) -> Result<users::Model> {
        use sea_orm::TransactionTrait;

        let now = Local::now().naive_local();

        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin transaction")?;

        let owner_count = users::Entity::find()
            .filter(users::Column::Role.eq(UserRole::Owner))
            .count(&txn)
            .await
            .context("Failed to check for owner users")?;

        let role = if owner_count == 0 {
            UserRole::Owner
        } else {
            UserRole::User
        };

        let new_user = users::ActiveModel {
            id: Set(user_id),
            username: Set(username.clone()),
            role: Set(role),
            created_at: Set(now),
        };

        users::Entity::insert(new_user)
            .on_conflict(
                OnConflict::column(users::Column::Id)
                    .update_column(users::Column::Username)
                    .to_owned(),
            )
            .exec(&txn)
            .await
            .context("Failed to upsert user")?;

        let user = users::Entity::find_by_id(user_id)
            .one(&txn)
            .await
            .context("Failed to fetch upserted user")?
            .ok_or_else(|| anyhow::anyhow!("User {} not found after upsert", user_id))?;

        txn.commit().await.context("Failed to commit transaction")?;

        Ok(user)
    }

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
}
