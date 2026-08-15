use std::sync::Arc;

use insights_bot_telegram_rs::{
    redis::{
        keys,
        recap_state::{InMemoryRecapStateStore, RecapStateStore, TestClock},
    },
    services::autorecap_queue::{
        AUTO_RECAP_POLL_INTERVAL, AUTO_RECAP_QUEUE_KEY, auto_recap_window_hours,
        decode_auto_recap_member, effective_auto_recap_rate, encode_auto_recap_member,
        enqueue_auto_recap, next_auto_recap_at_ms, pop_due_auto_recap,
    },
};

const START_MS: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z

fn store() -> (InMemoryRecapStateStore, Arc<TestClock>) {
    let clock = Arc::new(TestClock::new(START_MS));
    (InMemoryRecapStateStore::new(clock.clone()), clock)
}

#[test]
fn member_encoding_is_compact_deterministic_standard_base64() {
    assert_eq!(
        encode_auto_recap_member(42),
        "eyJwYXlsb2FkIjp7ImNoYXRfaWQiOjQyfX0="
    );
    assert_eq!(
        encode_auto_recap_member(-1_001_234_567_890),
        "eyJwYXlsb2FkIjp7ImNoYXRfaWQiOi0xMDAxMjM0NTY3ODkwfX0="
    );
}

#[test]
fn queue_contract_exposes_the_go_key_and_one_second_poll_interval() {
    assert_eq!(AUTO_RECAP_QUEUE_KEY, "time_capsule/auto_recap_capsules");
    assert_eq!(AUTO_RECAP_POLL_INTERVAL.as_millis(), 1_000);
}

#[test]
fn member_decoding_recovers_the_signed_chat_id() {
    let capsule = decode_auto_recap_member("eyJwYXlsb2FkIjp7ImNoYXRfaWQiOi0xMDAxMjM0NTY3ODkwfX0=")
        .expect("the pinned Go member decodes");

    assert_eq!(capsule.chat_id, -1_001_234_567_890);
    assert!(decode_auto_recap_member("not base64").is_err());
}

#[tokio::test]
async fn enqueue_uses_one_no_ttl_member_and_rescores_it() {
    let (state, _) = store();
    let member = enqueue_auto_recap(&state, 42, START_MS + 50_000)
        .await
        .expect("first enqueue succeeds");
    enqueue_auto_recap(&state, 42, START_MS + 90_000)
        .await
        .expect("rescore succeeds");

    assert_eq!(
        state.raw_zset(keys::AUTO_RECAP_QUEUE_KEY),
        Some(vec![(START_MS + 90_000, member)])
    );
    assert_eq!(
        state.ttl_ms(keys::AUTO_RECAP_QUEUE_KEY),
        Some(i64::MAX - START_MS)
    );
}

#[tokio::test]
async fn due_pop_consumes_the_member_before_another_call() {
    let (state, _) = store();
    enqueue_auto_recap(&state, -42, START_MS)
        .await
        .expect("enqueue succeeds");

    let capsule = pop_due_auto_recap(&state, START_MS)
        .await
        .expect("pop succeeds")
        .expect("one capsule is due");

    assert_eq!(capsule.chat_id, -42);
    assert!(
        pop_due_auto_recap(&state, START_MS)
            .await
            .expect("second pop succeeds")
            .is_none()
    );
}

#[tokio::test]
async fn a_future_member_is_restored_after_the_functional_pop_race() {
    let (state, _) = store();
    let future_ms = START_MS + 1;
    let member = encode_auto_recap_member(7);
    state
        .auto_recap_zadd(&member, future_ms)
        .await
        .expect("future enqueue succeeds");

    assert!(
        pop_due_auto_recap(&state, START_MS)
            .await
            .expect("nothing due is not an error")
            .is_none()
    );
    assert_eq!(
        state.raw_zset(keys::AUTO_RECAP_QUEUE_KEY),
        Some(vec![(future_ms, member)])
    );
}

#[tokio::test]
async fn malformed_due_member_is_removed_before_decode_fails() {
    let (state, _) = store();
    state
        .auto_recap_zadd("not-base64", START_MS)
        .await
        .expect("raw member is planted");

    assert!(pop_due_auto_recap(&state, START_MS).await.is_err());
    assert!(
        state
            .auto_recap_zpop_due(START_MS)
            .await
            .expect("the queue remains readable")
            .is_none()
    );
}

#[test]
fn schedule_uses_fixed_offset_and_exact_slot_sets() {
    // 2026-01-01T07:30:00Z is 15:30 at UTC+08. The next 3/day slot is
    // 16:00 local, which is 08:00 UTC.
    assert_eq!(
        next_auto_recap_at_ms(1_767_252_600_000, 8 * 3_600, 3),
        1_767_254_400_000
    );
    // 07:30 local comes before the 08:00 2/day slot.
    assert_eq!(
        next_auto_recap_at_ms(1_767_252_600_000, 0, 2),
        1_767_254_400_000
    );
    // 13:00 local comes before the 14:00 default 4/day slot.
    assert_eq!(
        next_auto_recap_at_ms(1_767_272_400_000, 0, 4),
        1_767_276_000_000
    );
}

#[test]
fn schedule_treats_the_entire_target_hour_as_passed() {
    // At exactly 08:00 local the 08:00 slot is already passed, so 2/day moves
    // to 20:00 on the same local day.
    assert_eq!(
        next_auto_recap_at_ms(1_767_254_400_000, 0, 2),
        1_767_297_600_000
    );
    // 20:59 local is past the final 4/day slot, so the next slot is 02:00 on
    // the following local day.
    assert_eq!(
        next_auto_recap_at_ms(1_767_301_140_000, 0, 4),
        1_767_319_200_000
    );
}

#[test]
fn invalid_rate_uses_four_without_mutating_external_state() {
    let configured_rate = 99;

    assert_eq!(effective_auto_recap_rate(configured_rate), 4);
    assert_eq!(configured_rate, 99);
    assert_eq!(
        next_auto_recap_at_ms(1_767_272_400_000, 0, configured_rate),
        1_767_276_000_000
    );
}

#[test]
fn message_windows_follow_the_effective_rate() {
    assert_eq!(auto_recap_window_hours(2), 12);
    assert_eq!(auto_recap_window_hours(3), 8);
    assert_eq!(auto_recap_window_hours(4), 6);
    assert_eq!(auto_recap_window_hours(-1), 6);
}
