// Admin related handlers
mod admin;

// Help and Info handlers
mod info;

// Chat settings handlers
mod settings;
pub(crate) use settings::{InMemStorage, SettingsState, SettingsStorage, SETTINGS_CALLBACK_PREFIX};

// Subscription related handlers
mod subscription;
pub use subscription::LIST_CALLBACK_PREFIX;

// Download handler
mod download;

/// Callback data prefix for download button
pub const DOWNLOAD_CALLBACK_PREFIX: &str = "dl:";
