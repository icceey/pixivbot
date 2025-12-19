//! E-Hentai/ExHentai API 客户端框架层
//!
//! 这是一个干净的 E-Hentai API 封装，不依赖项目其他代码。
//! 参考 [exloli-next](https://github.com/lolishinshi/exloli-next) 的设计，感谢原作者。

mod client;
mod error;
mod models;

pub use client::{EhClient, EhClientConfig, EhCredentials, EhSource};
pub use models::{Category, GalleryImage, GalleryInfo, GalleryMetadata, GalleryTag, SearchResult};
