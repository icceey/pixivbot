use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum, Deserialize, Serialize, Default,
)]
#[sea_orm(rs_type = "String", db_type = "String(StringLen::N(10))")]
pub enum UserRole {
    #[sea_orm(string_value = "user")]
    #[default]
    User,
    #[sea_orm(string_value = "admin")]
    Admin,
    #[sea_orm(string_value = "owner")]
    Owner,
}

impl UserRole {
    pub fn is_owner(&self) -> bool {
        matches!(self, UserRole::Owner)
    }

    pub fn is_admin(&self) -> bool {
        matches!(self, UserRole::Admin | UserRole::Owner)
    }

    pub fn as_str(&self) -> &str {
        match self {
            UserRole::User => "user",
            UserRole::Admin => "admin",
            UserRole::Owner => "owner",
        }
    }
}

impl std::fmt::Display for UserRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}
