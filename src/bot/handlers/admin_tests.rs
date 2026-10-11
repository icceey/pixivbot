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
    Mock::given(path("/botfake_token/SendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "result": {"message_id": 2, "date": 1700000000, "chat": {"id": -100, "type": "supergroup", "title": "Test"}}
        })))
        .mount(&server).await;
    let bot = Bot::new("fake_token")
        .set_api_url(url::Url::parse(&server.uri()).unwrap())
        .throttle(Limits::default());
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
    assert_eq!(
        resolve_admin_target(&bot, &handler.repo, &message(2, 1), "@alice")
            .await
            .unwrap(),
        2
    );
}

// Revoking Telegram admin status while a settings dialogue is open must prevent a write.
#[tokio::test]
async fn settings_submission_rechecks_revoked_group_permission() {
    use crate::bot::state::{new_settings_storage, SettingsState};
    let (server, bot, handler) = fixture().await;
    let storage = new_settings_storage();
    storage.write().await.insert(
        (ChatId(-100), UserId(2)),
        SettingsState::WaitingForExcludedTags {
            settings_message_id: teloxide::types::MessageId(2),
            created_at: std::time::Instant::now(),
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
        message(-100, 2),
        handler.clone(),
        storage.clone()
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
