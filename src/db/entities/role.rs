use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum, Deserialize, Serialize)]
#[sea_orm(rs_type = "String", db_type = "String(StringLen::N(10))")]
pub enum UserRole {
    #[sea_orm(string_value = "user")]
    User,
    #[sea_orm(string_value = "admin")]
    Admin,
    #[sea_orm(string_value = "owner")]
    Owner,
}

impl Default for UserRole {
    fn default() -> Self {
        UserRole::User
    }
}

impl UserRole {
    pub fn is_owner(&self) -> bool {
        matches!(self, UserRole::Owner)
    }

    pub fn is_admin(&self) -> bool {
        matches!(self, UserRole::Admin | UserRole::Owner)
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "user" => Some(UserRole::User),
            "admin" => Some(UserRole::Admin),
            "owner" => Some(UserRole::Owner),
            _ => None,
        }
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
