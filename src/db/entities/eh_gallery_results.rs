use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Deserialize, Serialize)]
#[sea_orm(table_name = "eh_gallery_results")]
#[allow(dead_code)]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub gid: i64,
    pub token: String,
    pub download_mode: String,
    pub resolution: String,
    pub source_fingerprint: String,
    pub telegraph_url: String,
    pub telegraph_rewrite_data: Option<String>,
    pub media_cids: Option<String>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
#[allow(dead_code)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
