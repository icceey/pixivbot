pub mod client;
pub mod downloader;
pub mod model;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PixivGallery {
    Collection(String),
    Bookmarks(u64),
    Author(u64),
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
