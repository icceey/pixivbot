//! Per-user, per-chat settings dialogues.
use crate::db::types::TagEditMode;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use teloxide::prelude::*;
use teloxide::types::MessageId;
use tokio::sync::RwLock;
use tokio::time::Instant;

pub const DIALOGUE_TIMEOUT: Duration = Duration::from_secs(3 * 60);

#[derive(Clone, Debug)]
pub enum SettingsPhase {
    Preparing,
    ChoosingMode,
    ConfirmingClear,
    WaitingForTags {
        mode: TagEditMode,
        after_update_id: u32,
    },
}

#[derive(Clone, Debug)]
pub struct SettingsState {
    pub session_id: u64,
    pub is_sensitive: bool,
    pub settings_message_id: MessageId,
    /// The message and session token jointly bind callbacks to this dialogue.
    pub selection_message_id: Option<MessageId>,
    pub phase: SettingsPhase,
    pub created_at: Instant,
}

impl SettingsState {
    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed() >= DIALOGUE_TIMEOUT
    }
}

pub type SettingsStorage = Arc<RwLock<HashMap<(ChatId, UserId), SettingsState>>>;
