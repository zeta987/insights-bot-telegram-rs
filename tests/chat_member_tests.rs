//! Bot-membership production wiring against Go v1.0.0
//! `welcome/welcome.go:57-184`.
//!
//! A `my_chat_member` transition of the bot itself to exactly `left`
//! triggers the five-step cleanup: subscribers, feature flags, recap options,
//! and chat histories are deleted, while the recap log keeps its row and only
//! blanks its input and output text. It sends no Telegram request. A
//! transition to exactly `member` triggers the first-join welcome: a stored
//! language and a best-effort HTML reply, gated by Go's
//! `HasJoinedGroupsBefore`. A ban (Telegram status `kicked`) or a direct
//! promotion to `administrator` matches no Go branch and must leave every row
//! alone and send nothing.

mod support;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use insights_bot_telegram_rs::{
    bot::{
        context::{AppContext, RecapRuntimeDependencies},
        handlers::chat_member::handle_my_chat_member,
    },
    config::AppConfig,
    db::{
        Database, chat_history, feature_flags,
        models::{CHAT_TYPE_GROUP, CHAT_TYPE_SUPERGROUP, NewTelegramChatHistory, TokenUsage},
        recap_logs, recap_options, subscribers,
    },
    i18n::I18n,
    redis::recap_state::{InMemoryRecapStateStore, TestClock},
    services::{
        openai::OpenAiClient,
        rate_limit::{CommandRateLimiter, GoRateLimiter},
    },
};
use serde_json::Value;
use sqlx::AnyPool;
use support::sqlite_fixture::SchemaFixture;
use teloxide::types::{ChatMemberUpdated, Me};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const CHAT_ID: i64 = -1_001_234_567_890;
const MEMBER_USER_ID: i64 = 7_654_321_098;
const START_MS: i64 = 1_700_000_000_000;

async fn test_context(server: &MockServer, database: Database) -> Arc<AppContext> {
    let values = BTreeMap::from([
        ("TELEGRAM_BOT_TOKEN".to_owned(), "test-token".to_owned()),
        (
            "TELEGRAM_BOT_API_ENDPOINT".to_owned(),
            format!("{}/telegram", server.uri()),
        ),
        (
            "OPENAI_API_SECRET".to_owned(),
            "chat-member-test-key".to_owned(),
        ),
        ("OPENAI_API_HOST".to_owned(), format!("{}/v1", server.uri())),
        (
            "OPENAI_API_MODEL_NAME".to_owned(),
            "detail-model".to_owned(),
        ),
        (
            "SARCASTIC_CONDENSED_MODEL_NAME".to_owned(),
            "condensed-model".to_owned(),
        ),
        ("REDIS_PORT".to_owned(), "6379".to_owned()),
        (
            "HARD_LIMIT_MANUAL_RECAP_RATE_PER_SECONDS".to_owned(),
            "120".to_owned(),
        ),
        ("LOCALE".to_owned(), "zh-Hant".to_owned()),
    ]);
    let config =
        AppConfig::from_lookup(|key| values.get(key).cloned()).expect("chat member test config");
    let openai = OpenAiClient::new(
        &config.openai,
        &config.recap_openai,
        &config.condensed_prompts,
    )
    .expect("OpenAI test client")
    .with_rate_limiter(Arc::new(GoRateLimiter::per_second(1_000)));
    AppContext::new(
        config,
        database,
        I18n::load_from_dir("locales").expect("embedded locales"),
        openai,
        CommandRateLimiter::new(1, Duration::from_secs(1)),
        None,
        RecapRuntimeDependencies {
            recap_state: Some(Arc::new(InMemoryRecapStateStore::new(Arc::new(
                TestClock::new(START_MS),
            )))),
            raw_telegram_http: reqwest::Client::new(),
            message_preprocessor: None,
        },
    )
}

fn bot_user() -> Value {
    serde_json::json!({
        "id": 9_999,
        "is_bot": true,
        "first_name": "Test Bot",
        "username": "TestBot"
    })
}

/// The bot's own identity, as teloxide's `Dispatcher` fetches once via
/// `get_me` and injects into every endpoint's dependency map.
fn bot_me() -> Me {
    serde_json::from_value(serde_json::json!({
        "id": 9_999,
        "is_bot": true,
        "first_name": "Test Bot",
        "username": "TestBot",
        "can_join_groups": true,
        "can_read_all_group_messages": true,
        "supports_inline_queries": false
    }))
    .expect("valid Telegram bot identity")
}

fn chat_member_update(from: Value, new_chat_member: Value) -> ChatMemberUpdated {
    serde_json::from_value(serde_json::json!({
        "chat": {
            "id": CHAT_ID,
            "type": "supergroup",
            "title": "Parity Lab"
        },
        "from": from,
        "date": 1_710_000_000,
        "old_chat_member": {
            "status": "member",
            "user": bot_user()
        },
        "new_chat_member": new_chat_member
    }))
    .expect("valid chat member update")
}

/// The human actor ("from") who performed the membership change, with an
/// optional Telegram `language_code`, matching Go's
/// `c.Update.MyChatMember.From.LanguageCode` (a raw, possibly-absent field).
fn joiner(language_code: Option<&str>) -> Value {
    let mut from = serde_json::json!({
        "id": 42,
        "is_bot": false,
        "first_name": "Ada"
    });
    if let Some(code) = language_code {
        from["language_code"] = Value::String(code.to_owned());
    }
    from
}

/// The bot's own membership becoming exactly `left`.
fn left_update() -> ChatMemberUpdated {
    chat_member_update(
        joiner(None),
        serde_json::json!({
            "status": "left",
            "user": bot_user()
        }),
    )
}

/// The bot being banned: Telegram reports `kicked`, which is not `left`.
fn banned_update() -> ChatMemberUpdated {
    chat_member_update(
        joiner(None),
        serde_json::json!({
            "status": "kicked",
            "user": bot_user(),
            "until_date": 0
        }),
    )
}

/// The bot's own membership becoming exactly `member`: a first (or
/// repeated) join, joined by a human whose Telegram `language_code` is
/// `language_code`.
fn member_update(language_code: Option<&str>) -> ChatMemberUpdated {
    chat_member_update(
        joiner(language_code),
        serde_json::json!({
            "status": "member",
            "user": bot_user()
        }),
    )
}

/// The bot being added directly as `administrator`, never passing through
/// `member`. Go's `telegram.MemberStatusMember` match therefore never fires
/// (`welcome.go:64-71`).
fn administrator_update() -> ChatMemberUpdated {
    chat_member_update(
        joiner(Some("zh-Hans")),
        serde_json::json!({
            "status": "administrator",
            "user": bot_user(),
            "is_anonymous": false,
            "can_be_edited": false,
            "can_manage_chat": true,
            "can_change_info": true,
            "can_delete_messages": true,
            "can_manage_video_chats": true,
            "can_invite_users": true,
            "can_restrict_members": true,
            "can_pin_messages": true,
            "can_promote_members": true
        }),
    )
}

/// The literal request body a Telegram `SendMessage` call carries, tolerant
/// of either JSON or form-encoded transport.
fn request_body(request: &wiremock::Request) -> Value {
    serde_json::from_slice(&request.body).unwrap_or_else(|_| {
        let map = url::form_urlencoded::parse(&request.body)
            .into_owned()
            .map(|(key, value)| (key, Value::String(value)))
            .collect::<serde_json::Map<_, _>>();
        Value::Object(map)
    })
}

fn sample_message(chat_id: i64, message_id: i64) -> NewTelegramChatHistory {
    NewTelegramChatHistory {
        chat_id,
        chat_type: CHAT_TYPE_GROUP.to_owned(),
        chat_title: "Parity Lab".to_owned(),
        message_id,
        user_id: MEMBER_USER_ID,
        username: "sender_one".to_owned(),
        full_name: "Sender One".to_owned(),
        text: "captured".to_owned(),
        replied_to_message_id: 0,
        replied_to_user_id: 0,
        replied_to_full_name: String::new(),
        replied_to_username: String::new(),
        replied_to_text: String::new(),
        replied_to_chat_type: String::new(),
        chatted_at: START_MS,
    }
}

/// Seed the five tables the cleanup reaches, returning the recap log id.
async fn seed_recap_chat(db: &Database) -> String {
    feature_flags::enable_recap(db, CHAT_ID, CHAT_TYPE_GROUP, "Parity Lab")
        .await
        .expect("seed the recap feature flag");
    recap_options::find_one_or_create(db, CHAT_ID)
        .await
        .expect("seed the recap options");
    subscribers::insert_unchecked(db, CHAT_ID, MEMBER_USER_ID)
        .await
        .expect("seed one subscriber");
    chat_history::save_one(db, &sample_message(CHAT_ID, 1))
        .await
        .expect("seed one chat history row");
    recap_logs::create_group_recap(
        db,
        CHAT_ID,
        "the recap inputs",
        "the recap outputs",
        TokenUsage {
            prompt_tokens: 11,
            completion_tokens: 22,
            total_tokens: 33,
        },
        "gpt-parity",
    )
    .await
    .expect("seed one recap log row")
}

/// The log row's text columns, proving the row itself still exists.
async fn read_recap_log(pool: &AnyPool, log_id: &str) -> (String, String) {
    let row: (String, String) = sqlx::query_as(
        "SELECT CAST(recap_inputs AS TEXT), CAST(recap_outputs AS TEXT)
         FROM log_chat_histories_recaps WHERE CAST(id AS TEXT) = $1",
    )
    .bind(log_id)
    .fetch_one(pool)
    .await
    .expect("the recap log row is readable");
    (row.0, row.1)
}

#[tokio::test]
async fn a_left_update_runs_the_five_step_cleanup_without_a_telegram_reply() {
    let server = MockServer::start().await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let log_id = seed_recap_chat(&database).await;
    let context = test_context(&server, database.clone()).await;

    handle_my_chat_member(
        left_update(),
        context.config.telegram.bot(),
        bot_me(),
        context.clone(),
    )
    .await
    .expect("bot-left update");

    assert!(
        subscribers::list(&database, CHAT_ID)
            .await
            .expect("subscribers are readable")
            .is_empty(),
        "every subscriber of the chat is deleted"
    );
    assert!(
        feature_flags::find_one_for_groups(&database, CHAT_ID, "")
            .await
            .expect("flags are readable")
            .is_none(),
        "the feature flag row is deleted"
    );
    assert!(
        recap_options::find_one(&database, CHAT_ID)
            .await
            .expect("options are readable")
            .is_none(),
        "the recap options row is deleted"
    );
    assert!(
        chat_history::find_chatted_after(&database, CHAT_ID, 0)
            .await
            .expect("histories are readable")
            .is_empty(),
        "every chat history row is deleted"
    );
    let (inputs, outputs) = read_recap_log(&database.pool, &log_id).await;
    assert_eq!(inputs, "", "the recap log input text is blanked");
    assert_eq!(outputs, "", "the recap log output text is blanked");
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty(),
        "Go's bot-left cleanup sends no Telegram reply"
    );
}

#[tokio::test]
async fn a_banned_update_leaves_every_row_alone() {
    let server = MockServer::start().await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let log_id = seed_recap_chat(&database).await;
    let context = test_context(&server, database.clone()).await;

    handle_my_chat_member(
        banned_update(),
        context.config.telegram.bot(),
        bot_me(),
        context.clone(),
    )
    .await
    .expect("bot-banned update");

    assert_eq!(
        subscribers::list(&database, CHAT_ID)
            .await
            .expect("subscribers are readable")
            .len(),
        1,
        "a ban is not Go's left status, so the subscriber survives"
    );
    assert!(
        feature_flags::find_one_for_groups(&database, CHAT_ID, "")
            .await
            .expect("flags are readable")
            .is_some()
    );
    assert!(
        recap_options::find_one(&database, CHAT_ID)
            .await
            .expect("options are readable")
            .is_some()
    );
    assert_eq!(
        chat_history::find_chatted_after(&database, CHAT_ID, 0)
            .await
            .expect("histories are readable")
            .len(),
        1
    );
    let (inputs, outputs) = read_recap_log(&database.pool, &log_id).await;
    assert_eq!(inputs, "the recap inputs");
    assert_eq!(outputs, "the recap outputs");
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty()
    );
}

/// `locales/zh-Hans.yml` `welcome.message_normal_group`, with `{Username}`
/// substituted for the test bot's `TestBot` handle.
const EXPECTED_WELCOME_ZH_HANS: &str = "🤗 欢迎使用 @TestBot！\n\n- 如果要让我帮忙阅读网页文章，请直接使用开箱即用的命令 /smr@TestBot <code>要阅读的链接</code>；\n\n- 如果想要我帮忙总结本群组的聊天记录，请以<b>管理员</b>身份将我配置为本群组的管理员（可以关闭所有权限），然后在<b>非匿名和非其他身份的身份</b>下（推荐，否则容易出现权限识别错误的情况）发送 /configure_recap@TestBot 来开始配置本群组的聊天回顾功能。\n\n- 如果你在授权 Bot 管理员之后希望 Bot 将已经记录的消息全数移除，可以通过撤销 Bot 的管理员权限来触发 Bot 的历史数据自动清理（如果该部分代码未经其他 Bot 实例维护者修改的话）。\n\n⚠️ 警告：你的群组尚未是超级群组（supergroup）。<b>普通群组的消息链接引用功能无法正常工作。</b>\n\n如果你希望使用消息链接引用功能，请通过下面任意操作使其正常运作：\n\n- 短时间内将群组开放为公共群组并快速还原回私有群组；\n- 通过其他操作将本群组升级为超级群组；\n\n如果还有疑问的话可以通过\n\n1. 执行帮助命令 /help@TestBot 来查看支持的命令；\n2. 前往 Bot 所在的<a href=\"https://github.com/nekomeowww/insights-bot\">开源仓库</a>提交 Issue 问询开发者。\n\n祝你使用愉快！\n";

/// `locales/en.yml` `welcome.message_normal_group`, with `{Username}`
/// substituted for the test bot's `TestBot` handle. English is also what a
/// missing or unrecognized `language_code` falls back to.
const EXPECTED_WELCOME_EN: &str = "🤗 Welcome to @TestBot!\n\n- Use /smr@TestBot <code>article link</code> for article readings.\n\n- For chat history summaries, please assign me as admin (all permissions can be omitted) using a <b>non-anonymous identity</b> (recommended, otherwise permission validation may fail. Then, start /configure_recap@TestBot to configure chat recap.\n\n- Removing my admin status is a simple way to delete all bot-recorded messages, automatically purging bot data unless modifications have been made by another maintainer.\n\n⚠️ Your group isn't a supergroup yet; message reference linking will not work.\n\nTo enable message reference linking:\n\n- Temporarily switch your group to public, then revert to private.\n- Upgrade to a supergroup by other means.\n\nQuestions?\n\n1. Consult /help@TestBot for command details.\n2. Visit our <a href=\"https://github.com/nekomeowww/insights-bot\">GitHub</a> for support.\n\nEnjoy your experience!\n";

#[tokio::test]
async fn a_first_member_update_sets_the_joiner_language_and_sends_the_welcome_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 500,
                "date": 1_710_000_001,
                "chat": {"id": CHAT_ID, "type": "supergroup"},
                "text": "welcomed"
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let context = test_context(&server, database.clone()).await;

    handle_my_chat_member(
        member_update(Some("zh-Hans")),
        context.config.telegram.bot(),
        bot_me(),
        context.clone(),
    )
    .await
    .expect("bot-join update");

    assert_eq!(
        feature_flags::find_language(&database, CHAT_ID, "Parity Lab")
            .await
            .expect("language is readable"),
        "zh-Hans",
        "the joiner's raw Telegram language_code is stored verbatim"
    );

    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 1, "exactly one Telegram request was sent");
    let body = request_body(&requests[0]);
    assert_eq!(body["chat_id"].as_i64(), Some(CHAT_ID));
    assert_eq!(body["parse_mode"], "HTML");
    assert_eq!(body["text"], EXPECTED_WELCOME_ZH_HANS);
}

#[tokio::test]
async fn a_repeated_member_update_does_nothing() {
    let server = MockServer::start().await;
    // No mock is mounted for SendMessage: the assertion below on
    // `received_requests()` is what proves the repeated join sends nothing.

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    // Go's `HasJoinedGroupsBefore` reads true once any row exists for the
    // chat; seed one with a language chosen by an earlier join.
    feature_flags::set_language(
        &database,
        CHAT_ID,
        CHAT_TYPE_SUPERGROUP,
        "Parity Lab",
        "zh-Hans",
    )
    .await
    .expect("seed the group as already joined");
    let context = test_context(&server, database.clone()).await;

    handle_my_chat_member(
        member_update(Some("zh-Hant")),
        context.config.telegram.bot(),
        bot_me(),
        context.clone(),
    )
    .await
    .expect("repeated bot-join update");

    assert_eq!(
        feature_flags::find_language(&database, CHAT_ID, "Parity Lab")
            .await
            .expect("language is readable"),
        "zh-Hans",
        "Go's HasJoinedGroupsBefore guard returns before SetLanguageForGroups \
         runs, so the language from the new join is never written"
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty(),
        "Go's HasJoinedGroupsBefore guard returns before the welcome send too"
    );
}

#[tokio::test]
async fn an_administrator_update_triggers_nothing() {
    let server = MockServer::start().await;
    // No mock is mounted for SendMessage: being added directly as
    // administrator never matches Go's `telegram.MemberStatusMember` arm.

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let context = test_context(&server, database.clone()).await;

    handle_my_chat_member(
        administrator_update(),
        context.config.telegram.bot(),
        bot_me(),
        context.clone(),
    )
    .await
    .expect("bot-promoted-to-administrator update");

    assert!(
        feature_flags::find_one_for_groups(&database, CHAT_ID, "")
            .await
            .expect("flags are readable")
            .is_none(),
        "no feature flag row is created for a direct administrator add"
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty(),
        "a direct administrator add sends no welcome message"
    );
}

#[tokio::test]
async fn a_failed_welcome_send_does_not_undo_the_stored_language_or_error_the_handler() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
            "ok": false,
            "error_code": 500,
            "description": "Internal Server Error"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let context = test_context(&server, database.clone()).await;

    let outcome = handle_my_chat_member(
        member_update(None),
        context.config.telegram.bot(),
        bot_me(),
        context.clone(),
    )
    .await;

    assert!(
        outcome.is_ok(),
        "a best-effort welcome-send failure must not surface as a handler error"
    );
    assert_eq!(
        feature_flags::find_language(&database, CHAT_ID, "Parity Lab")
            .await
            .expect("language is readable"),
        "",
        "the stored language survives a welcome-send failure \
         (Go's MaySend failure is only logged)"
    );
    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(
        requests.len(),
        1,
        "the welcome send was attempted despite failing"
    );
    let body = request_body(&requests[0]);
    assert_eq!(
        body["text"], EXPECTED_WELCOME_EN,
        "a missing language_code falls back to English, matching Go's i18n default"
    );
}
