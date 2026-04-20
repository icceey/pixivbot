// Admin related handlers
mod admin;

// Help and Info handlers
mod info;

// Chat settings handlers
mod settings;
pub use settings::{
    handle_settings_callback, handle_settings_cancel, handle_settings_input,
    SETTINGS_CALLBACK_PREFIX,
};

// Subscription related handlers
mod subscription;
pub use subscription::{parse_list_callback_data, ListPaginationAction, LIST_CALLBACK_PREFIX};

// Download handler
mod download;

mod booru_download;

/// Callback data prefix for download button (Pixiv illust).
pub const DOWNLOAD_CALLBACK_PREFIX: &str = "dl:";

/// Callback data prefix for download button (Booru post).
/// Format: `dlb:<site_name>:<post_id>`.
pub const BOORU_DOWNLOAD_CALLBACK_PREFIX: &str = "dlb:";
