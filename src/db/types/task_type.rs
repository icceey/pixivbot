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
    /// E-Hentai 画廊订阅
    #[sea_orm(string_value = "eh_gallery")]
    EhGallery,
    /// E-Hentai 搜索订阅
    #[sea_orm(string_value = "eh_search")]
    EhSearch,
}

impl fmt::Display for TaskType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskType::Author => write!(f, "author"),
            TaskType::Ranking => write!(f, "ranking"),
            TaskType::EhGallery => write!(f, "eh_gallery"),
            TaskType::EhSearch => write!(f, "eh_search"),
        }
    }
}

impl TaskType {
    /// 是否是 E-Hentai 相关任务
    pub fn is_ehentai(&self) -> bool {
        matches!(self, TaskType::EhGallery | TaskType::EhSearch)
    }
}
