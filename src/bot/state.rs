//! Dialogue state management for multi-step interactions.
//!
//! This module provides the state machine for handling interactive settings
//! where users need to provide input across multiple messages.

use std::collections::HashMap;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::MessageId;
use tokio::sync::RwLock;

/// State for the settings dialogue.
///
/// Each user in a chat has their own independent state, preventing
/// interference between concurrent users editing settings.
#[derive(Clone, Debug)]
pub enum SettingsState {
    /// Waiting for user to input sensitive tags
    WaitingForSensitiveTags {
        /// The message ID of the settings panel to update after input
        settings_message_id: MessageId,
    },
    /// Waiting for user to input excluded tags
    WaitingForExcludedTags {
        /// The message ID of the settings panel to update after input
        settings_message_id: MessageId,
    },
}

/// Storage for dialogue states - thread-safe HashMap keyed by (ChatId, UserId)
pub type SettingsStorage = Arc<RwLock<HashMap<(ChatId, UserId), SettingsState>>>;

/// Create a new settings storage instance
pub fn new_settings_storage() -> SettingsStorage {
    Arc::new(RwLock::new(HashMap::new()))
}
