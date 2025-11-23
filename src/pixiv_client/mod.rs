//! Pixiv API 客户端框架层
//! 
//! 这是一个干净的 Pixiv API 封装，不依赖项目其他代码。
//! 参考 [pixivpy](https://github.com/upbit/pixivpy) 的设计和实现，感谢原作者 @upbit。
//! 只包含本项目需要的 API。

mod error;
mod models;
mod auth;
mod client;

pub use error::{Error, Result};
pub use models::{Illust, User, ImageUrls, MetaSinglePage, IllustDetail};
pub use client::PixivClient;
