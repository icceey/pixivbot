use super::Repo;
use crate::db::entities::users;
use crate::db::types::UserRole;
use anyhow::{Context, Result};
use chrono::Local;
use sea_orm::{sea_query::OnConflict, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, Set};

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

    /// Cached names are candidates only; callers must verify with Telegram.
    pub async fn get_username_candidates(&self, username: &str) -> Result<Vec<users::Model>> {
        use sea_orm::sea_query::{Expr, Func};
        users::Entity::find()
            .filter(
                Expr::expr(Func::lower(Expr::col(users::Column::Username)))
                    .eq(username.to_ascii_lowercase()),
            )
            .all(&self.db)
            .await
            .context("Failed to find username candidates")
    }

    pub async fn get_admin_users(&self) -> Result<Vec<users::Model>> {
        users::Entity::find()
            .filter(users::Column::Role.is_in([UserRole::Admin, UserRole::Owner]))
            .all(&self.db)
            .await
            .context("Failed to get admin users")
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

    /// A validated reply identifies a real sender, but its username may be old.
    /// Cache it only for a new row; never overwrite existing identity metadata or
    /// run the first-contact Owner initialization path.
    pub async fn set_admin_from_reply(
        &self,
        user_id: i64,
        username: Option<String>,
    ) -> Result<users::Model> {
        use sea_orm::TransactionTrait;
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to start reply promotion")?;
        users::Entity::insert(users::ActiveModel {
            id: Set(user_id),
            username: Set(username),
            role: Set(UserRole::User),
            created_at: Set(Local::now().naive_local()),
        })
        .on_conflict(
            OnConflict::column(users::Column::Id)
                .do_nothing()
                .to_owned(),
        )
        .exec_without_returning(&txn)
        .await
        .context("Failed to register replied user")?;
        let result = users::Entity::update_many()
            .col_expr(
                users::Column::Role,
                sea_orm::sea_query::Expr::value(UserRole::Admin),
            )
            .filter(users::Column::Id.eq(user_id))
            .filter(users::Column::Role.ne(UserRole::Owner))
            .exec(&txn)
            .await
            .context("Failed to promote replied user")?;
        anyhow::ensure!(result.rows_affected == 1, "Cannot modify Owner");
        let user = users::Entity::find_by_id(user_id)
            .one(&txn)
            .await?
            .context("Replied user missing")?;
        txn.commit()
            .await
            .context("Failed to commit reply promotion")?;
        Ok(user)
    }

    pub async fn set_user_role(&self, user_id: i64, role: UserRole) -> Result<users::Model> {
        anyhow::ensure!(!role.is_owner(), "Owner assignment is not supported here");
        let result = users::Entity::update_many()
            .col_expr(users::Column::Role, sea_orm::sea_query::Expr::value(role))
            .filter(users::Column::Id.eq(user_id))
            .filter(users::Column::Role.ne(UserRole::Owner))
            .exec(&self.db)
            .await
            .context("Failed to update user role")?;
        anyhow::ensure!(result.rows_affected == 1, "User missing or protected Owner");
        self.get_user(user_id)
            .await?
            .context("Updated user not found")
    }
}
