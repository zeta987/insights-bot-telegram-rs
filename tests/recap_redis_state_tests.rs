//! Task 3 — Redis recap state and callback codec parity tests.
//!
//! Every literal in this file is pinned against Go v1.0.0 commit
//! `02aee8ce260165592e2152eb5a024a602e4eced1` as recorded in
//! `docs/parity/go-v1.0.0-rich-recap-ledger.md` rows `REDIS-002` through
//! `REDIS-006` and `REDIS-008`. SHA-256 digests below were computed
//! independently of the implementation under test.

mod support;

use std::sync::Arc;

use insights_bot_telegram_rs::redis::{
    keys::{self, StartContextDomain},
    recap_state::{
        CallbackResolution, CallbackRouteRegistry, InMemoryRecapStateStore, RecapStateStore,
        TestClock,
    },
};

/// Route literal to first-sixteen lowercase SHA-256 hex, computed out of band.
const ROUTE_HASHES: [(&str, &str); 9] = [
    ("recap/select-hour", "ab65affa2e72fdef"),
    ("recap/configure/toggle", "580daac9b77b24e8"),
    ("recap/configure/assign_mode", "51e7962f471bd70c"),
    ("recap/configure/complete", "8dc7dadaa64a3398"),
    ("recap/unsubscribe_recap", "4016f9ed68bc7638"),
    ("recap/recap/feedback/react", "972854b11a16c262"),
    (
        "recap/configure/auto_recap_rates_per_day",
        "b85a93fec884514b",
    ),
    ("recap/configure/pin", "6b1f8f12d4f5aadb"),
    ("smr/summarization/feedback/react", "636b18c9d6a0b580"),
];

const SAMPLE_CHAT_ID: i64 = -1_001_234_567_890;
const SAMPLE_PAYLOAD: &str = r#"{"chatId":-1001234567890,"hours":6}"#;
const SAMPLE_PAYLOAD_ACTION_HASH: &str = "0472918c0c2e0f2a";
const OTHER_PAYLOAD: &str = r#"{"chatId":-1001234567890,"hours":12}"#;
const OTHER_PAYLOAD_ACTION_HASH: &str = "73af6eccf05ef491";
const EMPTY_OBJECT_ACTION_HASH: &str = "44136fa355b3678a";

const START_MS: i64 = 1_700_000_000_000;

fn store() -> (InMemoryRecapStateStore, Arc<TestClock>) {
    let clock = Arc::new(TestClock::new(START_MS));
    (InMemoryRecapStateStore::new(clock.clone()), clock)
}

// ---------------------------------------------------------------------------
// Callback codec — REDIS-006 / CALLBACK-008
// ---------------------------------------------------------------------------

#[test]
fn registered_callback_routes_are_the_nine_go_literals() {
    let expected: Vec<&str> = ROUTE_HASHES.iter().map(|(route, _)| *route).collect();
    assert_eq!(keys::REGISTERED_CALLBACK_ROUTES.to_vec(), expected);
    assert!(
        keys::REGISTERED_CALLBACK_ROUTES.contains(&"smr/summarization/feedback/react"),
        "the summarization compatibility route must stay registered"
    );
}

#[test]
fn callback_route_hash_is_the_first_sixteen_lowercase_sha256_hex() {
    for (route, expected) in ROUTE_HASHES {
        let actual = keys::callback_route_hash(route);
        assert_eq!(actual, expected, "route hash mismatch for {route}");
        assert_eq!(actual.len(), keys::CALLBACK_ROUTE_HASH_HEX_LEN);
        assert!(
            actual
                .chars()
                .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase())
        );
    }
}

#[test]
fn callback_action_hash_is_the_first_sixteen_lowercase_sha256_hex_of_the_payload() {
    assert_eq!(
        keys::callback_action_hash(SAMPLE_PAYLOAD),
        SAMPLE_PAYLOAD_ACTION_HASH
    );
    assert_eq!(
        keys::callback_action_hash(OTHER_PAYLOAD),
        OTHER_PAYLOAD_ACTION_HASH
    );
    assert_eq!(keys::callback_action_hash("{}"), EMPTY_OBJECT_ACTION_HASH);
}

#[test]
fn callback_wire_value_is_route_hash_semicolon_action_hash() {
    assert_eq!(
        keys::callback_wire_value("recap/select-hour", SAMPLE_PAYLOAD),
        format!("ab65affa2e72fdef;{SAMPLE_PAYLOAD_ACTION_HASH}")
    );
}

#[test]
fn callback_payload_key_uses_the_literal_route_not_the_route_hash() {
    assert_eq!(
        keys::callback_payload_key("recap/select-hour", SAMPLE_PAYLOAD_ACTION_HASH),
        format!("callback_query/button_data/recap/select-hour/{SAMPLE_PAYLOAD_ACTION_HASH}")
    );
    assert_eq!(
        keys::callback_payload_key("smr/summarization/feedback/react", EMPTY_OBJECT_ACTION_HASH),
        format!(
            "callback_query/button_data/smr/summarization/feedback/react/{EMPTY_OBJECT_ACTION_HASH}"
        )
    );
}

#[tokio::test]
async fn put_callback_stores_raw_json_under_the_literal_route_key_with_a_day_ttl() {
    let (store, _clock) = store();

    let wire = store
        .put_callback("recap/select-hour", SAMPLE_PAYLOAD)
        .await
        .expect("put_callback");

    assert_eq!(
        wire,
        format!("ab65affa2e72fdef;{SAMPLE_PAYLOAD_ACTION_HASH}")
    );

    let key = keys::callback_payload_key("recap/select-hour", SAMPLE_PAYLOAD_ACTION_HASH);
    assert_eq!(store.raw_string(&key).as_deref(), Some(SAMPLE_PAYLOAD));
    assert_eq!(store.ttl_ms(&key), Some(86_400_000));
    assert_eq!(keys::CALLBACK_PAYLOAD_TTL_SECONDS, 86_400);
    assert_eq!(store.keys(), vec![key]);
}

#[tokio::test]
async fn get_callback_is_reusable_and_never_refreshes_the_ttl() {
    let (store, clock) = store();
    store
        .put_callback("recap/select-hour", SAMPLE_PAYLOAD)
        .await
        .expect("put_callback");
    let key = keys::callback_payload_key("recap/select-hour", SAMPLE_PAYLOAD_ACTION_HASH);

    clock.advance_ms(1_000);
    for _ in 0..3 {
        let payload = store
            .get_callback("recap/select-hour", SAMPLE_PAYLOAD_ACTION_HASH)
            .await
            .expect("get_callback");
        assert_eq!(payload.as_deref(), Some(SAMPLE_PAYLOAD));
    }

    assert_eq!(
        store.ttl_ms(&key),
        Some(86_400_000 - 1_000),
        "a GET must never extend the payload lifetime"
    );
}

#[tokio::test]
async fn callback_payload_expires_exactly_at_the_86400_second_boundary() {
    let (store, clock) = store();
    store
        .put_callback("recap/configure/pin", "{}")
        .await
        .expect("put_callback");

    clock.advance_ms(86_400_000 - 1);
    assert!(
        store
            .get_callback("recap/configure/pin", EMPTY_OBJECT_ACTION_HASH)
            .await
            .expect("get_callback")
            .is_some()
    );

    clock.advance_ms(1);
    assert_eq!(
        store
            .get_callback("recap/configure/pin", EMPTY_OBJECT_ACTION_HASH)
            .await
            .expect("get_callback"),
        None
    );
}

#[tokio::test]
async fn get_callback_for_a_never_written_action_is_missing() {
    let (store, _clock) = store();
    assert_eq!(
        store
            .get_callback("recap/select-hour", SAMPLE_PAYLOAD_ACTION_HASH)
            .await
            .expect("get_callback"),
        None
    );
}

#[test]
fn decode_callback_wire_accepts_exactly_two_semicolon_segments() {
    assert_eq!(
        keys::decode_callback_wire("ab65affa2e72fdef;0472918c0c2e0f2a"),
        Some(("ab65affa2e72fdef", "0472918c0c2e0f2a"))
    );
    assert_eq!(keys::decode_callback_wire(";"), Some(("", "")));

    for malformed in ["", "ab65affa2e72fdef", "a;b;c", "a;b;"] {
        assert_eq!(
            keys::decode_callback_wire(malformed),
            None,
            "{malformed} must be malformed"
        );
    }
}

#[test]
fn resolve_callback_route_maps_route_hashes_back_to_literals() {
    for (route, hash) in ROUTE_HASHES {
        assert_eq!(keys::resolve_callback_route(hash), Some(route));
    }
    assert_eq!(keys::resolve_callback_route("ffffffffffffffff"), None);
    assert_eq!(keys::resolve_callback_route(""), None);
}

#[tokio::test]
async fn malformed_wire_resolves_to_malformed() {
    let (store, _clock) = store();
    let registry = CallbackRouteRegistry::with_all_registered_routes();

    for malformed in ["", "ab65affa2e72fdef", "a;b;c"] {
        assert_eq!(
            registry.resolve(&store, malformed).await.expect("resolve"),
            CallbackResolution::Malformed,
            "{malformed} must resolve as malformed"
        );
    }
}

#[tokio::test]
async fn unknown_route_hash_resolves_to_unknown_route() {
    let (store, _clock) = store();
    let registry = CallbackRouteRegistry::with_all_registered_routes();

    assert_eq!(
        registry
            .resolve(&store, "ffffffffffffffff;0472918c0c2e0f2a")
            .await
            .expect("resolve"),
        CallbackResolution::UnknownRoute
    );
}

#[tokio::test]
async fn known_route_without_a_bound_handler_resolves_to_missing_handler() {
    let (store, _clock) = store();
    let mut registry = CallbackRouteRegistry::new();
    registry
        .bind("recap/select-hour")
        .expect("select-hour is a registered literal");

    let wire = store
        .put_callback("recap/configure/pin", "{}")
        .await
        .expect("put_callback");

    assert_eq!(
        registry.resolve(&store, &wire).await.expect("resolve"),
        CallbackResolution::MissingHandler {
            route: "recap/configure/pin"
        }
    );
}

#[test]
fn binding_an_unregistered_route_is_rejected() {
    let mut registry = CallbackRouteRegistry::new();
    assert!(registry.bind("recap/not-a-real-route").is_err());
    assert!(!registry.is_bound("recap/not-a-real-route"));
}

#[tokio::test]
async fn bound_route_resolves_to_the_stored_payload() {
    let (store, _clock) = store();
    let registry = CallbackRouteRegistry::with_all_registered_routes();
    let wire = store
        .put_callback("recap/select-hour", SAMPLE_PAYLOAD)
        .await
        .expect("put_callback");

    assert_eq!(
        registry.resolve(&store, &wire).await.expect("resolve"),
        CallbackResolution::Dispatch {
            route: "recap/select-hour",
            action_hash: SAMPLE_PAYLOAD_ACTION_HASH.to_owned(),
            payload_json: SAMPLE_PAYLOAD.to_owned(),
        }
    );
}

#[tokio::test]
async fn expired_callback_payload_still_reaches_the_handler_with_an_empty_payload() {
    let (store, clock) = store();
    let registry = CallbackRouteRegistry::with_all_registered_routes();
    let wire = store
        .put_callback("recap/select-hour", SAMPLE_PAYLOAD)
        .await
        .expect("put_callback");

    clock.advance_ms(86_400_000);

    assert_eq!(
        registry.resolve(&store, &wire).await.expect("resolve"),
        CallbackResolution::Dispatch {
            route: "recap/select-hour",
            action_hash: SAMPLE_PAYLOAD_ACTION_HASH.to_owned(),
            payload_json: String::new(),
        }
    );
}

// ---------------------------------------------------------------------------
// Start command contexts — REDIS-004 / REDIS-005
// ---------------------------------------------------------------------------

#[test]
fn start_context_tokens_use_the_first_eight_hex_of_the_go_source_strings() {
    assert_eq!(
        StartContextDomain::PrivateSubscription.token_source(SAMPLE_CHAT_ID),
        "recap/private_subscription_mode/start_command_context/-1001234567890"
    );
    assert_eq!(
        StartContextDomain::SubscribeRecap.token_source(SAMPLE_CHAT_ID),
        "recap/subscribe_recap/start_command_context/-1001234567890"
    );

    assert_eq!(
        StartContextDomain::PrivateSubscription.token(SAMPLE_CHAT_ID),
        "73e294e6"
    );
    assert_eq!(
        StartContextDomain::SubscribeRecap.token(SAMPLE_CHAT_ID),
        "8a82e9b4"
    );
    assert_eq!(
        StartContextDomain::PrivateSubscription.token(42),
        "0823190e"
    );
    assert_eq!(StartContextDomain::SubscribeRecap.token(42), "729dcd76");

    for domain in [
        StartContextDomain::PrivateSubscription,
        StartContextDomain::SubscribeRecap,
    ] {
        assert_eq!(
            domain.token(SAMPLE_CHAT_ID).len(),
            keys::START_CONTEXT_TOKEN_HEX_LEN
        );
    }
}

#[test]
fn the_private_token_source_prefix_deliberately_differs_from_its_redis_key_prefix() {
    assert_eq!(
        StartContextDomain::PrivateSubscription.token_source_prefix(),
        "recap/private_subscription_mode/start_command_context"
    );
    assert_eq!(
        StartContextDomain::PrivateSubscription.key_prefix(),
        "recap/private_subscription/start_command_context"
    );
    assert_ne!(
        StartContextDomain::PrivateSubscription.token_source_prefix(),
        StartContextDomain::PrivateSubscription.key_prefix(),
        "Go hashes the `_mode` variant but stores under the shorter prefix"
    );

    // A token derived from the key prefix must not equal the Go token.
    assert_ne!(
        StartContextDomain::PrivateSubscription.token(SAMPLE_CHAT_ID),
        "dfd764d7"
    );

    assert_eq!(
        StartContextDomain::SubscribeRecap.token_source_prefix(),
        StartContextDomain::SubscribeRecap.key_prefix(),
        "only the private domain carries the mismatch"
    );
}

#[test]
fn start_context_domains_use_distinct_redis_keys() {
    assert_eq!(
        StartContextDomain::PrivateSubscription.key("73e294e6"),
        "recap/private_subscription/start_command_context/73e294e6"
    );
    assert_eq!(
        StartContextDomain::SubscribeRecap.key("8a82e9b4"),
        "recap/subscribe_recap/start_command_context/8a82e9b4"
    );
    assert_ne!(
        StartContextDomain::PrivateSubscription.key("same-token"),
        StartContextDomain::SubscribeRecap.key("same-token")
    );
}

#[tokio::test]
async fn start_contexts_are_stored_per_domain_with_a_day_ttl_and_never_refresh() {
    let (store, clock) = store();
    let private = r#"{"chatId":-1001234567890,"mode":"private"}"#;
    let subscribe = r#"{"chatId":-1001234567890,"mode":"subscribe"}"#;

    store
        .put_start_context(StartContextDomain::PrivateSubscription, "73e294e6", private)
        .await
        .expect("put private start context");
    store
        .put_start_context(StartContextDomain::SubscribeRecap, "8a82e9b4", subscribe)
        .await
        .expect("put subscribe start context");

    let private_key = StartContextDomain::PrivateSubscription.key("73e294e6");
    let subscribe_key = StartContextDomain::SubscribeRecap.key("8a82e9b4");
    assert_eq!(store.raw_string(&private_key).as_deref(), Some(private));
    assert_eq!(store.raw_string(&subscribe_key).as_deref(), Some(subscribe));
    assert_eq!(store.ttl_ms(&private_key), Some(86_400_000));
    assert_eq!(store.ttl_ms(&subscribe_key), Some(86_400_000));
    assert_eq!(keys::START_CONTEXT_TTL_SECONDS, 86_400);

    clock.advance_ms(5_000);
    for _ in 0..2 {
        assert_eq!(
            store
                .get_start_context(StartContextDomain::PrivateSubscription, "73e294e6")
                .await
                .expect("get")
                .as_deref(),
            Some(private),
            "the context is reusable, never consumed"
        );
    }
    assert_eq!(store.ttl_ms(&private_key), Some(86_400_000 - 5_000));
}

#[tokio::test]
async fn a_start_context_token_is_scoped_to_its_own_domain() {
    let (store, _clock) = store();
    store
        .put_start_context(
            StartContextDomain::PrivateSubscription,
            "shared",
            "{\"a\":1}",
        )
        .await
        .expect("put");

    assert_eq!(
        store
            .get_start_context(StartContextDomain::SubscribeRecap, "shared")
            .await
            .expect("get"),
        None
    );
}

#[tokio::test]
async fn start_context_expires_exactly_at_the_86400_second_boundary() {
    let (store, clock) = store();
    store
        .put_start_context(StartContextDomain::SubscribeRecap, "8a82e9b4", "{}")
        .await
        .expect("put");

    clock.advance_ms(86_400_000 - 1);
    assert!(
        store
            .get_start_context(StartContextDomain::SubscribeRecap, "8a82e9b4")
            .await
            .expect("get")
            .is_some()
    );

    clock.advance_ms(1);
    assert_eq!(
        store
            .get_start_context(StartContextDomain::SubscribeRecap, "8a82e9b4")
            .await
            .expect("get"),
        None
    );
}

// ---------------------------------------------------------------------------
// Forwarded session — REDIS-002 / REDIS-003
// ---------------------------------------------------------------------------

const ACTOR: i64 = 777_000_111;

#[test]
fn forwarded_keys_match_the_go_literals() {
    assert_eq!(
        keys::forwarded_control_key(ACTOR),
        "recap/replay_from_private_message/777000111"
    );
    assert_eq!(
        keys::forwarded_batch_key(ACTOR),
        "recap/replay_from_private_message/777000111/batch"
    );
    assert_eq!(
        keys::forwarded_control_key(-42),
        "recap/replay_from_private_message/-42"
    );
    assert_eq!(keys::FORWARDED_SESSION_TTL_SECONDS, 7_200);
    assert_eq!(keys::FORWARDED_CONTROL_ACTIVE_VALUE, "1");
}

#[tokio::test]
async fn start_forwarded_sets_control_one_and_deletes_any_existing_batch() {
    let (store, _clock) = store();
    store.start_forwarded(ACTOR).await.expect("start");
    store
        .append_forwarded(ACTOR, 1_000, r#"{"t":"stale"}"#)
        .await
        .expect("append");
    assert_eq!(store.forwarded_batch(ACTOR).await.expect("batch").len(), 1);

    store.start_forwarded(ACTOR).await.expect("restart");

    let control_key = keys::forwarded_control_key(ACTOR);
    assert_eq!(store.raw_string(&control_key).as_deref(), Some("1"));
    assert_eq!(store.ttl_ms(&control_key), Some(7_200_000));
    assert!(store.forwarded_active(ACTOR).await.expect("active"));
    assert!(
        store
            .forwarded_batch(ACTOR)
            .await
            .expect("batch")
            .is_empty(),
        "restart must drop the previous batch"
    );
    assert_eq!(store.raw_zset(&keys::forwarded_batch_key(ACTOR)), None);
}

#[tokio::test]
async fn start_forwarded_keeps_an_orphan_batch_when_no_session_was_open() {
    let (store, _clock) = store();
    store.start_forwarded(ACTOR).await.expect("start");
    store
        .append_forwarded(ACTOR, 100, r#"{"t":"orphan"}"#)
        .await
        .expect("append");

    // Only the control key is lost, so the next start sees no ongoing session.
    store.expire_key_now(&keys::forwarded_control_key(ACTOR));
    store.start_forwarded(ACTOR).await.expect("restart");

    assert!(store.forwarded_active(ACTOR).await.expect("active"));
    assert_eq!(
        store.raw_zset(&keys::forwarded_batch_key(ACTOR)),
        Some(vec![(100, r#"{"t":"orphan"}"#.to_owned())]),
        "the batch is dropped only when a session was already open"
    );
}

#[tokio::test]
async fn forwarded_active_is_false_without_a_control_key() {
    let (store, _clock) = store();
    assert!(!store.forwarded_active(ACTOR).await.expect("active"));
}

#[tokio::test]
async fn append_forwarded_stores_json_members_with_unix_millisecond_scores() {
    let (store, _clock) = store();
    store.start_forwarded(ACTOR).await.expect("start");

    store
        .append_forwarded(ACTOR, 1_700_000_000_123, r#"{"t":"alpha"}"#)
        .await
        .expect("append");
    store
        .append_forwarded(ACTOR, 1_700_000_000_456, r#"{"t":"beta"}"#)
        .await
        .expect("append");

    let batch_key = keys::forwarded_batch_key(ACTOR);
    assert_eq!(
        store.raw_zset(&batch_key),
        Some(vec![
            (1_700_000_000_123, r#"{"t":"alpha"}"#.to_owned()),
            (1_700_000_000_456, r#"{"t":"beta"}"#.to_owned()),
        ])
    );
    assert_eq!(store.ttl_ms(&batch_key), Some(7_200_000));
}

#[tokio::test]
async fn append_forwarded_refreshes_both_session_ttls() {
    let (store, clock) = store();
    store.start_forwarded(ACTOR).await.expect("start");
    store
        .append_forwarded(ACTOR, 1_000, r#"{"t":"first"}"#)
        .await
        .expect("append");

    clock.advance_ms(3_600_000);
    assert_eq!(
        store.ttl_ms(&keys::forwarded_control_key(ACTOR)),
        Some(3_600_000)
    );

    store
        .append_forwarded(ACTOR, 2_000, r#"{"t":"second"}"#)
        .await
        .expect("append");

    assert_eq!(
        store.ttl_ms(&keys::forwarded_control_key(ACTOR)),
        Some(7_200_000)
    );
    assert_eq!(
        store.ttl_ms(&keys::forwarded_batch_key(ACTOR)),
        Some(7_200_000)
    );
}

#[tokio::test]
async fn forwarded_batch_reads_by_descending_score_then_reverses() {
    let (store, _clock) = store();
    store.start_forwarded(ACTOR).await.expect("start");

    // Inserted out of chronological order on purpose.
    store
        .append_forwarded(ACTOR, 300, r#"{"t":"third"}"#)
        .await
        .expect("append");
    store
        .append_forwarded(ACTOR, 100, r#"{"t":"first"}"#)
        .await
        .expect("append");
    store
        .append_forwarded(ACTOR, 200, r#"{"t":"second"}"#)
        .await
        .expect("append");

    assert_eq!(
        store.forwarded_batch(ACTOR).await.expect("batch"),
        vec![
            r#"{"t":"first"}"#.to_owned(),
            r#"{"t":"second"}"#.to_owned(),
            r#"{"t":"third"}"#.to_owned(),
        ]
    );
}

#[tokio::test]
async fn equal_scores_replay_in_redis_lexicographic_member_order() {
    let (store, _clock) = store();
    store.start_forwarded(ACTOR).await.expect("start");

    for member in [r#"{"t":"gamma"}"#, r#"{"t":"alpha"}"#, r#"{"t":"beta"}"#] {
        store
            .append_forwarded(ACTOR, 500, member)
            .await
            .expect("append");
    }

    // ZREVRANGE returns equal-score members in reverse lexicographic order;
    // reversing that yields ascending lexicographic order, not insertion order.
    assert_eq!(
        store.forwarded_batch(ACTOR).await.expect("batch"),
        vec![
            r#"{"t":"alpha"}"#.to_owned(),
            r#"{"t":"beta"}"#.to_owned(),
            r#"{"t":"gamma"}"#.to_owned(),
        ]
    );
}

#[tokio::test]
async fn appending_an_identical_member_updates_the_score_without_duplicating() {
    let (store, _clock) = store();
    store.start_forwarded(ACTOR).await.expect("start");

    store
        .append_forwarded(ACTOR, 100, r#"{"t":"same"}"#)
        .await
        .expect("append");
    store
        .append_forwarded(ACTOR, 900, r#"{"t":"same"}"#)
        .await
        .expect("append");
    store
        .append_forwarded(ACTOR, 500, r#"{"t":"other"}"#)
        .await
        .expect("append");

    assert_eq!(
        store.raw_zset(&keys::forwarded_batch_key(ACTOR)),
        Some(vec![
            (500, r#"{"t":"other"}"#.to_owned()),
            (900, r#"{"t":"same"}"#.to_owned()),
        ])
    );
    assert_eq!(
        store.forwarded_batch(ACTOR).await.expect("batch"),
        vec![r#"{"t":"other"}"#.to_owned(), r#"{"t":"same"}"#.to_owned()]
    );
}

#[tokio::test]
async fn forwarded_batch_of_an_absent_session_is_empty() {
    let (store, _clock) = store();
    assert!(
        store
            .forwarded_batch(ACTOR)
            .await
            .expect("batch")
            .is_empty()
    );
}

#[tokio::test]
async fn cancel_forwarded_clears_both_keys_only_while_the_control_is_active() {
    let (store, _clock) = store();
    store.start_forwarded(ACTOR).await.expect("start");
    store
        .append_forwarded(ACTOR, 100, r#"{"t":"only"}"#)
        .await
        .expect("append");

    assert!(store.cancel_forwarded(ACTOR).await.expect("cancel"));
    assert_eq!(store.raw_string(&keys::forwarded_control_key(ACTOR)), None);
    assert_eq!(store.raw_zset(&keys::forwarded_batch_key(ACTOR)), None);

    assert!(
        !store.cancel_forwarded(ACTOR).await.expect("cancel again"),
        "cancelling an inactive session reports already-cancelled"
    );
}

#[tokio::test]
async fn cancel_forwarded_retains_an_orphan_batch_when_the_control_is_gone() {
    let (store, _clock) = store();
    store.start_forwarded(ACTOR).await.expect("start");
    store
        .append_forwarded(ACTOR, 100, r#"{"t":"orphan"}"#)
        .await
        .expect("append");

    // Expire only the control key, leaving the batch behind.
    store.expire_key_now(&keys::forwarded_control_key(ACTOR));

    assert!(!store.cancel_forwarded(ACTOR).await.expect("cancel"));
    assert_eq!(
        store.raw_zset(&keys::forwarded_batch_key(ACTOR)),
        Some(vec![(100, r#"{"t":"orphan"}"#.to_owned())]),
        "an orphan batch is left in place"
    );
}

#[tokio::test]
async fn a_control_key_holding_any_other_value_is_not_an_active_session() {
    let (store, _clock) = store();
    store.set_raw_string(
        &keys::forwarded_control_key(ACTOR),
        "0",
        keys::FORWARDED_SESSION_TTL_SECONDS,
    );
    store
        .append_forwarded(ACTOR, 100, r#"{"t":"orphan"}"#)
        .await
        .expect("append");

    assert!(
        !store.forwarded_active(ACTOR).await.expect("active"),
        "only the literal control value 1 opens a session"
    );
    assert!(!store.cancel_forwarded(ACTOR).await.expect("cancel"));
    assert_eq!(
        store.raw_zset(&keys::forwarded_batch_key(ACTOR)),
        Some(vec![(100, r#"{"t":"orphan"}"#.to_owned())]),
        "an inactive control must not take the batch with it"
    );
}

#[tokio::test]
async fn a_successful_forwarded_recap_retains_the_session_and_batch() {
    let (store, _clock) = store();
    store.start_forwarded(ACTOR).await.expect("start");
    store
        .append_forwarded(ACTOR, 100, r#"{"t":"kept"}"#)
        .await
        .expect("append");

    let replayed = store.forwarded_batch(ACTOR).await.expect("batch");
    assert_eq!(replayed, vec![r#"{"t":"kept"}"#.to_owned()]);

    // Reading the batch is not a consuming operation.
    assert!(store.forwarded_active(ACTOR).await.expect("active"));
    assert_eq!(store.forwarded_batch(ACTOR).await.expect("batch"), replayed);
}

#[tokio::test]
async fn forwarded_session_keys_expire_exactly_at_the_7200_second_boundary() {
    let (store, clock) = store();
    store.start_forwarded(ACTOR).await.expect("start");
    store
        .append_forwarded(ACTOR, 100, r#"{"t":"expiring"}"#)
        .await
        .expect("append");

    clock.advance_ms(7_200_000 - 1);
    assert!(store.forwarded_active(ACTOR).await.expect("active"));
    assert_eq!(store.forwarded_batch(ACTOR).await.expect("batch").len(), 1);

    clock.advance_ms(1);
    assert!(!store.forwarded_active(ACTOR).await.expect("active"));
    assert!(
        store
            .forwarded_batch(ACTOR)
            .await
            .expect("batch")
            .is_empty()
    );
}

#[tokio::test]
async fn forwarded_sessions_are_isolated_per_actor() {
    let (store, _clock) = store();
    store.start_forwarded(ACTOR).await.expect("start");
    store
        .append_forwarded(ACTOR, 100, r#"{"t":"mine"}"#)
        .await
        .expect("append");

    assert!(!store.forwarded_active(ACTOR + 1).await.expect("active"));
    assert!(
        store
            .forwarded_batch(ACTOR + 1)
            .await
            .expect("batch")
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// Delete-later list — REDIS-008
// ---------------------------------------------------------------------------

#[test]
fn delete_later_key_and_member_match_the_go_literals() {
    assert_eq!(
        keys::delete_later_key(ACTOR),
        "session/delete_later_messages_for_actor/777000111"
    );
    assert_eq!(
        keys::delete_later_member(-1_001_234_567_890, 4_242),
        "-1001234567890;4242"
    );
    assert_eq!(keys::DELETE_LATER_TTL_SECONDS, 86_400);
}

#[test]
fn parse_delete_later_member_filters_malformed_values() {
    assert_eq!(
        keys::parse_delete_later_member("-1001234567890;4242"),
        Some((-1_001_234_567_890, 4_242))
    );

    for malformed in [
        "",
        "-1001234567890",
        "-1001234567890;",
        ";4242",
        "-1001234567890;4242;7",
        "not-a-chat;4242",
        "-1001234567890;not-a-message",
        "-1001234567890;99999999999",
        // Go skips a parsed pair whose chat or message identifier is zero.
        "0;4242",
        "-1001234567890;0",
        "0;0",
    ] {
        assert_eq!(
            keys::parse_delete_later_member(malformed),
            None,
            "{malformed} must be filtered"
        );
    }
}

#[tokio::test]
async fn push_delete_later_ignores_zero_identifiers() {
    let (store, _clock) = store();

    for (user_id, chat_id, message_id) in [(0, -100, 1), (ACTOR, 0, 1), (ACTOR, -100, 0)] {
        store
            .push_delete_later(user_id, chat_id, message_id)
            .await
            .expect("push");
        assert_eq!(
            store.raw_list(&keys::delete_later_key(user_id)),
            None,
            "a zero identifier must not create the list"
        );
    }

    assert!(store.keys().is_empty());
}

#[tokio::test]
async fn push_delete_later_lpushes_the_member_and_refreshes_the_day_ttl() {
    let (store, clock) = store();
    store
        .push_delete_later(ACTOR, -1_001_234_567_890, 11)
        .await
        .expect("push");

    let key = keys::delete_later_key(ACTOR);
    assert_eq!(
        store.raw_list(&key),
        Some(vec!["-1001234567890;11".to_owned()])
    );
    assert_eq!(store.ttl_ms(&key), Some(86_400_000));

    clock.advance_ms(60_000);
    store
        .push_delete_later(ACTOR, -1_001_234_567_890, 12)
        .await
        .expect("push");

    assert_eq!(
        store.raw_list(&key),
        Some(vec![
            "-1001234567890;12".to_owned(),
            "-1001234567890;11".to_owned(),
        ]),
        "LPUSH prepends the newest member"
    );
    assert_eq!(store.ttl_ms(&key), Some(86_400_000), "the TTL is refreshed");
}

#[tokio::test]
async fn the_delete_later_list_is_shared_per_actor_across_chats() {
    let (store, _clock) = store();
    store.push_delete_later(ACTOR, -100, 1).await.expect("push");
    store.push_delete_later(ACTOR, -200, 2).await.expect("push");

    assert_eq!(
        store.drain_delete_later(ACTOR).await.expect("drain"),
        vec![(-200, 2), (-100, 1)]
    );
}

#[tokio::test]
async fn drain_delete_later_deletes_the_key_before_returning_the_pairs() {
    let (store, _clock) = store();
    store.push_delete_later(ACTOR, -100, 1).await.expect("push");

    let key = keys::delete_later_key(ACTOR);
    let drained = store.drain_delete_later(ACTOR).await.expect("drain");

    assert_eq!(drained, vec![(-100, 1)]);
    assert_eq!(
        store.raw_list(&key),
        None,
        "Redis state is cleared before the best-effort Telegram deletions run"
    );
    assert!(
        store
            .drain_delete_later(ACTOR)
            .await
            .expect("drain again")
            .is_empty(),
        "a redelivery must not retry the same messages"
    );
}

#[tokio::test]
async fn drain_delete_later_filters_malformed_members_but_still_clears_the_key() {
    let (store, _clock) = store();
    store.push_delete_later(ACTOR, -100, 1).await.expect("push");
    store.push_raw_delete_later_member(ACTOR, "garbage");
    store.push_raw_delete_later_member(ACTOR, "-200;not-a-message");

    let drained = store.drain_delete_later(ACTOR).await.expect("drain");

    assert_eq!(drained, vec![(-100, 1)]);
    assert_eq!(store.raw_list(&keys::delete_later_key(ACTOR)), None);
}

#[tokio::test]
async fn delete_later_expires_exactly_at_the_86400_second_boundary() {
    let (store, clock) = store();
    store.push_delete_later(ACTOR, -100, 1).await.expect("push");

    clock.advance_ms(86_400_000 - 1);
    assert_eq!(
        store.drain_delete_later(ACTOR).await.expect("drain").len(),
        1
    );

    store.push_delete_later(ACTOR, -100, 2).await.expect("push");
    clock.advance_ms(86_400_000);
    assert!(
        store
            .drain_delete_later(ACTOR)
            .await
            .expect("drain")
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// Real Redis backend — skipped when no loopback server answers.
// ---------------------------------------------------------------------------

/// Startup now aborts on a Redis failure, so the propagated error is the only
/// thing an operator sees. It must stay free of connection details.
#[tokio::test]
async fn a_failed_connection_reports_nothing_about_the_endpoint_or_credentials() {
    use insights_bot_telegram_rs::redis::recap_state::RedisRecapStateStore;
    use support::redis_fixture::{SENTINEL_CREDENTIAL, SENTINEL_USERNAME, UNPARSEABLE_HOST};

    let error =
        RedisRecapStateStore::connect(&support::redis_fixture::unparseable_address_test_config())
            .await
            .err()
            .expect("an unparseable address must fail to connect");
    let rendered = format!("{error:?}");

    for leak in [
        UNPARSEABLE_HOST,
        SENTINEL_USERNAME,
        SENTINEL_CREDENTIAL,
        "redis://",
        "rediss://",
        "6379",
    ] {
        assert!(
            !rendered.contains(leak),
            "the connection error leaked {leak}: {rendered}"
        );
    }
    assert!(
        rendered.contains("recap Redis"),
        "the error must still name the failing operation: {rendered}"
    );
}

#[tokio::test]
async fn redis_backend_matches_the_in_memory_double() {
    let Some(store) = support::redis_fixture::connect().await else {
        eprintln!("skipping: no loopback Redis on 127.0.0.1:6379");
        return;
    };

    let actor = support::redis_fixture::unique_actor_id();
    let payload = support::redis_fixture::unique_payload_json();
    let action_hash = keys::callback_action_hash(&payload);

    // Callback payload lifecycle.
    let wire = store
        .put_callback("recap/select-hour", &payload)
        .await
        .expect("put_callback");
    assert_eq!(wire, format!("ab65affa2e72fdef;{action_hash}"));
    let callback_key = keys::callback_payload_key("recap/select-hour", &action_hash);
    assert_eq!(
        store.raw_string(&callback_key).await.expect("raw"),
        Some(payload.clone())
    );
    assert_eq!(
        store.ttl_seconds(&callback_key).await.expect("ttl"),
        Some(86_400)
    );
    assert_eq!(
        store
            .get_callback("recap/select-hour", &action_hash)
            .await
            .expect("get"),
        Some(payload.clone())
    );
    assert_eq!(
        store.ttl_seconds(&callback_key).await.expect("ttl"),
        Some(86_400),
        "a GET must not refresh the TTL"
    );

    // Forwarded session lifecycle, including equal-score ordering.
    store.start_forwarded(actor).await.expect("start");
    assert!(store.forwarded_active(actor).await.expect("active"));
    for member in [r#"{"t":"gamma"}"#, r#"{"t":"alpha"}"#, r#"{"t":"beta"}"#] {
        store
            .append_forwarded(actor, 500, member)
            .await
            .expect("append");
    }
    store
        .append_forwarded(actor, 100, r#"{"t":"earliest"}"#)
        .await
        .expect("append");
    assert_eq!(
        store.forwarded_batch(actor).await.expect("batch"),
        vec![
            r#"{"t":"earliest"}"#.to_owned(),
            r#"{"t":"alpha"}"#.to_owned(),
            r#"{"t":"beta"}"#.to_owned(),
            r#"{"t":"gamma"}"#.to_owned(),
        ]
    );
    assert_eq!(
        store
            .ttl_seconds(&keys::forwarded_batch_key(actor))
            .await
            .expect("ttl"),
        Some(7_200)
    );
    assert!(store.cancel_forwarded(actor).await.expect("cancel"));
    assert!(!store.cancel_forwarded(actor).await.expect("cancel again"));

    // Delete-later lifecycle.
    store.push_delete_later(actor, -100, 1).await.expect("push");
    store.push_delete_later(actor, -200, 2).await.expect("push");
    assert_eq!(
        store
            .ttl_seconds(&keys::delete_later_key(actor))
            .await
            .expect("ttl"),
        Some(86_400)
    );
    assert_eq!(
        store.drain_delete_later(actor).await.expect("drain"),
        vec![(-200, 2), (-100, 1)]
    );
    assert!(
        store
            .drain_delete_later(actor)
            .await
            .expect("drain again")
            .is_empty()
    );

    // Start-context lifecycle.
    let token = StartContextDomain::SubscribeRecap.token(actor);
    store
        .put_start_context(StartContextDomain::SubscribeRecap, &token, &payload)
        .await
        .expect("put start context");
    assert_eq!(
        store
            .get_start_context(StartContextDomain::SubscribeRecap, &token)
            .await
            .expect("get start context"),
        Some(payload.clone())
    );
    assert_eq!(
        store
            .get_start_context(StartContextDomain::PrivateSubscription, &token)
            .await
            .expect("get other domain"),
        None
    );

    store
        .delete_keys(&[
            callback_key,
            StartContextDomain::SubscribeRecap.key(&token),
            keys::forwarded_control_key(actor),
            keys::forwarded_batch_key(actor),
            keys::delete_later_key(actor),
        ])
        .await
        .expect("cleanup");
}
