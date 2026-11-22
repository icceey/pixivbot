use sea_orm::FromQueryResult;

#[derive(Debug, Clone, PartialEq, Eq, FromQueryResult)]
pub struct Model {
    pub id: i64,
    pub chat_id: i64,
    pub user_id: i64,
    pub source: String,
    pub telegraph: bool,
    pub generation: i64,
    pub delivered: i64,
    pub cursor: Option<String>,
}
