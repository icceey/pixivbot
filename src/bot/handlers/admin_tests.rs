use super::*;
use crate::bot::{Command, UserChatContext};
use crate::db::repo::{tests_helpers::setup_test_db, Repo};
use serde_json::{json, Value};
use std::sync::Arc;
use teloxide::adaptors::throttle::Limits;
use wiremock::matchers::{body_partial_json, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn message(chat: i64, actor: u64) -> Message {
    serde_json::from_value(json!({
        "message_id": 1, "date": 1700000000,
        "chat": {"id": chat, "type": if chat > 0 {"private"} else {"supergroup"}, "title": "Test"},
        "from": {"id": actor, "is_bot": false, "first_name": "Actor"}, "text": "/enablechat"
    }))
    .unwrap()
}

async fn fixture() -> (MockServer, ThrottledBot, BotHandler) {
    use crate::bot::notifier::Notifier;
    use crate::cache::FileCacheManager;
    use crate::config::PixivConfig;
    use crate::pixiv::{client::PixivClient, downloader::Downloader};
    let server = MockServer::start().await;
    let sequence = Arc::new(std::sync::atomic::AtomicI32::new(100));
    Mock::given(path("/botfake_token/SendMessage"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let mut result = message(body["chat_id"].as_i64().unwrap(), 99);
            result.id = teloxide::types::MessageId(
                sequence.fetch_add(10, std::sync::atomic::Ordering::SeqCst),
            );
            ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": result}))
        })
        .mount(&server)
        .await;
    Mock::given(path("/botfake_token/AnswerCallbackQuery"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": true})))
        .mount(&server)
        .await;
    Mock::given(path("/botfake_token/EditMessageText"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": true, "result": message(-100, 99)})),
        )
        .mount(&server)
        .await;
    let bot = Bot::new("fake_token")
        .set_api_url(url::Url::parse(&server.uri()).unwrap())
        .throttle(Limits {
            messages_per_sec_chat: 1000,
            messages_per_min_chat: 60000,
            messages_per_min_channel_or_supergroup: 60000,
            messages_per_sec_overall: 1000,
        });
    let repo = Arc::new(setup_test_db().await.unwrap());
    for (id, role, name) in [
        (1, UserRole::Owner, "owner"),
        (2, UserRole::User, "Alice"),
        (3, UserRole::User, "Bob"),
    ] {
        repo.upsert_user(id, Some(name.into()), role).await.unwrap();
    }
    for id in [-100, -200, 2] {
        repo.upsert_chat(
            id,
            if id > 0 { "private" } else { "group" }.into(),
            None,
            false,
            Default::default(),
        )
        .await
        .unwrap();
    }
    let handler = BotHandler::new(
        repo,
        Arc::new(tokio::sync::RwLock::new(
            PixivClient::new(PixivConfig {
                refresh_token: "fake".into(),
            })
            .unwrap(),
        )),
        Notifier::new(
            bot.clone(),
            Arc::new(Downloader::new(
                reqwest::Client::new(),
                FileCacheManager::new("/tmp/pixivbot-admin-test-cache", 7),
            )),
        ),
        vec![],
        Some(1),
        false,
        pixiv_client::ImageSize::Large,
        3,
        true,
        String::new(),
        String::new(),
        Arc::new(Default::default()),
        None,
        Arc::new(Default::default()),
        false,
    );
    (server, bot, handler)
}

async fn command(handler: &BotHandler, bot: &ThrottledBot, msg: Message, cmd: Command) {
    let ctx = UserChatContext {
        user: handler
            .repo
            .get_user(msg.from.as_ref().unwrap().id.0 as i64)
            .await
            .unwrap()
            .unwrap(),
        chat: handler.repo.get_chat(msg.chat.id.0).await.unwrap().unwrap(),
    };
    handler
        .handle_command(bot.clone(), msg, cmd, ctx)
        .await
        .unwrap();
}

fn member(status: &str, id: u64, name: &str) -> Value {
    json!({"ok": true, "result": {
        "status": status, "user": {"id": id, "is_bot": false, "first_name": "User", "username": name},
        "is_anonymous": false, "can_be_edited": false, "can_manage_chat": true,
        "can_change_info": true, "can_delete_messages": true, "can_manage_video_chats": true,
        "can_invite_users": true, "can_restrict_members": true, "can_promote_members": false
    }})
}

async fn mock_member(server: &MockServer, chat: i64, id: u64, response: Value) {
    Mock::given(path("/botfake_token/GetChatMember"))
        .and(body_partial_json(json!({"chat_id": chat, "user_id": id})))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .expect(1)
        .mount(server)
        .await;
}

async fn role(repo: &Repo, id: i64) -> UserRole {
    repo.get_user(id).await.unwrap().unwrap().role
}

// A local admin must never acquire a persisted role or reuse group A privileges
// in group B/private chat. Exercise the command dispatch and actual DB writes.
#[tokio::test]
async fn telegram_roles_are_scoped_and_queries_fail_closed() {
    for status in ["creator", "administrator", "member"] {
        let (server, bot, handler) = fixture().await;
        mock_member(&server, -100, 2, member(status, 2, "Alice")).await;
        command(
            &handler,
            &bot,
            message(-100, 2),
            Command::EnableChat(String::new()),
        )
        .await;
        assert_eq!(
            handler.repo.get_chat(-100).await.unwrap().unwrap().enabled,
            status != "member"
        );
        assert_eq!(role(&handler.repo, 2).await, UserRole::User);
        command(
            &handler,
            &bot,
            message(-100, 2),
            Command::EnableChat("-200".into()),
        )
        .await;
        command(
            &handler,
            &bot,
            message(2, 2),
            Command::EnableChat(String::new()),
        )
        .await;
        assert!(!handler.repo.get_chat(-200).await.unwrap().unwrap().enabled);
        assert!(!handler.repo.get_chat(2).await.unwrap().unwrap().enabled);
        mock_member(
            &server,
            -200,
            2,
            json!({"ok": false, "error_code": 400, "description": "unavailable"}),
        )
        .await;
        command(
            &handler,
            &bot,
            message(-200, 2),
            Command::EnableChat(String::new()),
        )
        .await;
        assert!(!handler.repo.get_chat(-200).await.unwrap().unwrap().enabled);
        // Owner retains global chat-control behavior, including an explicit target.
        command(
            &handler,
            &bot,
            message(-100, 1),
            Command::EnableChat("-200".into()),
        )
        .await;
        assert!(handler.repo.get_chat(-200).await.unwrap().unwrap().enabled);
    }
}

// Replies to forwarded messages must affect A, not the embedded B identity.
#[tokio::test]
async fn owner_role_commands_resolve_ids_and_actual_reply_sender() {
    let (_server, bot, handler) = fixture().await;
    command(
        &handler,
        &bot,
        message(-100, 1),
        Command::SetAdmin("2".into()),
    )
    .await;
    assert_eq!(role(&handler.repo, 2).await, UserRole::Admin);
    let mut reply = serde_json::to_value(message(-100, 2)).unwrap();
    reply["forward_origin"] = json!({"type": "user", "date": 1600000000, "sender_user": {"id": 3, "is_bot": false, "first_name": "Origin"}});
    let mut msg = serde_json::to_value(message(-100, 1)).unwrap();
    msg["reply_to_message"] = reply.clone();
    command(
        &handler,
        &bot,
        serde_json::from_value(msg.clone()).unwrap(),
        Command::UnsetAdmin(String::new()),
    )
    .await;
    assert_eq!(role(&handler.repo, 2).await, UserRole::User);
    command(
        &handler,
        &bot,
        serde_json::from_value(msg.clone()).unwrap(),
        Command::SetAdmin(String::new()),
    )
    .await;
    assert_eq!(role(&handler.repo, 2).await, UserRole::Admin);
    assert_eq!(role(&handler.repo, 3).await, UserRole::User);
    reply.as_object_mut().unwrap().remove("forward_origin");
    msg["reply_to_message"] = reply;
    command(
        &handler,
        &bot,
        serde_json::from_value(msg).unwrap(),
        Command::UnsetAdmin(String::new()),
    )
    .await;
    assert_eq!(role(&handler.repo, 2).await, UserRole::User);
}

// Guard both operations and the Owner target, including spoofed sender_chat.from.
#[tokio::test]
async fn global_roles_reject_non_owner_and_anonymous_identities() {
    let (_server, bot, handler) = fixture().await;
    handler
        .repo
        .set_user_role(3, UserRole::Admin)
        .await
        .unwrap();
    for actor in [2, 3] {
        command(
            &handler,
            &bot,
            message(-100, actor),
            Command::SetAdmin("2".into()),
        )
        .await;
        command(
            &handler,
            &bot,
            message(-100, actor),
            Command::UnsetAdmin("3".into()),
        )
        .await;
    }
    assert_eq!(role(&handler.repo, 2).await, UserRole::User);
    assert_eq!(role(&handler.repo, 3).await, UserRole::Admin);
    for cmd in [
        Command::SetAdmin("1".into()),
        Command::UnsetAdmin("1".into()),
    ] {
        command(&handler, &bot, message(-100, 1), cmd).await;
        assert_eq!(role(&handler.repo, 1).await, UserRole::Owner);
    }
    let mut anonymous = serde_json::to_value(message(-100, 1)).unwrap();
    anonymous["sender_chat"] = json!({"id": -100, "type": "supergroup", "title": "Group"});
    command(
        &handler,
        &bot,
        serde_json::from_value(anonymous.clone()).unwrap(),
        Command::SetAdmin("2".into()),
    )
    .await;
    command(
        &handler,
        &bot,
        serde_json::from_value(anonymous).unwrap(),
        Command::UnsetAdmin("3".into()),
    )
    .await;
    for kind in ["sender_chat", "bot", "missing"] {
        let mut msg = serde_json::to_value(message(-100, 1)).unwrap();
        let mut reply = serde_json::to_value(message(-100, 2)).unwrap();
        match kind {
            "sender_chat" => {
                reply["sender_chat"] = json!({"id": -100, "type": "supergroup", "title": "Group"})
            }
            "bot" => reply["from"]["is_bot"] = json!(true),
            _ => {
                reply.as_object_mut().unwrap().remove("from");
            }
        }
        msg["reply_to_message"] = reply;
        command(
            &handler,
            &bot,
            serde_json::from_value(msg).unwrap(),
            Command::SetAdmin(String::new()),
        )
        .await;
    }
    assert_eq!(role(&handler.repo, 2).await, UserRole::User);
    assert_eq!(role(&handler.repo, 3).await, UserRole::Admin);
}

// A cached username reassigned to somebody else must not grant the old holder a role.
#[tokio::test]
async fn username_candidates_require_fresh_unambiguous_telegram_identity() {
    let (server, bot, handler) = fixture().await;
    mock_member(&server, -100, 2, member("member", 2, "aLiCe")).await;
    command(
        &handler,
        &bot,
        message(-100, 1),
        Command::SetAdmin("@ALICE".into()),
    )
    .await;
    assert_eq!(role(&handler.repo, 2).await, UserRole::Admin);
    server.reset().await;
    mock_member(&server, -100, 2, member("member", 2, "renamed")).await;
    assert!(
        resolve_admin_target(&bot, &handler.repo, &message(-100, 1), "@alice")
            .await
            .is_err()
    );
    assert!(
        resolve_admin_target(&bot, &handler.repo, &message(-100, 1), "@unknown")
            .await
            .is_err()
    );
    server.reset().await;
    mock_member(
        &server,
        -100,
        2,
        json!({"ok": false, "error_code": 400, "description": "not found"}),
    )
    .await;
    assert!(
        resolve_admin_target(&bot, &handler.repo, &message(-100, 1), "@alice")
            .await
            .is_err()
    );
    server.reset().await;
    handler
        .repo
        .upsert_user(3, Some("alice".into()), UserRole::User)
        .await
        .unwrap();
    for id in [2, 3] {
        mock_member(&server, -100, id, member("member", id, "alice")).await;
    }
    assert!(
        resolve_admin_target(&bot, &handler.repo, &message(-100, 1), "@alice")
            .await
            .is_err()
    );
    server.reset().await;
    handler
        .repo
        .upsert_user(3, Some("bob".into()), UserRole::User)
        .await
        .unwrap();
    Mock::given(path("/botfake_token/GetChat"))
        .and(body_partial_json(json!({"chat_id": 2})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": {"id": 2, "type": "private", "first_name": "Alice", "username": "ALICE", "accent_color_id": 0, "max_reaction_count": 1, "accepted_gift_types": {"unlimited_gifts": false, "limited_gifts": false, "unique_gifts": false, "premium_subscription": false}}})))
        .expect(1).mount(&server).await;
    assert!(matches!(
        resolve_admin_target(&bot, &handler.repo, &message(2, 1), "@alice")
            .await
            .unwrap(),
        AdminTarget::Known(2)
    ));
}

// Revoking Telegram admin status while a settings dialogue is open must prevent a write.
#[tokio::test]
async fn settings_submission_rechecks_revoked_group_permission() {
    use crate::bot::state::{new_settings_storage, SettingsState};
    let (server, bot, handler) = fixture().await;
    let storage = new_settings_storage();
    storage.write().await.insert(
        (ChatId(-100), UserId(2)),
        SettingsState {
            session_id: 1,
            is_sensitive: false,
            settings_message_id: teloxide::types::MessageId(2),
            selection_message_id: Some(teloxide::types::MessageId(-1)),
            phase: crate::bot::state::SettingsPhase::WaitingForTags {
                mode: crate::db::types::TagEditMode::Replace,
                after_update_id: 0,
            },
            created_at: tokio::time::Instant::now(),
        },
    );
    mock_member(&server, -100, 2, member("member", 2, "Alice")).await;
    let before = handler
        .repo
        .get_chat(-100)
        .await
        .unwrap()
        .unwrap()
        .excluded_tags;
    assert!(crate::bot::handlers::handle_settings_input(
        bot,
        serde_json::from_value({
            let mut value = serde_json::to_value(message(-100, 2)).unwrap();
            value["text"] = json!("tag");
            value
        })
        .unwrap(),
        handler.clone(),
        storage.clone(),
        1,
    )
    .await
    .unwrap());
    assert_eq!(
        handler
            .repo
            .get_chat(-100)
            .await
            .unwrap()
            .unwrap()
            .excluded_tags,
        before
    );
    assert!(storage.read().await.is_empty());
}

// Settings callback messages are sent by the bot; authorization must use the
// clicking user and the message's own chat, never reuse a different group's role.
#[tokio::test]
async fn settings_callback_uses_clicker_and_current_group() {
    let (server, bot, handler) = fixture().await;
    Mock::given(path("/botfake_token/AnswerCallbackQuery"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": true})))
        .mount(&server)
        .await;
    Mock::given(path("/botfake_token/EditMessageText"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": true, "result": message(-100, 99)})),
        )
        .mount(&server)
        .await;
    let storage = crate::bot::state::new_settings_storage();
    for (chat, status) in [(-100, "administrator"), (-200, "member")] {
        mock_member(&server, chat, 2, member(status, 2, "Alice")).await;
        let mut panel = serde_json::to_value(message(chat, 99)).unwrap();
        panel["from"]["is_bot"] = json!(true);
        let q = serde_json::from_value(json!({
            "id": "callback", "from": {"id": 2, "is_bot": false, "first_name": "Alice"},
            "message": panel, "chat_instance": "instance", "data": "settings:blur:toggle"
        }))
        .unwrap();
        let before = handler
            .repo
            .get_chat(chat)
            .await
            .unwrap()
            .unwrap()
            .blur_sensitive_tags;
        crate::bot::handlers::handle_settings_callback(
            bot.clone(),
            q,
            "settings:blur:toggle".into(),
            handler.clone(),
            storage.clone(),
            1,
        )
        .await
        .unwrap();
        let after = handler
            .repo
            .get_chat(chat)
            .await
            .unwrap()
            .unwrap()
            .blur_sensitive_tags;
        assert_eq!(after != before, chat == -100);
    }
    assert_eq!(role(&handler.repo, 2).await, UserRole::User);
}

async fn settings_click(
    bot: &ThrottledBot,
    handler: &BotHandler,
    storage: &crate::bot::state::SettingsStorage,
    chat: i64,
    actor: u64,
    message_id: teloxide::types::MessageId,
    action: &str,
) {
    let parts: Vec<_> = action.split(':').collect();
    let action = if parts.len() == 3 && matches!(parts[1], "mode" | "cancel" | "confirm") {
        let token = storage
            .read()
            .await
            .values()
            .find(|state| state.selection_message_id == Some(message_id))
            .map(|state| state.session_id)
            .unwrap_or(0);
        format!("settings:{}:{}:{}", parts[1], token, parts[2])
    } else {
        action.to_string()
    };
    let mut panel = message(chat, 99);
    panel.id = message_id;
    panel.from.as_mut().unwrap().is_bot = true;
    let q = serde_json::from_value(json!({
        "id": "settings-callback", "from": {"id": actor, "is_bot": false, "first_name": "Editor"},
        "message": panel, "chat_instance": "instance", "data": action
    }))
    .unwrap();
    crate::bot::handlers::handle_settings_callback(
        bot.clone(),
        q,
        action,
        handler.clone(),
        storage.clone(),
        message_id.0 as u32 + 100000,
    )
    .await
    .unwrap();
}

async fn begin_tag_edit(
    bot: &ThrottledBot,
    handler: &BotHandler,
    storage: &crate::bot::state::SettingsStorage,
    sensitive: bool,
) -> teloxide::types::MessageId {
    settings_click(
        bot,
        handler,
        storage,
        -100,
        1,
        teloxide::types::MessageId(5),
        if sensitive {
            "settings:edit:sensitive"
        } else {
            "settings:edit:exclude"
        },
    )
    .await;
    storage.read().await[&(ChatId(-100), UserId(1))]
        .selection_message_id
        .unwrap()
}

async fn tag_input(
    bot: &ThrottledBot,
    handler: &BotHandler,
    storage: &crate::bot::state::SettingsStorage,
    text: &str,
) {
    use crate::bot::state::SettingsPhase;
    let after_update_id = match storage.read().await[&(ChatId(-100), UserId(1))].phase {
        SettingsPhase::WaitingForTags {
            after_update_id, ..
        } => after_update_id,
        _ => panic!("mode was not selected"),
    };
    let mut msg = serde_json::to_value(message(-100, 1)).unwrap();
    msg["message_id"] = json!(after_update_id + 1);
    msg["text"] = json!(text);
    assert!(crate::bot::handlers::handle_settings_input(
        bot.clone(),
        serde_json::from_value(msg).unwrap(),
        handler.clone(),
        storage.clone(),
        after_update_id + 1,
    )
    .await
    .unwrap());
}

// Two editors can change the list while a user is choosing a mode. Submission
// must apply to current data, not resurrect a list captured at dialogue start.
#[tokio::test]
async fn tag_modes_apply_to_current_lists_and_empty_input_never_clears() {
    use crate::db::types::{TagEditMode, Tags};
    let (_server, bot, handler) = fixture().await;
    let storage = crate::bot::state::new_settings_storage();
    for sensitive in [true, false] {
        handler
            .repo
            .edit_chat_tags(
                -100,
                sensitive,
                TagEditMode::Replace,
                Tags::from(vec!["old".into()]),
            )
            .await
            .unwrap();
        let selector = begin_tag_edit(&bot, &handler, &storage, sensitive).await;
        // An independent edit after the panel opens must survive the append.
        handler
            .repo
            .edit_chat_tags(
                -100,
                sensitive,
                TagEditMode::Append,
                Tags::from(vec!["concurrent".into()]),
            )
            .await
            .unwrap();
        settings_click(
            &bot,
            &handler,
            &storage,
            -100,
            1,
            selector,
            "settings:mode:append",
        )
        .await;
        tag_input(&bot, &handler, &storage, " old， clear, CLEAR, clear ,, ").await;
        let current = handler.repo.get_chat(-100).await.unwrap().unwrap();
        let tags = if sensitive {
            current.sensitive_tags
        } else {
            current.excluded_tags
        };
        assert_eq!(tags.0, vec!["old", "concurrent", "clear", "CLEAR"]);
        let selector = begin_tag_edit(&bot, &handler, &storage, sensitive).await;
        settings_click(
            &bot,
            &handler,
            &storage,
            -100,
            1,
            selector,
            "settings:mode:remove",
        )
        .await;
        tag_input(&bot, &handler, &storage, "clear,missing").await;
        let current = handler.repo.get_chat(-100).await.unwrap().unwrap();
        let tags = if sensitive {
            current.sensitive_tags
        } else {
            current.excluded_tags
        };
        assert_eq!(tags.0, vec!["old", "concurrent", "CLEAR"]);
        let selector = begin_tag_edit(&bot, &handler, &storage, sensitive).await;
        settings_click(
            &bot,
            &handler,
            &storage,
            -100,
            1,
            selector,
            "settings:mode:replace",
        )
        .await;
        tag_input(&bot, &handler, &storage, " , ， ").await;
        assert!(storage
            .read()
            .await
            .contains_key(&(ChatId(-100), UserId(1))));
        tag_input(&bot, &handler, &storage, "CLEAR").await;
        let current = handler.repo.get_chat(-100).await.unwrap().unwrap();
        let tags = if sensitive {
            current.sensitive_tags
        } else {
            current.excluded_tags
        };
        assert_eq!(tags.0, vec!["CLEAR"]);
    }
}

// Telegram callback data is not an authorization token. Even another global
// admin cannot reuse somebody else's mode buttons or a superseded selection.
#[tokio::test]
async fn tag_mode_buttons_bind_actor_chat_and_session_and_select_once() {
    use crate::bot::state::SettingsPhase;
    let (_server, bot, handler) = fixture().await;
    handler
        .repo
        .set_user_role(2, UserRole::Admin)
        .await
        .unwrap();
    let storage = crate::bot::state::new_settings_storage();
    let old = begin_tag_edit(&bot, &handler, &storage, false).await;
    let early_input: Message = serde_json::from_value(json!({
        "message_id": 50, "date": 1700000000,
        "chat": {"id": -100, "type": "supergroup", "title": "Test"},
        "from": {"id": 1, "is_bot": false, "first_name": "Owner"}, "text": "early tags"
    }))
    .unwrap();
    assert!(!crate::bot::handlers::handle_settings_input(
        bot.clone(),
        early_input.clone(),
        handler.clone(),
        storage.clone(),
        0,
    )
    .await
    .unwrap());
    settings_click(
        &bot,
        &handler,
        &storage,
        -100,
        2,
        old,
        "settings:mode:replace",
    )
    .await;
    settings_click(
        &bot,
        &handler,
        &storage,
        -200,
        1,
        old,
        "settings:mode:replace",
    )
    .await;
    assert!(matches!(
        storage.read().await[&(ChatId(-100), UserId(1))].phase,
        SettingsPhase::ChoosingMode
    ));
    let current = begin_tag_edit(&bot, &handler, &storage, false).await;
    settings_click(
        &bot,
        &handler,
        &storage,
        -100,
        1,
        old,
        "settings:mode:replace",
    )
    .await;
    assert!(matches!(
        storage.read().await[&(ChatId(-100), UserId(1))].phase,
        SettingsPhase::ChoosingMode
    ));
    settings_click(
        &bot,
        &handler,
        &storage,
        -100,
        1,
        current,
        "settings:mode:append",
    )
    .await;
    let deadline_start = storage.read().await[&(ChatId(-100), UserId(1))].created_at;
    settings_click(
        &bot,
        &handler,
        &storage,
        -100,
        1,
        current,
        "settings:mode:replace",
    )
    .await;
    let state = storage.read().await[&(ChatId(-100), UserId(1))].clone();
    assert_eq!(state.created_at, deadline_start);
    assert!(matches!(
        state.phase,
        SettingsPhase::WaitingForTags {
            mode: crate::db::types::TagEditMode::Append,
            ..
        }
    ));
    // Text sent before the mode prompt cannot be consumed by a delayed handler.
    assert!(!crate::bot::handlers::handle_settings_input(
        bot.clone(),
        early_input.clone(),
        handler.clone(),
        storage.clone(),
        0,
    )
    .await
    .unwrap());
    settings_click(
        &bot,
        &handler,
        &storage,
        -100,
        1,
        current,
        "settings:cancel:input",
    )
    .await;
    settings_click(
        &bot,
        &handler,
        &storage,
        -100,
        1,
        current,
        "settings:mode:replace",
    )
    .await;
    assert!(storage.read().await.is_empty());
    let selector = begin_tag_edit(&bot, &handler, &storage, false).await;
    let msg = message(-100, 1);
    let ctx = UserChatContext {
        user: handler.repo.get_user(1).await.unwrap().unwrap(),
        chat: handler.repo.get_chat(-100).await.unwrap().unwrap(),
    };
    crate::bot::handle_command(
        bot.clone(),
        msg,
        Command::Settings,
        handler.clone(),
        ctx,
        storage.clone(),
    )
    .await
    .unwrap();
    settings_click(
        &bot,
        &handler,
        &storage,
        -100,
        1,
        selector,
        "settings:mode:append",
    )
    .await;
    assert!(storage.read().await.is_empty());
}

async fn advance_settings_time(seconds: u64) {
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(seconds)).await;
    tokio::time::resume();
}

#[tokio::test]
async fn tag_dialogue_enforces_both_deadlines_and_renews_only_on_selection() {
    let (_server, bot, handler) = fixture().await;
    let storage = crate::bot::state::new_settings_storage();
    let selector = begin_tag_edit(&bot, &handler, &storage, false).await;
    advance_settings_time(180).await;
    settings_click(
        &bot,
        &handler,
        &storage,
        -100,
        1,
        selector,
        "settings:mode:append",
    )
    .await;
    assert!(storage.read().await.is_empty());
    let selector = begin_tag_edit(&bot, &handler, &storage, false).await;
    advance_settings_time(179).await;
    settings_click(
        &bot,
        &handler,
        &storage,
        -100,
        1,
        selector,
        "settings:mode:append",
    )
    .await;
    advance_settings_time(179).await;
    tag_input(&bot, &handler, &storage, "accepted").await;
    assert_eq!(
        handler
            .repo
            .get_chat(-100)
            .await
            .unwrap()
            .unwrap()
            .excluded_tags
            .0,
        vec!["accepted"]
    );
    let selector = begin_tag_edit(&bot, &handler, &storage, false).await;
    settings_click(
        &bot,
        &handler,
        &storage,
        -100,
        1,
        selector,
        "settings:mode:replace",
    )
    .await;
    advance_settings_time(180).await;
    tag_input(&bot, &handler, &storage, "expired").await;
    assert_eq!(
        handler
            .repo
            .get_chat(-100)
            .await
            .unwrap()
            .unwrap()
            .excluded_tags
            .0,
        vec!["accepted"]
    );
    assert!(storage.read().await.is_empty());
}

#[tokio::test]
async fn simultaneous_tag_appends_do_not_lose_committed_tags() {
    use crate::db::types::{TagEditMode, Tags};
    let repo = setup_test_db().await.unwrap();
    repo.upsert_chat(-100, "group".into(), None, false, Default::default())
        .await
        .unwrap();
    let edits = ["one", "two", "three", "four"].map(|tag| {
        repo.edit_chat_tags(
            -100,
            false,
            TagEditMode::Append,
            Tags::from(vec![tag.into()]),
        )
    });
    let [a, b, c, d] = edits;
    let results = tokio::join!(a, b, c, d);
    for result in [results.0, results.1, results.2, results.3] {
        result.unwrap();
    }
    let mut tags = repo.get_chat(-100).await.unwrap().unwrap().excluded_tags.0;
    tags.sort();
    assert_eq!(tags, vec!["four", "one", "three", "two"]);
}

async fn dispatch_settings_text(
    bot: &ThrottledBot,
    handler: &BotHandler,
    storage: &crate::bot::state::SettingsStorage,
    text: &str,
) {
    let mut msg = serde_json::to_value(message(-100, 1)).unwrap();
    msg["message_id"] = json!(9000);
    msg["text"] = json!(text);
    let update = Update {
        id: teloxide::types::UpdateId(1000000),
        kind: teloxide::types::UpdateKind::Message(serde_json::from_value(msg).unwrap()),
    };
    let me: teloxide::types::Me = serde_json::from_value(json!({
        "id": 99, "is_bot": true, "first_name": "Bot", "username": "testbot",
        "can_join_groups": true, "can_read_all_group_messages": true, "supports_inline_queries": false, "has_main_web_app": false
    })).unwrap();
    let result = crate::bot::build_handler_tree()
        .dispatch(teloxide::dptree::deps![
            bot.clone(),
            handler.clone(),
            handler.repo.clone(),
            storage.clone(),
            me,
            update
        ])
        .await;
    assert!(
        matches!(&result, std::ops::ControlFlow::Break(Ok(()))),
        "{text}: {result:?}"
    );
}

// The real routing tree must intercept bare controls before mention gating can
// turn them into tag data in groups requiring /command@bot.
#[tokio::test]
async fn active_settings_controls_bypass_mention_without_becoming_tags() {
    let (_server, bot, handler) = fixture().await;
    handler.repo.set_chat_enabled(-100, true).await.unwrap();
    let storage = crate::bot::state::new_settings_storage();
    for control in [
        "/cancel",
        "/settings",
        "/cancel@testbot",
        "/settings@testbot",
    ] {
        let selector = begin_tag_edit(&bot, &handler, &storage, false).await;
        settings_click(
            &bot,
            &handler,
            &storage,
            -100,
            1,
            selector,
            "settings:mode:replace",
        )
        .await;
        dispatch_settings_text(&bot, &handler, &storage, control).await;
        assert!(storage.read().await.is_empty());
        assert!(handler
            .repo
            .get_chat(-100)
            .await
            .unwrap()
            .unwrap()
            .excluded_tags
            .is_empty());
        settings_click(
            &bot,
            &handler,
            &storage,
            -100,
            1,
            selector,
            "settings:mode:append",
        )
        .await;
        assert!(storage.read().await.is_empty());
    }
}

// Synchronize on the actual HTTP request. Unrelated operations must finish
// before the deliberately delayed response, without a global dialogue lock.
#[tokio::test]
async fn delayed_mode_prompt_does_not_block_other_chats_or_resurrect_cancelled_session() {
    use wiremock::matchers::body_string_contains;
    let (server, bot, handler) = fixture().await;
    let storage = crate::bot::state::new_settings_storage();
    let received = Arc::new(tokio::sync::Notify::new());
    let signal = received.clone();
    Mock::given(path("/botfake_token/SendMessage"))
        .and(body_partial_json(json!({"chat_id": -100})))
        .and(body_string_contains("settings:mode:"))
        .respond_with(move |_: &wiremock::Request| {
            signal.notify_one();
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(5))
                .set_body_json(json!({"ok": true, "result": message(-100, 99)}))
        })
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    let pending = {
        let bot = bot.clone();
        let handler = handler.clone();
        let storage = storage.clone();
        tokio::spawn(async move {
            settings_click(
                &bot,
                &handler,
                &storage,
                -100,
                1,
                teloxide::types::MessageId(5),
                "settings:edit:exclude",
            )
            .await;
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), received.notified())
        .await
        .expect("expected delayed HTTP request");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        settings_click(
            &bot,
            &handler,
            &storage,
            -200,
            1,
            teloxide::types::MessageId(5),
            "settings:edit:exclude",
        )
        .await;
        crate::bot::handlers::handle_settings_cancel(
            bot.clone(),
            message(-100, 1),
            storage.clone(),
        )
        .await
        .unwrap();
    })
    .await
    .expect("network I/O held the global dialogue lock");
    pending.await.unwrap();
    assert!(!storage
        .read()
        .await
        .contains_key(&(ChatId(-100), UserId(1))));
    assert!(storage
        .read()
        .await
        .contains_key(&(ChatId(-200), UserId(1))));
}

fn reply_command(actor: u64, target: u64) -> Message {
    let mut reply = serde_json::to_value(message(-100, target)).unwrap();
    reply["from"]["username"] = json!("old_reply_name");
    reply["forward_origin"] = json!({"type": "user", "date": 1600000000,
        "sender_user": {"id": 500, "is_bot": false, "first_name": "Origin"}});
    let mut msg = serde_json::to_value(message(-100, actor)).unwrap();
    msg["reply_to_message"] = reply;
    serde_json::from_value(msg).unwrap()
}

#[tokio::test]
async fn reply_promotion_registers_only_authorized_actual_sender_and_preserves_owner() {
    let (_server, bot, handler) = fixture().await;
    let msg = reply_command(1, 400);
    command(
        &handler,
        &bot,
        msg.clone(),
        Command::SetAdmin(String::new()),
    )
    .await;
    let new_user = handler.repo.get_user(400).await.unwrap().unwrap();
    assert_eq!(new_user.role, UserRole::Admin);
    assert_eq!(new_user.username.as_deref(), Some("old_reply_name"));
    assert!(handler.repo.get_user(500).await.unwrap().is_none());
    // An old reply cannot replace newer stored metadata or initialize Owner.
    handler
        .repo
        .upsert_user(400, Some("current_name".into()), UserRole::User)
        .await
        .unwrap();
    command(&handler, &bot, msg, Command::SetAdmin(String::new())).await;
    assert_eq!(
        handler
            .repo
            .get_user(400)
            .await
            .unwrap()
            .unwrap()
            .username
            .as_deref(),
        Some("current_name")
    );
    command(
        &handler,
        &bot,
        reply_command(1, 1),
        Command::SetAdmin(String::new()),
    )
    .await;
    assert_eq!(role(&handler.repo, 1).await, UserRole::Owner);
    assert_eq!(
        handler
            .repo
            .get_user(1)
            .await
            .unwrap()
            .unwrap()
            .username
            .as_deref(),
        Some("owner")
    );
    command(
        &handler,
        &bot,
        reply_command(2, 401),
        Command::SetAdmin(String::new()),
    )
    .await;
    command(
        &handler,
        &bot,
        reply_command(1, 402),
        Command::UnsetAdmin(String::new()),
    )
    .await;
    command(
        &handler,
        &bot,
        message(-100, 1),
        Command::SetAdmin("403".into()),
    )
    .await;
    command(
        &handler,
        &bot,
        message(-100, 1),
        Command::SetAdmin("@never_seen".into()),
    )
    .await;
    for id in [401, 402, 403] {
        assert!(handler.repo.get_user(id).await.unwrap().is_none());
    }
    for kind in ["sender_chat", "bot", "missing"] {
        let mut msg = serde_json::to_value(reply_command(1, 404)).unwrap();
        match kind {
            "sender_chat" => {
                msg["reply_to_message"]["sender_chat"] =
                    json!({"id": -100, "type": "supergroup", "title": "Group"})
            }
            "bot" => msg["reply_to_message"]["from"]["is_bot"] = json!(true),
            _ => {
                msg["reply_to_message"]
                    .as_object_mut()
                    .unwrap()
                    .remove("from");
            }
        }
        command(
            &handler,
            &bot,
            serde_json::from_value(msg).unwrap(),
            Command::SetAdmin(String::new()),
        )
        .await;
    }
    assert!(handler.repo.get_user(404).await.unwrap().is_none());
}

#[tokio::test]
async fn concurrent_reply_registration_never_assigns_owner_or_overwrites_metadata() {
    let (_server, bot, handler) = fixture().await;
    let msg = reply_command(1, 400);
    let results = tokio::join!(
        handler.handle_set_admin(bot.clone(), &msg, String::new(), true),
        handler.handle_set_admin(bot.clone(), &msg, String::new(), true),
    );
    results.0.unwrap();
    results.1.unwrap();
    assert_eq!(role(&handler.repo, 400).await, UserRole::Admin);
    assert_eq!(role(&handler.repo, 1).await, UserRole::Owner);
    let repo = setup_test_db().await.unwrap();
    // No configured/existing Owner: explicit reply registration still cannot
    // take the first-contact auto-owner initialization branch.
    repo.set_admin_from_reply(600, Some("first".into()))
        .await
        .unwrap();
    assert_eq!(role(&repo, 600).await, UserRole::Admin);
    assert_eq!(repo.get_admin_users().await.unwrap().len(), 1);
}

async fn last_settings_prompt(server: &MockServer) -> Value {
    let requests = server.received_requests().await.unwrap();
    let request = requests
        .iter()
        .rev()
        .find(|r| {
            matches!(
                r.url.path(),
                "/botfake_token/SendMessage" | "/botfake_token/EditMessageText"
            )
        })
        .unwrap();
    serde_json::from_slice(&request.body).unwrap()
}

// Display is not the write snapshot. Both prompts read current tags, and UTF-16
// truncation/plain text prevent Telegram failures without truncating stored data.
#[tokio::test]
async fn tag_prompts_refresh_current_values_and_bound_display_without_data_loss() {
    use crate::db::types::{TagEditMode, Tags};
    let (server, bot, handler) = fixture().await;
    let storage = crate::bot::state::new_settings_storage();
    let selector = begin_tag_edit(&bot, &handler, &storage, false).await;
    assert!(last_settings_prompt(&server).await["text"]
        .as_str()
        .unwrap()
        .contains("暂无标签"));
    let special = "[*原有*]`tag`\\_".to_string();
    handler
        .repo
        .edit_chat_tags(
            -100,
            false,
            TagEditMode::Replace,
            Tags::from(vec![special.clone()]),
        )
        .await
        .unwrap();
    settings_click(
        &bot,
        &handler,
        &storage,
        -100,
        1,
        selector,
        "settings:mode:append",
    )
    .await;
    let prompt = last_settings_prompt(&server).await;
    assert_eq!(prompt["message_id"], selector.0);
    assert!(prompt["text"].as_str().unwrap().contains(&special));
    assert!(prompt.get("parse_mode").is_none()); // Plain text cannot interpret tag markup.
    handler
        .repo
        .edit_chat_tags(
            -100,
            false,
            TagEditMode::Append,
            Tags::from(vec!["external".into()]),
        )
        .await
        .unwrap();
    tag_input(&bot, &handler, &storage, "new").await;
    assert_eq!(
        handler
            .repo
            .get_chat(-100)
            .await
            .unwrap()
            .unwrap()
            .excluded_tags
            .0,
        vec![special.clone(), "external".into(), "new".into()]
    );
    let long = "😀".repeat(3000);
    let full = vec![special, long, "tail-must-survive".into()];
    handler
        .repo
        .edit_chat_tags(-100, false, TagEditMode::Replace, Tags::from(full.clone()))
        .await
        .unwrap();
    let selector = begin_tag_edit(&bot, &handler, &storage, false).await;
    assert!(last_settings_prompt(&server).await["text"]
        .as_str()
        .unwrap()
        .contains("仅展示部分"));
    settings_click(
        &bot,
        &handler,
        &storage,
        -100,
        1,
        selector,
        "settings:mode:remove",
    )
    .await;
    assert!(last_settings_prompt(&server).await["text"]
        .as_str()
        .unwrap()
        .contains("仅展示部分"));
    tag_input(&bot, &handler, &storage, "missing").await;
    assert_eq!(
        handler
            .repo
            .get_chat(-100)
            .await
            .unwrap()
            .unwrap()
            .excluded_tags
            .0,
        full
    );
    for request in server.received_requests().await.unwrap() {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        if let Some(text) = body.get("text").and_then(Value::as_str) {
            assert!(text.encode_utf16().count() <= 4096);
            if request.url.path() == "/botfake_token/EditMessageText"
                && body["parse_mode"] == "MarkdownV2"
            {
                assert!(text.contains("\\[\\*原有\\*\\]\\`tag\\`\\\\\\_"));
            }
        }
    }
}

// Replaying a clear confirmation must not erase tags added after the first
// confirmation, and cancelled/expired dialogs must never authorize writes.
#[tokio::test]
async fn clear_confirmation_is_bound_expiring_and_consumed_once() {
    use crate::bot::state::SettingsPhase;
    use crate::db::types::{TagEditMode, Tags};
    let (_server, bot, handler) = fixture().await;
    let storage = crate::bot::state::new_settings_storage();
    handler
        .repo
        .set_user_role(2, UserRole::Admin)
        .await
        .unwrap();
    for cancelled in [true, false] {
        handler
            .repo
            .edit_chat_tags(
                -100,
                false,
                TagEditMode::Replace,
                Tags::from(vec!["keep".into()]),
            )
            .await
            .unwrap();
        let selector = begin_tag_edit(&bot, &handler, &storage, false).await;
        let token = storage.read().await[&(ChatId(-100), UserId(1))].session_id;
        let confirm = format!("settings:confirm:{token}:clear");
        settings_click(&bot, &handler, &storage, -100, 1, selector, &confirm).await;
        assert!(matches!(
            storage.read().await[&(ChatId(-100), UserId(1))].phase,
            SettingsPhase::ChoosingMode
        ));
        advance_settings_time(179).await;
        settings_click(
            &bot,
            &handler,
            &storage,
            -100,
            1,
            selector,
            "settings:mode:clear",
        )
        .await;
        let started = storage.read().await[&(ChatId(-100), UserId(1))].created_at;
        for (chat, actor, id, action) in [
            (-100, 2, selector, confirm.clone()),
            (-200, 1, selector, confirm.clone()),
            (
                -100,
                1,
                teloxide::types::MessageId(selector.0 + 1),
                confirm.clone(),
            ),
            (
                -100,
                1,
                selector,
                format!("settings:confirm:{}:clear", token.wrapping_add(1)),
            ),
            (-100, 1, selector, format!("settings:mode:{token}:replace")),
        ] {
            settings_click(&bot, &handler, &storage, chat, actor, id, &action).await;
        }
        assert_eq!(
            storage.read().await[&(ChatId(-100), UserId(1))].created_at,
            started
        );
        assert_eq!(
            handler
                .repo
                .get_chat(-100)
                .await
                .unwrap()
                .unwrap()
                .excluded_tags
                .0,
            ["keep"]
        );
        if cancelled {
            settings_click(
                &bot,
                &handler,
                &storage,
                -100,
                1,
                selector,
                "settings:cancel:confirm",
            )
            .await;
        } else {
            advance_settings_time(180).await;
        }
        settings_click(&bot, &handler, &storage, -100, 1, selector, &confirm).await;
        assert!(storage.read().await.is_empty());
        assert_eq!(
            handler
                .repo
                .get_chat(-100)
                .await
                .unwrap()
                .unwrap()
                .excluded_tags
                .0,
            ["keep"]
        );
    }
    let selector = begin_tag_edit(&bot, &handler, &storage, false).await;
    let token = storage.read().await[&(ChatId(-100), UserId(1))].session_id;
    let confirm = format!("settings:confirm:{token}:clear");
    settings_click(
        &bot,
        &handler,
        &storage,
        -100,
        1,
        selector,
        "settings:mode:clear",
    )
    .await;
    advance_settings_time(179).await;
    settings_click(&bot, &handler, &storage, -100, 1, selector, &confirm).await;
    assert!(handler
        .repo
        .get_chat(-100)
        .await
        .unwrap()
        .unwrap()
        .excluded_tags
        .is_empty());
    handler
        .repo
        .edit_chat_tags(
            -100,
            false,
            TagEditMode::Append,
            Tags::from(vec!["new".into()]),
        )
        .await
        .unwrap();
    settings_click(&bot, &handler, &storage, -100, 1, selector, &confirm).await;
    assert_eq!(
        handler
            .repo
            .get_chat(-100)
            .await
            .unwrap()
            .unwrap()
            .excluded_tags
            .0,
        ["new"]
    );
    // A role revoked after opening confirmation must fail closed on API error.
    settings_click(
        &bot,
        &handler,
        &storage,
        -100,
        2,
        teloxide::types::MessageId(5),
        "settings:edit:exclude",
    )
    .await;
    let selector = storage.read().await[&(ChatId(-100), UserId(2))]
        .selection_message_id
        .unwrap();
    settings_click(
        &bot,
        &handler,
        &storage,
        -100,
        2,
        selector,
        "settings:mode:clear",
    )
    .await;
    handler.repo.set_user_role(2, UserRole::User).await.unwrap();
    settings_click(
        &bot,
        &handler,
        &storage,
        -100,
        2,
        selector,
        "settings:confirm:clear",
    )
    .await;
    assert_eq!(
        handler
            .repo
            .get_chat(-100)
            .await
            .unwrap()
            .unwrap()
            .excluded_tags
            .0,
        ["new"]
    );
}
