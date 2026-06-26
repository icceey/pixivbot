pub mod client;
pub mod error;
pub mod models;
pub mod parser;
pub mod telegraph;

pub use client::EhClient;
pub use error::{Error, Result};
pub use models::{EhCategory, EhCookies, EhGallery, EhGalleryRef};
pub use telegraph::TelegraphClient;
