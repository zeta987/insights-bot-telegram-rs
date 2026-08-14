//! Task 4B1 — feature flags, recap options, and auto-recap subscribers.
//!
//! Behaviour is pinned to Go v1.0.0 `internal/models/tgchats/{feature_flags,
//! recaps_options,auto_recaps_subscribers}.go`. The deliberate Go quirks are
//! reproduced rather than tidied: no uniqueness on subscribers, no ordering on
//! either list, one-row unsubscribe, and the asymmetry where only the
//! find-or-create path seeds `feature_language`.
//!
//! The first section characterises the `sqlx::Any` decode paths this slice is
//! allowed to depend on. The driver narrows a SQLite `INTEGER` to 32 bits on the
//! typed scalar path and refuses a genuine `NULL` on the `Option<String>` path,
//! so production reads every integer, identifier, and boolean through an
//! explicit `CAST(... AS TEXT)` and parses it.

mod support;

use insights_bot_telegram_rs::db::{
    Database, codec, feature_flags,
    models::{AutoRecapSendMode, TelegramChatRecapsOptions},
    recap_options, subscribers,
};
use support::sqlite_fixture::SchemaFixture;

/// Beyond signed 32 bits in both directions, like a real supergroup and user.
const BIG_CHAT_ID: i64 = -1_001_234_567_890;
const BIG_USER_ID: i64 = 7_654_321_098;
/// A Unix-millisecond instant that does not fit in 32 bits.
const BIG_TIMESTAMP_MS: i64 = 1_700_000_000_000;

const GROUP: &str = "group";
const SUPERGROUP: &str = "supergroup";

async fn database() -> (SchemaFixture, Database) {
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    (fixture, database)
}

// ---------------------------------------------------------------------------
// sqlx::Any decode characterization
// ---------------------------------------------------------------------------

#[tokio::test]
async fn large_telegram_identifiers_round_trip_through_the_text_decode_path() {
    let (_fixture, db) = database().await;

    for chat_id in [BIG_CHAT_ID, BIG_USER_ID, i64::MIN, i64::MAX, 0, -1] {
        subscribers::insert_unchecked(&db, chat_id, BIG_USER_ID)
            .await
            .expect("insert");

        let rows = subscribers::list(&db, chat_id).await.expect("list");
        assert_eq!(rows.len(), 1, "exactly one row for {chat_id}");
        assert_eq!(rows[0].chat_id, chat_id, "chat id {chat_id} was narrowed");
        assert_eq!(rows[0].user_id, BIG_USER_ID, "user id was narrowed");
    }
}

#[tokio::test]
async fn unix_millisecond_timestamps_round_trip() {
    let (_fixture, db) = database().await;

    subscribers::insert_unchecked(&db, BIG_CHAT_ID, BIG_USER_ID)
        .await
        .expect("insert");
    let stored = subscribers::list(&db, BIG_CHAT_ID).await.expect("list");

    // Repositories stamp Unix milliseconds, so the value must exceed 32 bits.
    assert!(
        stored[0].created_at > BIG_TIMESTAMP_MS,
        "created_at {} is not a plausible Unix-millisecond instant",
        stored[0].created_at
    );
    assert_eq!(stored[0].created_at, stored[0].updated_at);
}

#[tokio::test]
async fn uuid_identifiers_round_trip_as_text() {
    let (_fixture, db) = database().await;

    subscribers::insert_unchecked(&db, BIG_CHAT_ID, BIG_USER_ID)
        .await
        .expect("insert");
    subscribers::insert_unchecked(&db, BIG_CHAT_ID, BIG_USER_ID + 1)
        .await
        .expect("insert");

    let rows = subscribers::list(&db, BIG_CHAT_ID).await.expect("list");
    for row in &rows {
        assert_eq!(row.id.len(), 36, "a hyphenated UUID: {}", row.id);
        uuid::Uuid::parse_str(&row.id).expect("a parseable UUID");
    }
    assert_ne!(rows[0].id, rows[1].id, "identifiers must be distinct");
}

#[tokio::test]
async fn booleans_round_trip_in_both_directions() {
    let (_fixture, db) = database().await;

    feature_flags::enable_recap(&db, BIG_CHAT_ID, SUPERGROUP, "Group")
        .await
        .expect("enable");
    let enabled = feature_flags::find_one_for_groups(&db, BIG_CHAT_ID, "")
        .await
        .expect("find")
        .expect("row");
    assert!(enabled.feature_chat_histories_recap);

    feature_flags::disable_recap(&db, BIG_CHAT_ID, SUPERGROUP, "Group")
        .await
        .expect("disable");
    let disabled = feature_flags::find_one_for_groups(&db, BIG_CHAT_ID, "")
        .await
        .expect("find")
        .expect("row");
    assert!(!disabled.feature_chat_histories_recap);
}

#[tokio::test]
async fn a_true_sql_null_decodes_as_none_rather_than_failing() {
    let (_fixture, db) = database().await;

    // media_url is the one nullable column reachable without a repository.
    sqlx::query(
        "INSERT INTO chat_histories (chat_id, message_id, kind, created_at) VALUES ($1, $2, $3, $4)",
    )
    .bind(BIG_CHAT_ID)
    .bind(1_i64)
    .bind("text")
    .bind(BIG_TIMESTAMP_MS)
    .execute(&db.pool)
    .await
    .expect("insert");

    let row = sqlx::query(
        "SELECT CAST(media_url AS TEXT), CAST(chat_id AS TEXT), CAST(created_at AS TEXT)
         FROM chat_histories WHERE chat_id = $1 LIMIT 1",
    )
    .bind(BIG_CHAT_ID)
    .fetch_one(&db.pool)
    .await
    .expect("select");

    assert_eq!(codec::nullable_text_at(&row, 0).expect("decode"), None);
    assert_eq!(codec::i64_at(&row, 1).expect("decode"), BIG_CHAT_ID);
    assert_eq!(codec::i64_at(&row, 2).expect("decode"), BIG_TIMESTAMP_MS);
}

// ---------------------------------------------------------------------------
// Timestamps
//
// Ent declares `updated_at` with `DefaultFunc` only, never `UpdateDefault`, and
// no Go update or migrate caller sets it. Both stamps are therefore written once
// at create and never move again.
//
// Each test pins the stored stamps to a sentinel first, so the assertions do not
// depend on how many milliseconds an operation happens to take.
// ---------------------------------------------------------------------------

/// A fixed instant well before now, planted so any restamp is visible.
const SENTINEL_MS: i64 = 1_600_000_000_000;

async fn pin_timestamps(db: &Database, table: &str, chat_id: i64) {
    sqlx::query(&format!(
        "UPDATE {table} SET created_at = $1, updated_at = $1 WHERE chat_id = $2"
    ))
    .bind(SENTINEL_MS)
    .bind(chat_id)
    .execute(&db.pool)
    .await
    .expect("pin the stored timestamps");
}

#[tokio::test]
async fn feature_flag_updates_and_migration_never_advance_the_timestamps() {
    let (_fixture, db) = database().await;

    feature_flags::enable_recap(&db, BIG_CHAT_ID, GROUP, "")
        .await
        .expect("enable");
    let created = feature_flags::find_one_for_groups(&db, BIG_CHAT_ID, "")
        .await
        .expect("find")
        .expect("row");
    assert_eq!(
        created.created_at, created.updated_at,
        "a create stamps both columns from one instant"
    );

    pin_timestamps(&db, "telegram_chat_feature_flags", BIG_CHAT_ID).await;

    // Title repair, language, disable, and enable all mutate the row.
    feature_flags::find_one_for_groups(&db, BIG_CHAT_ID, "Example Group")
        .await
        .expect("repair the title");
    feature_flags::set_language(&db, BIG_CHAT_ID, GROUP, "Example Group", "zh-Hant")
        .await
        .expect("set language");
    feature_flags::disable_recap(&db, BIG_CHAT_ID, GROUP, "Example Group")
        .await
        .expect("disable");
    feature_flags::enable_recap(&db, BIG_CHAT_ID, GROUP, "Example Group")
        .await
        .expect("enable");

    let mutated = feature_flags::find_one_for_groups(&db, BIG_CHAT_ID, "")
        .await
        .expect("find")
        .expect("row");
    assert_eq!(
        mutated.chat_title, "Example Group",
        "the mutations did happen"
    );
    assert_eq!(mutated.feature_language, "zh-Hant");
    assert!(mutated.feature_chat_histories_recap);
    assert_eq!(mutated.created_at, SENTINEL_MS);
    assert_eq!(mutated.updated_at, SENTINEL_MS);

    feature_flags::migrate_chat_id(&db, BIG_CHAT_ID, -1_009_999_999_999)
        .await
        .expect("migrate");
    let moved = feature_flags::find_one_for_groups(&db, -1_009_999_999_999, "")
        .await
        .expect("find")
        .expect("row");
    assert_eq!(moved.created_at, SENTINEL_MS);
    assert_eq!(moved.updated_at, SENTINEL_MS);
}

#[tokio::test]
async fn recap_option_updates_and_migration_never_advance_the_timestamps() {
    let (_fixture, db) = database().await;

    let created = recap_options::find_one_or_create(&db, BIG_CHAT_ID)
        .await
        .expect("create");
    assert_eq!(
        created.created_at, created.updated_at,
        "a create stamps both columns from one instant"
    );

    pin_timestamps(&db, "telegram_chat_recaps_options", BIG_CHAT_ID).await;

    recap_options::set_send_mode(
        &db,
        BIG_CHAT_ID,
        AutoRecapSendMode::OnlyPrivateSubscriptions,
    )
    .await
    .expect("set mode");
    recap_options::set_rates_per_day(&db, BIG_CHAT_ID, 2)
        .await
        .expect("set rate");
    recap_options::set_pin_enabled(&db, BIG_CHAT_ID)
        .await
        .expect("pin");
    recap_options::set_pin_disabled(&db, BIG_CHAT_ID)
        .await
        .expect("unpin");

    let mutated = recap_options::find_one(&db, BIG_CHAT_ID)
        .await
        .expect("find")
        .expect("row");
    assert_eq!(mutated.auto_recap_send_mode, 1, "the mutations did happen");
    assert_eq!(mutated.auto_recap_rates_per_day, 2);
    assert_eq!(mutated.created_at, SENTINEL_MS);
    assert_eq!(mutated.updated_at, SENTINEL_MS);

    recap_options::migrate_chat_id(&db, BIG_CHAT_ID, -1_009_999_999_999)
        .await
        .expect("migrate");
    let moved = recap_options::find_one(&db, -1_009_999_999_999)
        .await
        .expect("find")
        .expect("row");
    assert_eq!(moved.created_at, SENTINEL_MS);
    assert_eq!(moved.updated_at, SENTINEL_MS);
}

#[tokio::test]
async fn subscriber_migration_never_advances_the_timestamps() {
    let (_fixture, db) = database().await;

    subscribers::insert_unchecked(&db, BIG_CHAT_ID, BIG_USER_ID)
        .await
        .expect("insert");
    subscribers::insert_unchecked(&db, BIG_CHAT_ID, BIG_USER_ID)
        .await
        .expect("insert a duplicate");
    for row in subscribers::list(&db, BIG_CHAT_ID).await.expect("list") {
        assert_eq!(
            row.created_at, row.updated_at,
            "a create stamps both columns from one instant"
        );
    }

    pin_timestamps(&db, "telegram_chat_auto_recaps_subscribers", BIG_CHAT_ID).await;

    subscribers::migrate_chat_id(&db, BIG_CHAT_ID, -1_009_999_999_999)
        .await
        .expect("migrate");

    let moved = subscribers::list(&db, -1_009_999_999_999)
        .await
        .expect("list");
    assert_eq!(moved.len(), 2, "every physical row moved");
    for row in moved {
        assert_eq!(row.created_at, SENTINEL_MS);
        assert_eq!(row.updated_at, SENTINEL_MS);
    }
}

// ---------------------------------------------------------------------------
// Feature flags
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reads_for_an_absent_row_behave_disabled() {
    let (_fixture, db) = database().await;

    assert_eq!(
        feature_flags::find_one_for_groups(&db, BIG_CHAT_ID, "Group")
            .await
            .expect("find"),
        None
    );
    assert!(
        !feature_flags::has_recap_enabled(&db, BIG_CHAT_ID, "Group")
            .await
            .expect("has")
    );
    assert!(
        !feature_flags::has_joined_before(&db, BIG_CHAT_ID, "Group")
            .await
            .expect("joined")
    );
    assert_eq!(
        feature_flags::find_language(&db, BIG_CHAT_ID, "Group")
            .await
            .expect("language"),
        "en"
    );
    assert!(
        feature_flags::list_recap_enabled_groups(&db)
            .await
            .expect("list")
            .is_empty()
    );
}

#[tokio::test]
async fn only_group_and_supergroup_chat_types_are_eligible() {
    let (_fixture, db) = database().await;

    assert!(feature_flags::is_eligible_chat_type(GROUP));
    assert!(feature_flags::is_eligible_chat_type(SUPERGROUP));
    for ineligible in ["private", "channel", "", "Group", "supergroups"] {
        assert!(
            !feature_flags::is_eligible_chat_type(ineligible),
            "{ineligible} must not be eligible"
        );

        feature_flags::enable_recap(&db, BIG_CHAT_ID, ineligible, "Chat")
            .await
            .expect("an ineligible chat type is a silent no-op");
        feature_flags::disable_recap(&db, BIG_CHAT_ID, ineligible, "Chat")
            .await
            .expect("an ineligible chat type is a silent no-op");
        feature_flags::set_language(&db, BIG_CHAT_ID, ineligible, "Chat", "zh-Hant")
            .await
            .expect("an ineligible chat type is a silent no-op");

        assert_eq!(
            feature_flags::find_one_for_groups(&db, BIG_CHAT_ID, "")
                .await
                .expect("find"),
            None,
            "{ineligible} must not create a row"
        );
    }
}

#[tokio::test]
async fn enabling_a_missing_row_creates_it_with_english_and_then_enables_recap() {
    let (_fixture, db) = database().await;

    feature_flags::enable_recap(&db, BIG_CHAT_ID, GROUP, "Example Group")
        .await
        .expect("enable");

    let stored = feature_flags::find_one_for_groups(&db, BIG_CHAT_ID, "")
        .await
        .expect("find")
        .expect("row");
    assert_eq!(stored.chat_id, BIG_CHAT_ID);
    assert_eq!(stored.chat_type, GROUP);
    assert_eq!(stored.chat_title, "Example Group");
    assert_eq!(stored.feature_language, "en");
    assert!(stored.feature_chat_histories_recap);
    assert!(stored.created_at > BIG_TIMESTAMP_MS);

    // Enabling again is a no-op rather than an error.
    feature_flags::enable_recap(&db, BIG_CHAT_ID, GROUP, "Example Group")
        .await
        .expect("enable again");
    assert_eq!(
        feature_flags::list_recap_enabled_groups(&db)
            .await
            .expect("list")
            .len(),
        1
    );
}

#[tokio::test]
async fn disabling_a_missing_row_creates_a_disabled_row_without_seeding_a_language() {
    let (_fixture, db) = database().await;

    feature_flags::disable_recap(&db, BIG_CHAT_ID, SUPERGROUP, "Example Group")
        .await
        .expect("disable");

    let stored = feature_flags::find_one_for_groups(&db, BIG_CHAT_ID, "")
        .await
        .expect("find")
        .expect("row");
    assert!(!stored.feature_chat_histories_recap);
    assert_eq!(
        stored.feature_language, "",
        "Go's disable path never sets a language, unlike find-or-create"
    );
    // The reader still reports the Go default for an unset language.
    assert_eq!(
        feature_flags::find_language(&db, BIG_CHAT_ID, "")
            .await
            .expect("language"),
        ""
    );
}

#[tokio::test]
async fn an_empty_stored_title_is_repaired_in_storage_but_not_in_the_returned_model() {
    let (_fixture, db) = database().await;

    feature_flags::enable_recap(&db, BIG_CHAT_ID, GROUP, "")
        .await
        .expect("enable without a title");
    assert_eq!(
        feature_flags::find_one_for_groups(&db, BIG_CHAT_ID, "")
            .await
            .expect("find")
            .expect("row")
            .chat_title,
        ""
    );

    // Go builds the update from the queried entity, discards the node `Save`
    // returns, and hands back the original, so the repairing call still reports
    // the empty title it read.
    let repairing = feature_flags::find_one_for_groups(&db, BIG_CHAT_ID, "Example Group")
        .await
        .expect("find")
        .expect("row");
    assert_eq!(
        repairing.chat_title, "",
        "the repairing call returns the stale entity it read"
    );

    // The write itself happened, so every later read observes it.
    for supplied in ["", "Example Group", "Renamed"] {
        assert_eq!(
            feature_flags::find_one_for_groups(&db, BIG_CHAT_ID, supplied)
                .await
                .expect("find")
                .expect("row")
                .chat_title,
            "Example Group",
            "the repair must be persisted and then never overwritten"
        );
    }
}

#[tokio::test]
async fn a_known_stored_title_is_never_overwritten() {
    let (_fixture, db) = database().await;

    feature_flags::enable_recap(&db, BIG_CHAT_ID, GROUP, "Original")
        .await
        .expect("enable");
    let unchanged = feature_flags::find_one_for_groups(&db, BIG_CHAT_ID, "Renamed")
        .await
        .expect("find")
        .expect("row");
    assert_eq!(unchanged.chat_title, "Original");
}

#[tokio::test]
async fn list_enabled_groups_returns_the_physical_rows_it_finds() {
    let (_fixture, db) = database().await;

    feature_flags::enable_recap(&db, -300, GROUP, "C")
        .await
        .expect("enable");
    feature_flags::enable_recap(&db, -100, SUPERGROUP, "A")
        .await
        .expect("enable");
    feature_flags::enable_recap(&db, -200, GROUP, "B")
        .await
        .expect("enable");
    feature_flags::disable_recap(&db, -400, GROUP, "D")
        .await
        .expect("disable");

    let listed = feature_flags::list_recap_enabled_groups(&db)
        .await
        .expect("list");
    // Go adds no ORDER BY, so the engine may return any permutation. Only the
    // membership and the row count are contractual.
    let mut ids: Vec<i64> = listed.iter().map(|flags| flags.chat_id).collect();
    ids.sort_unstable();

    assert_eq!(ids, vec![-300, -200, -100], "only enabled groups appear");
    assert!(!ids.contains(&-400), "a disabled group is excluded");
}

#[tokio::test]
async fn set_language_creates_then_updates_the_stored_language() {
    let (_fixture, db) = database().await;

    feature_flags::set_language(&db, BIG_CHAT_ID, SUPERGROUP, "Example Group", "zh-Hant")
        .await
        .expect("set language");
    assert_eq!(
        feature_flags::find_language(&db, BIG_CHAT_ID, "")
            .await
            .expect("language"),
        "zh-Hant"
    );
    assert!(
        !feature_flags::has_recap_enabled(&db, BIG_CHAT_ID, "")
            .await
            .expect("has"),
        "setting a language must not enable recap"
    );

    feature_flags::set_language(&db, BIG_CHAT_ID, SUPERGROUP, "Example Group", "en")
        .await
        .expect("set language again");
    assert_eq!(
        feature_flags::find_language(&db, BIG_CHAT_ID, "")
            .await
            .expect("language"),
        "en"
    );
}

#[tokio::test]
async fn feature_flags_delete_and_migrate_touch_every_matching_row() {
    let (_fixture, db) = database().await;

    feature_flags::enable_recap(&db, BIG_CHAT_ID, GROUP, "Example Group")
        .await
        .expect("enable");

    feature_flags::migrate_chat_id(&db, BIG_CHAT_ID, -1_009_999_999_999)
        .await
        .expect("migrate");
    assert_eq!(
        feature_flags::find_one_for_groups(&db, BIG_CHAT_ID, "")
            .await
            .expect("find"),
        None
    );
    let moved = feature_flags::find_one_for_groups(&db, -1_009_999_999_999, "")
        .await
        .expect("find")
        .expect("row");
    assert_eq!(
        moved.chat_type, SUPERGROUP,
        "Go rewrites the chat type on migration"
    );

    feature_flags::delete_by_chat_id(&db, -1_009_999_999_999)
        .await
        .expect("delete");
    assert_eq!(
        feature_flags::find_one_for_groups(&db, -1_009_999_999_999, "")
            .await
            .expect("find"),
        None
    );
}

// ---------------------------------------------------------------------------
// Recap options
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_recap_option_reads_as_none() {
    let (_fixture, db) = database().await;
    assert_eq!(
        recap_options::find_one(&db, BIG_CHAT_ID)
            .await
            .expect("find"),
        None
    );
}

#[tokio::test]
async fn the_first_enable_path_materializes_public_mode_daily_rate_four_and_pin_false() {
    let (_fixture, db) = database().await;

    let created = recap_options::find_one_or_create(&db, BIG_CHAT_ID)
        .await
        .expect("create");

    assert_eq!(created.chat_id, BIG_CHAT_ID);
    assert_eq!(created.auto_recap_send_mode, 0);
    assert_eq!(created.send_mode(), Some(AutoRecapSendMode::Publicly));
    assert_eq!(created.auto_recap_rates_per_day, 4);
    assert!(!created.pin_auto_recap_message);
    assert_eq!(created.manual_recap_rate_per_seconds, 0);
    assert!(created.created_at > BIG_TIMESTAMP_MS);

    // The second call finds rather than recreates.
    let found = recap_options::find_one_or_create(&db, BIG_CHAT_ID)
        .await
        .expect("find");
    assert_eq!(found.id, created.id);
}

#[tokio::test]
async fn setting_send_mode_on_a_missing_row_leaves_the_other_values_at_schema_defaults() {
    let (_fixture, db) = database().await;

    recap_options::set_send_mode(
        &db,
        BIG_CHAT_ID,
        AutoRecapSendMode::OnlyPrivateSubscriptions,
    )
    .await
    .expect("set mode");

    let stored = recap_options::find_one(&db, BIG_CHAT_ID)
        .await
        .expect("find")
        .expect("row");
    assert_eq!(stored.auto_recap_send_mode, 1);
    assert_eq!(
        stored.auto_recap_rates_per_day, 0,
        "the mode-only path must not materialize the daily rate of four"
    );
    assert_eq!(stored.manual_recap_rate_per_seconds, 0);
    assert!(!stored.pin_auto_recap_message);
}

#[tokio::test]
async fn the_send_mode_and_pin_setters_round_trip() {
    let (_fixture, db) = database().await;

    for mode in [
        AutoRecapSendMode::OnlyPrivateSubscriptions,
        AutoRecapSendMode::Publicly,
        AutoRecapSendMode::OnlyPrivateSubscriptions,
    ] {
        recap_options::set_send_mode(&db, BIG_CHAT_ID, mode)
            .await
            .expect("set mode");
        assert_eq!(
            recap_options::find_one(&db, BIG_CHAT_ID)
                .await
                .expect("find")
                .expect("row")
                .send_mode(),
            Some(mode)
        );
    }

    recap_options::set_pin_enabled(&db, BIG_CHAT_ID)
        .await
        .expect("pin");
    assert!(
        recap_options::find_one(&db, BIG_CHAT_ID)
            .await
            .expect("find")
            .expect("row")
            .pin_auto_recap_message
    );

    recap_options::set_pin_disabled(&db, BIG_CHAT_ID)
        .await
        .expect("unpin");
    assert!(
        !recap_options::find_one(&db, BIG_CHAT_ID)
            .await
            .expect("find")
            .expect("row")
            .pin_auto_recap_message
    );
}

#[tokio::test]
async fn disabling_pin_on_a_missing_row_creates_the_first_enable_shape() {
    let (_fixture, db) = database().await;

    recap_options::set_pin_disabled(&db, BIG_CHAT_ID)
        .await
        .expect("unpin");

    let stored = recap_options::find_one(&db, BIG_CHAT_ID)
        .await
        .expect("find")
        .expect("row");
    assert_eq!(stored.auto_recap_send_mode, 0);
    assert_eq!(stored.auto_recap_rates_per_day, 4);
    assert!(!stored.pin_auto_recap_message);
}

#[tokio::test]
async fn the_daily_rate_setter_accepts_any_integer_exactly_like_go() {
    let (_fixture, db) = database().await;

    for rate in [
        2_i64,
        3,
        4,
        0,
        1,
        99,
        -7,
        i64::from(i32::MAX),
        i64::from(i32::MIN),
        // Immediately outside the 32-bit range in both directions, because Go's
        // int is 64-bit and ent maps field.Int to PostgreSQL bigint.
        i64::from(i32::MAX) + 1,
        i64::from(i32::MIN) - 1,
        i64::MAX,
        i64::MIN,
    ] {
        recap_options::set_rates_per_day(&db, BIG_CHAT_ID, rate)
            .await
            .unwrap_or_else(|error| panic!("rate {rate} must be accepted: {error}"));
        assert_eq!(
            recap_options::find_one(&db, BIG_CHAT_ID)
                .await
                .expect("find")
                .expect("row")
                .auto_recap_rates_per_day,
            rate
        );
    }
}

#[tokio::test]
async fn an_unknown_send_mode_beyond_32_bits_is_preserved_raw_without_error() {
    let (_fixture, db) = database().await;

    recap_options::find_one_or_create(&db, BIG_CHAT_ID)
        .await
        .expect("create");

    for stored in [
        2_i64,
        -1,
        i64::from(i32::MAX) + 1,
        i64::from(i32::MIN) - 1,
        i64::MAX,
        i64::MIN,
    ] {
        // A mode written by Go, or by a future release, must stay readable.
        sqlx::query(
            "UPDATE telegram_chat_recaps_options SET auto_recap_send_mode = $1 WHERE chat_id = $2",
        )
        .bind(stored)
        .bind(BIG_CHAT_ID)
        .execute(&db.pool)
        .await
        .expect("write a raw mode");

        let read = recap_options::find_one(&db, BIG_CHAT_ID)
            .await
            .unwrap_or_else(|error| panic!("mode {stored} must decode: {error}"))
            .expect("row");
        assert_eq!(
            read.auto_recap_send_mode, stored,
            "the raw column must survive untouched"
        );
        assert_eq!(
            read.send_mode(),
            None,
            "{stored} must not be coerced onto a known mode"
        );
    }
}

#[tokio::test]
async fn a_stored_manual_rate_overrides_the_configured_fallback_only_when_greater() {
    fn option(stored: i64) -> TelegramChatRecapsOptions {
        TelegramChatRecapsOptions {
            id: "00000000-0000-4000-8000-000000000000".to_owned(),
            chat_id: BIG_CHAT_ID,
            auto_recap_send_mode: 0,
            manual_recap_rate_per_seconds: stored,
            auto_recap_rates_per_day: 4,
            pin_auto_recap_message: false,
            created_at: BIG_TIMESTAMP_MS,
            updated_at: BIG_TIMESTAMP_MS,
        }
    }

    assert_eq!(recap_options::manual_rate_per_seconds(None, 300), 300);
    assert_eq!(
        recap_options::manual_rate_per_seconds(Some(&option(600)), 300),
        600,
        "a strictly greater stored rate wins"
    );
    assert_eq!(
        recap_options::manual_rate_per_seconds(Some(&option(300)), 300),
        300,
        "an equal stored rate does not override"
    );
    assert_eq!(
        recap_options::manual_rate_per_seconds(Some(&option(60)), 300),
        300,
        "a smaller stored rate never lowers the configured floor"
    );
    assert_eq!(
        recap_options::manual_rate_per_seconds(Some(&option(0)), 0),
        0
    );
}

#[tokio::test]
async fn recap_options_delete_and_migrate_touch_every_matching_row() {
    let (_fixture, db) = database().await;

    recap_options::find_one_or_create(&db, BIG_CHAT_ID)
        .await
        .expect("create");

    recap_options::migrate_chat_id(&db, BIG_CHAT_ID, -1_009_999_999_999)
        .await
        .expect("migrate");
    assert_eq!(
        recap_options::find_one(&db, BIG_CHAT_ID)
            .await
            .expect("find"),
        None
    );
    assert!(
        recap_options::find_one(&db, -1_009_999_999_999)
            .await
            .expect("find")
            .is_some()
    );

    recap_options::delete_by_chat_id(&db, -1_009_999_999_999)
        .await
        .expect("delete");
    assert_eq!(
        recap_options::find_one(&db, -1_009_999_999_999)
            .await
            .expect("find"),
        None
    );
}

// ---------------------------------------------------------------------------
// Auto-recap subscribers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn subscribing_is_sequentially_idempotent_without_a_unique_constraint() {
    let (_fixture, db) = database().await;

    for _ in 0..3 {
        subscribers::subscribe(&db, BIG_CHAT_ID, BIG_USER_ID)
            .await
            .expect("subscribe");
    }

    let rows = subscribers::list(&db, BIG_CHAT_ID).await.expect("list");
    assert_eq!(rows.len(), 1, "the LIMIT 1 precheck absorbs the repeats");
    assert_eq!(rows[0].user_id, BIG_USER_ID);
    assert!(
        subscribers::find_one(&db, BIG_CHAT_ID, BIG_USER_ID)
            .await
            .expect("find")
            .is_some()
    );
}

#[tokio::test]
async fn physical_duplicate_subscriber_rows_remain_possible() {
    let (_fixture, db) = database().await;

    // A concurrent racer bypasses the precheck; no constraint stops it.
    for _ in 0..3 {
        subscribers::insert_unchecked(&db, BIG_CHAT_ID, BIG_USER_ID)
            .await
            .expect("insert");
    }

    let rows = subscribers::list(&db, BIG_CHAT_ID).await.expect("list");
    assert_eq!(
        rows.len(),
        3,
        "duplicates are neither rejected nor collapsed"
    );
    assert!(rows.iter().all(|row| row.user_id == BIG_USER_ID));

    // A later subscribe still finds one and adds nothing.
    subscribers::subscribe(&db, BIG_CHAT_ID, BIG_USER_ID)
        .await
        .expect("subscribe");
    assert_eq!(
        subscribers::list(&db, BIG_CHAT_ID)
            .await
            .expect("list")
            .len(),
        3
    );
}

#[tokio::test]
async fn the_subscriber_list_is_neither_ordered_nor_deduplicated() {
    let (_fixture, db) = database().await;

    for user_id in [30_i64, 10, 20, 10] {
        subscribers::insert_unchecked(&db, BIG_CHAT_ID, user_id)
            .await
            .expect("insert");
    }

    // Ordering is undefined without an ORDER BY, but duplicate multiplicity is
    // part of the contract, so compare as a multiset.
    let mut listed: Vec<i64> = subscribers::list(&db, BIG_CHAT_ID)
        .await
        .expect("list")
        .into_iter()
        .map(|row| row.user_id)
        .collect();
    listed.sort_unstable();
    assert_eq!(listed, vec![10, 10, 20, 30], "nothing is deduplicated");
    assert_eq!(listed.len(), 4, "every physical row is returned");
}

#[tokio::test]
async fn unsubscribing_removes_exactly_one_physical_row() {
    let (_fixture, db) = database().await;

    for _ in 0..3 {
        subscribers::insert_unchecked(&db, BIG_CHAT_ID, BIG_USER_ID)
            .await
            .expect("insert");
    }
    subscribers::insert_unchecked(&db, BIG_CHAT_ID, BIG_USER_ID + 1)
        .await
        .expect("insert another subscriber");

    subscribers::unsubscribe(&db, BIG_CHAT_ID, BIG_USER_ID)
        .await
        .expect("unsubscribe");

    let remaining: Vec<i64> = subscribers::list(&db, BIG_CHAT_ID)
        .await
        .expect("list")
        .into_iter()
        .map(|row| row.user_id)
        .collect();
    assert_eq!(
        remaining.iter().filter(|id| **id == BIG_USER_ID).count(),
        2,
        "only one duplicate is removed per call"
    );
    assert!(
        remaining.contains(&(BIG_USER_ID + 1)),
        "other users are untouched"
    );

    // Unsubscribing an absent user is a no-op rather than an error.
    subscribers::unsubscribe(&db, BIG_CHAT_ID, 999_999)
        .await
        .expect("unsubscribe absent");
    assert_eq!(
        subscribers::list(&db, BIG_CHAT_ID)
            .await
            .expect("list")
            .len(),
        3
    );
}

#[tokio::test]
async fn deleting_all_subscribers_clears_only_the_named_chat() {
    let (_fixture, db) = database().await;

    for user_id in [1_i64, 2, 2] {
        subscribers::insert_unchecked(&db, BIG_CHAT_ID, user_id)
            .await
            .expect("insert");
    }
    subscribers::insert_unchecked(&db, -500, 1)
        .await
        .expect("insert into another chat");

    subscribers::delete_all_by_chat_id(&db, BIG_CHAT_ID)
        .await
        .expect("delete all");

    assert!(
        subscribers::list(&db, BIG_CHAT_ID)
            .await
            .expect("list")
            .is_empty()
    );
    assert_eq!(subscribers::list(&db, -500).await.expect("list").len(), 1);
}

#[tokio::test]
async fn migrating_subscribers_moves_every_physical_row() {
    let (_fixture, db) = database().await;

    for user_id in [1_i64, 2, 2] {
        subscribers::insert_unchecked(&db, BIG_CHAT_ID, user_id)
            .await
            .expect("insert");
    }

    subscribers::migrate_chat_id(&db, BIG_CHAT_ID, -1_009_999_999_999)
        .await
        .expect("migrate");

    assert!(
        subscribers::list(&db, BIG_CHAT_ID)
            .await
            .expect("list")
            .is_empty()
    );
    let mut moved: Vec<i64> = subscribers::list(&db, -1_009_999_999_999)
        .await
        .expect("list")
        .into_iter()
        .map(|row| row.user_id)
        .collect();
    moved.sort_unstable();
    assert_eq!(moved, vec![1, 2, 2], "duplicates move along with the rest");
}
