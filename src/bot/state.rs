//! Dialogue state management for multi-step interactions.
//!
//! This module provides the state machine for handling interactive settings
//! where users need to provide input across multiple messages.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use teloxide::prelude::*;
use teloxide::types::MessageId;
use tokio::sync::RwLock;

/// Timeout duration for settings dialogue (5 minutes)
pub const DIALOGUE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

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
        /// When this state was created
        created_at: Instant,
    },
    /// Waiting for user to input excluded tags
    WaitingForExcludedTags {
        /// The message ID of the settings panel to update after input
        settings_message_id: MessageId,
        /// When this state was created
        created_at: Instant,
    },
}

impl SettingsState {
    /// Check if this state has expired
    pub fn is_expired(&self) -> bool {
        let created_at = match self {
            SettingsState::WaitingForSensitiveTags { created_at, .. } => created_at,
            SettingsState::WaitingForExcludedTags { created_at, .. } => created_at,
        };
        created_at.elapsed() > DIALOGUE_TIMEOUT
    }

    /// Get the settings message ID
    pub fn settings_message_id(&self) -> MessageId {
        match self {
            SettingsState::WaitingForSensitiveTags {
                settings_message_id,
                ..
            } => *settings_message_id,
            SettingsState::WaitingForExcludedTags {
                settings_message_id,
                ..
            } => *settings_message_id,
        }
    }
}

/// Storage for dialogue states - thread-safe HashMap keyed by (ChatId, UserId)
pub type SettingsStorage = Arc<RwLock<HashMap<(ChatId, UserId), SettingsState>>>;

/// Create a new settings storage instance
pub fn new_settings_storage() -> SettingsStorage {
    Arc::new(RwLock::new(HashMap::new()))
}
