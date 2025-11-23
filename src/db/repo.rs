use sea_orm::{DatabaseConnection, DbErr};

pub struct Repo {
    db: DatabaseConnection,
}

impl Repo {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    // Placeholder for future DB operations
    pub async fn ping(&self) -> Result<(), DbErr> {
        self.db.ping().await
    }
}
