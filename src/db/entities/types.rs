use core::fmt;

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum, Serialize, Deserialize)]
#[sea_orm(rs_type = "String", db_type = "String(StringLen::N(20))")]
pub enum TaskType {
    #[sea_orm(string_value = "author")]
    Author,
    #[sea_orm(string_value = "ranking")]
    Ranking,
}

impl fmt::Display for TaskType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskType::Author => write!(f, "author"),
            TaskType::Ranking => write!(f, "ranking"),
        }
    }
}
