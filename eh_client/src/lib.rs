pub mod client;
pub mod error;
pub mod models;
pub mod parser;
pub mod telegraph;

pub use client::{EhClient, EhClientBuilder};
pub use error::{Error, Result};
pub use models::{EhCategory, EhCookies, EhGallery, EhGalleryRef};
pub use telegraph::{
    CatboxUploader, CatboxUploaderConfig, ImageUploadConfig, ImageUploadInput, ImageUploadProvider,
    ImageUploader, PixiUploader, S3Uploader, S3UploaderConfig, TelegraphClient,
};
