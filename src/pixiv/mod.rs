pub mod client;
pub mod downloader;
pub mod model;

pub use client::GalleryCursor;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PixivGallery {
    Collection(String),
    Bookmarks(u64),
    Author(u64),
    Tag(String),
    Ranking {
        mode: String,
        date: chrono::NaiveDate,
        limit: usize,
    },
    Pixivision {
        lang: String,
        id: u64,
    },
}
