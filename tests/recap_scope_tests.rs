use insights_bot_telegram_rs::{
    bot::commands::{Command, parse_go_command},
    db::{
        Database, DbBackend, chat_history, migration, models::MessageKind, models::RecapConfig,
        recap_config,
    },
};
use sqlx::{AnyPool, any::AnyPoolOptions};
use teloxide::utils::command::BotCommands;

async fn setup_sqlite_pool() -> AnyPool {
    sqlx::any::install_default_drivers();
    let pool = AnyPoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("sqlite memory pool");

    // Run every migration: the parity tables the chat migration touches are
    // created by 0003.
    let migrations = [
        include_str!("../migrations/sqlite/0001_init.sql"),
        include_str!("../migrations/sqlite/0002_recap_config_extensions.sql"),
        include_str!("../migrations/sqlite/0003_rich_recap_parity.sql"),
    ];

    for migration_sql in migrations {
        for statement in migration_sql.split(';') {
            let stmt: String = statement
                .lines()
                .filter(|line| {
                    let trimmed = line.trim();
                    !trimmed.is_empty() && !trimmed.starts_with("--")
                })
                .collect::<Vec<_>>()
                .join("\n");
            let stmt = stmt.trim();
            if stmt.is_empty() {
                continue;
            }
            if let Err(err) = sqlx::query(stmt).execute(&pool).await {
                let msg = err.to_string();
                if !msg.contains("duplicate column") {
                    panic!("migration failed: {msg}");
                }
            }
        }
    }

    pool
}

fn default_config(chat_id: i64) -> RecapConfig {
    RecapConfig {
        chat_id,
        enabled: true,
        auto_recap_enabled: false,
        last_recap_at: None,
        updated_at: None,
        auto_recap_rates_per_day: 4,
        pin_auto_recap_message: false,
        last_pinned_message_id: None,
    }
}

#[test]
fn command_surface_only_lists_supported_commands() {
    let descriptions = Command::descriptions().to_string();
    for supported in [
        "/start",
        "/help",
        "/cancel",
        "/recap",
        "/configure_recap",
        "/recap_forwarded_start",
        "/recap_forwarded",
        "/subscribe_recap",
        "/unsubscribe_recap",
    ] {
        assert!(
            descriptions.contains(supported),
            "missing supported command {supported}"
        );
    }

    assert!(Command::parse("/recap_forwarded_start", "TestBot").is_ok());
    assert!(Command::parse("/recap_forwarded", "TestBot").is_ok());
}

#[test]
fn go_command_parser_ignores_mentions_and_forwarded_arguments() {
    assert!(matches!(
        parse_go_command("/recap_forwarded_start@OtherBot ignored", "TestBot"),
        Some(Command::RecapForwardedStart)
    ));
    assert!(matches!(
        parse_go_command("/recap_forwarded@OtherBot ignored", "TestBot"),
        Some(Command::RecapForwarded)
    ));
}

#[test]
fn start_command_preserves_the_deep_link_argument_for_handler_validation() {
    match Command::parse("/start token extra", "TestBot").expect("parse /start") {
        Command::Start(arguments) => assert_eq!(arguments, "token extra"),
        command => panic!("unexpected command: {command:?}"),
    }
    match Command::parse("/start", "TestBot").expect("parse bare /start") {
        Command::Start(arguments) => assert!(arguments.is_empty()),
        command => panic!("unexpected command: {command:?}"),
    }
}

#[tokio::test]
async fn disabled_chat_blocks_message_capture_and_auto_recap_listing() {
    let pool = setup_sqlite_pool().await;

    let cfg = RecapConfig {
        enabled: false,
        auto_recap_enabled: true,
        ..default_config(42)
    };
    recap_config::upsert_recap_config(&pool, &cfg)
        .await
        .expect("config insert should succeed");

    assert!(
        !chat_history::is_recap_enabled(&pool, 42)
            .await
            .expect("enablement lookup should succeed")
    );

    let all = recap_config::list_auto_recap_enabled(&pool)
        .await
        .expect("list should succeed");
    assert!(
        all.iter().all(|item| item.chat_id != 42),
        "disabled chat must not appear in auto-recap list"
    );
}

#[tokio::test]
async fn auto_recap_enabled_configs_are_listed() {
    let pool = setup_sqlite_pool().await;

    recap_config::upsert_recap_config(
        &pool,
        &RecapConfig {
            enabled: true,
            auto_recap_enabled: true,
            last_recap_at: Some(100),
            ..default_config(1)
        },
    )
    .await
    .expect("config insert should succeed");

    recap_config::upsert_recap_config(
        &pool,
        &RecapConfig {
            enabled: true,
            auto_recap_enabled: false,
            ..default_config(2)
        },
    )
    .await
    .expect("config insert should succeed");

    recap_config::upsert_recap_config(
        &pool,
        &RecapConfig {
            enabled: true,
            auto_recap_enabled: true,
            last_recap_at: None,
            ..default_config(3)
        },
    )
    .await
    .expect("config insert should succeed");

    let configs = recap_config::list_auto_recap_enabled(&pool)
        .await
        .expect("list should succeed");
    let ids: Vec<i64> = configs.iter().map(|c| c.chat_id).collect();

    assert!(ids.contains(&1), "auto-recap enabled chat should be listed");
    assert!(ids.contains(&3), "auto-recap enabled chat should be listed");
    assert!(
        !ids.contains(&2),
        "auto-recap disabled chat should not be listed"
    );
}

#[tokio::test]
async fn frequency_and_pin_fields_round_trip() {
    let pool = setup_sqlite_pool().await;

    recap_config::upsert_recap_config(
        &pool,
        &RecapConfig {
            auto_recap_rates_per_day: 3,
            pin_auto_recap_message: true,
            last_pinned_message_id: Some(12345),
            ..default_config(100)
        },
    )
    .await
    .expect("upsert should succeed");

    let cfg = recap_config::get_recap_config(&pool, 100)
        .await
        .expect("get should succeed")
        .expect("config should exist");

    assert_eq!(cfg.auto_recap_rates_per_day, 3);
    assert!(cfg.pin_auto_recap_message);
    assert_eq!(cfg.last_pinned_message_id, Some(12345));
}

#[tokio::test]
async fn set_auto_recap_rates_per_day_updates_correctly() {
    let pool = setup_sqlite_pool().await;

    recap_config::upsert_recap_config(&pool, &default_config(200))
        .await
        .expect("upsert should succeed");

    recap_config::set_auto_recap_rates_per_day(&pool, 200, 2)
        .await
        .expect("set rates should succeed");

    let cfg = recap_config::get_recap_config(&pool, 200)
        .await
        .expect("get should succeed")
        .expect("config should exist");

    assert_eq!(cfg.auto_recap_rates_per_day, 2);
}

#[tokio::test]
async fn set_pin_auto_recap_message_updates_correctly() {
    let pool = setup_sqlite_pool().await;

    recap_config::upsert_recap_config(&pool, &default_config(300))
        .await
        .expect("upsert should succeed");

    recap_config::set_pin_auto_recap_message(&pool, 300, true)
        .await
        .expect("set pin should succeed");

    let cfg = recap_config::get_recap_config(&pool, 300)
        .await
        .expect("get should succeed")
        .expect("config should exist");

    assert!(cfg.pin_auto_recap_message);
}

#[tokio::test]
async fn default_config_has_expected_values() {
    let pool = setup_sqlite_pool().await;

    let cfg = recap_config::get_or_create_recap_config(&pool, 999)
        .await
        .expect("get_or_create should succeed");

    assert_eq!(
        cfg.auto_recap_rates_per_day, 4,
        "default frequency should be 4x/day"
    );
    assert!(!cfg.pin_auto_recap_message, "pin should default to false");
    assert_eq!(
        cfg.last_pinned_message_id, None,
        "no pinned message initially"
    );
}

// -- Tests for update_message_text (Task 9.2) --

#[tokio::test]
async fn update_message_text_updates_existing_message() {
    let pool = setup_sqlite_pool().await;

    chat_history::insert_message(
        &pool,
        10,
        100,
        Some(1),
        Some("Alice".into()),
        Some("alice".into()),
        MessageKind::Text,
        Some("original text".into()),
        None,
        1000,
    )
    .await
    .expect("insert should succeed");

    chat_history::update_message_text(&pool, 10, 100, "edited text")
        .await
        .expect("update should succeed");

    let msgs = chat_history::recent_messages(&pool, 10, 10)
        .await
        .expect("fetch should succeed");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].text, "edited text");
}

#[tokio::test]
async fn update_message_text_noop_on_missing_message() {
    let pool = setup_sqlite_pool().await;

    // No message inserted; update should succeed but change nothing
    chat_history::update_message_text(&pool, 10, 999, "some text")
        .await
        .expect("update on missing should not fail");
}

// -- Tests for migrate_chat_data (Task 9.3) --

/// The application handle around a fixture pool.
///
/// `migrate_chat_data` reaches several parity repositories, which branch on the
/// backend, so they need the handle rather than the bare pool.
fn database(pool: &AnyPool) -> Database {
    Database {
        pool: pool.clone(),
        backend: DbBackend::Sqlite,
    }
}

#[tokio::test]
async fn migrate_chat_data_moves_histories() {
    let pool = setup_sqlite_pool().await;
    let db = database(&pool);

    chat_history::insert_message(
        &pool,
        50,
        1,
        Some(1),
        Some("Contributor".into()),
        None,
        MessageKind::Text,
        Some("hello".into()),
        None,
        2000,
    )
    .await
    .expect("insert msg");

    migration::migrate_chat_data(&db, 50, 500).await;

    let old_msgs = chat_history::recent_messages(&pool, 50, 10)
        .await
        .expect("fetch old");
    assert!(
        old_msgs.is_empty(),
        "old chat should have no messages after migration"
    );

    let new_msgs = chat_history::recent_messages(&pool, 500, 10)
        .await
        .expect("fetch new");
    assert_eq!(new_msgs.len(), 1);
    assert_eq!(new_msgs[0].text, "hello");
}

#[tokio::test]
async fn migrate_chat_data_leaves_the_rust_only_recap_config_alone() {
    let pool = setup_sqlite_pool().await;
    let db = database(&pool);

    recap_config::upsert_recap_config(
        &pool,
        &RecapConfig {
            auto_recap_enabled: true,
            ..default_config(50)
        },
    )
    .await
    .expect("insert config");

    migration::migrate_chat_data(&db, 50, 500).await;

    // `recap_configs` has no Go counterpart, so the parity orchestrator must
    // not move, copy, or delete it.
    let old_cfg = recap_config::get_recap_config(&pool, 50)
        .await
        .expect("fetch old cfg")
        .expect("the original row stays where it was");
    assert!(old_cfg.auto_recap_enabled);

    assert!(
        recap_config::get_recap_config(&pool, 500)
            .await
            .expect("fetch new cfg")
            .is_none(),
        "no row is created under the supergroup identifier"
    );
}

#[tokio::test]
async fn migrate_chat_data_noop_on_empty() {
    let pool = setup_sqlite_pool().await;

    // No data for chat 999 — every step is a no-op and none reports a failure.
    migration::migrate_chat_data(&database(&pool), 999, 9999).await;
}
