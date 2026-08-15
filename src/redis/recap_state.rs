//! Recap state storage: the [`RecapStateStore`] abstraction, its production
//! Redis backend, and a deterministic in-memory double.
//!
//! Command sequences mirror Go v1.0.0 commit
//! `02aee8ce260165592e2152eb5a024a602e4eced1` verbatim, including the places
//! where Go issues several independent commands instead of a transaction. Those
//! sequences stay non-atomic on purpose: making them atomic would change the
//! observable interleaving that the released bot already exhibits.

use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use redis::IntoConnectionInfo;

use crate::{
    config::RedisConfig,
    redis::keys::{self, StartContextDomain},
};

/// Millisecond time source, so expiry can be driven deterministically in tests.
pub trait Clock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> i64;
}

/// Delete-later members plus a Redis `DEL` failure that occurred afterwards.
///
/// Go attempts Telegram deletions even when the Redis key deletion failed, and
/// only reports that failure after iterating every member.
pub struct DeleteLaterDrain {
    pub messages: Vec<(i64, i32)>,
    pub delete_error: Option<anyhow::Error>,
}

/// Wall-clock time source used in production.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
}

/// Manually advanced clock for expiry-boundary tests.
#[derive(Debug)]
pub struct TestClock {
    now_ms: AtomicI64,
}

impl TestClock {
    /// A clock frozen at `start_ms`.
    pub fn new(start_ms: i64) -> Self {
        Self {
            now_ms: AtomicI64::new(start_ms),
        }
    }

    /// Move time forward by `delta_ms`.
    pub fn advance_ms(&self, delta_ms: i64) {
        self.now_ms.fetch_add(delta_ms, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.now_ms.load(Ordering::SeqCst)
    }
}

/// Recap state persisted outside the process for the lifetime of a session.
///
/// The automatic TimeCapsule queue is intentionally not part of this trait; it
/// arrives with Task 13.
#[async_trait]
pub trait RecapStateStore: Send + Sync {
    /// Store a callback payload and return its `<route-hash>;<action-hash>`
    /// wire value.
    async fn put_callback(&self, route: &str, payload_json: &str) -> Result<String>;

    /// Read a stored callback payload without consuming it or extending its
    /// lifetime.
    async fn get_callback(&self, route: &str, action_hash: &str) -> Result<Option<String>>;

    /// Apply Go's non-atomic `GET`, `TTL`, `SET EX` command limiter for
    /// public manual recaps.
    async fn check_manual_recap_rate(
        &self,
        chat_id: i64,
        rate: i64,
        per_seconds: i64,
    ) -> Result<ManualRecapRateResult>;

    /// Store a `/start` deep-link context under `domain`.
    async fn put_start_context(
        &self,
        domain: StartContextDomain,
        token: &str,
        json: &str,
    ) -> Result<()>;

    /// Read a `/start` deep-link context without consuming it.
    async fn get_start_context(
        &self,
        domain: StartContextDomain,
        token: &str,
    ) -> Result<Option<String>>;

    /// Whether a forwarded-replay session is currently open for `user_id`.
    async fn forwarded_active(&self, user_id: i64) -> Result<bool>;

    /// Open a forwarded-replay session, discarding any previous batch.
    async fn start_forwarded(&self, user_id: i64) -> Result<()>;

    /// Append one forwarded message to the replay batch.
    async fn append_forwarded(&self, user_id: i64, score_ms: i64, json: &str) -> Result<()>;

    /// The forwarded batch in replay order.
    async fn forwarded_batch(&self, user_id: i64) -> Result<Vec<String>>;

    /// Cancel an open session. Reports whether a session was actually open.
    async fn cancel_forwarded(&self, user_id: i64) -> Result<bool>;

    /// Remember a message to delete once the session finishes.
    async fn push_delete_later(&self, user_id: i64, chat_id: i64, message_id: i32) -> Result<()>;

    /// Clear the delete-later list and return its well-formed members.
    async fn drain_delete_later(&self, user_id: i64) -> Result<Vec<(i64, i32)>>;

    /// Return members even when clearing their Redis key failed afterwards.
    async fn drain_delete_later_for_delivery(&self, user_id: i64) -> Result<DeleteLaterDrain> {
        Ok(DeleteLaterDrain {
            messages: self.drain_delete_later(user_id).await?,
            delete_error: None,
        })
    }

    /// Add or rescore one deterministic automatic-recap queue member.
    async fn auto_recap_zadd(&self, _member: &str, _score_ms: i64) -> Result<()> {
        Err(anyhow!("automatic recap queue is unavailable"))
    }

    /// Run the timecapsule/v2 due-check and functional minimum-pop sequence.
    async fn auto_recap_zpop_due(&self, _now_ms: i64) -> Result<Option<String>> {
        Err(anyhow!("automatic recap queue is unavailable"))
    }

    /// Remove a queue member. This is intentionally safe after a successful pop.
    async fn auto_recap_zrem(&self, _member: &str) -> Result<()> {
        Err(anyhow!("automatic recap queue is unavailable"))
    }
}

/// Result of Go's manual-recap command rate check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManualRecapRateResult {
    pub counted_rate: i64,
    pub ttl_seconds: i64,
    pub allowed: bool,
}

// ---------------------------------------------------------------------------
// Callback routing
// ---------------------------------------------------------------------------

/// Outcome of mapping an inline-button wire value back to a handler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallbackResolution {
    /// The wire value was not exactly two semicolon-delimited segments.
    Malformed,
    /// The route hash matches none of the registered literals.
    UnknownRoute,
    /// The route is registered but no handler is bound to it.
    MissingHandler {
        /// The registered route literal.
        route: &'static str,
    },
    /// A bound handler should run.
    ///
    /// `payload_json` is empty when the stored payload has already expired: Go
    /// still dispatches, letting the handler decide how to react.
    Dispatch {
        /// The registered route literal.
        route: &'static str,
        /// The action hash carried by the wire value.
        action_hash: String,
        /// The stored payload, or empty when it expired.
        payload_json: String,
    },
}

/// The set of callback routes that currently have a handler bound.
#[derive(Clone, Debug, Default)]
pub struct CallbackRouteRegistry {
    bound: BTreeSet<&'static str>,
}

impl CallbackRouteRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry with every registered literal bound.
    pub fn with_all_registered_routes() -> Self {
        Self {
            bound: keys::REGISTERED_CALLBACK_ROUTES.into_iter().collect(),
        }
    }

    /// Bind a handler to a registered route literal.
    ///
    /// Binding an unregistered literal is a programming error rather than a
    /// runtime condition, because the literal is part of the Telegram wire
    /// format.
    pub fn bind(&mut self, route: &str) -> Result<&'static str> {
        let literal = keys::REGISTERED_CALLBACK_ROUTES
            .into_iter()
            .find(|registered| *registered == route)
            .ok_or_else(|| anyhow!("callback route is not registered"))?;
        self.bound.insert(literal);
        Ok(literal)
    }

    /// Whether `route` has a handler bound.
    pub fn is_bound(&self, route: &str) -> bool {
        self.bound.contains(route)
    }

    /// Map an inline-button wire value onto a handler decision.
    pub async fn resolve<S>(&self, store: &S, wire: &str) -> Result<CallbackResolution>
    where
        S: RecapStateStore + ?Sized,
    {
        let Some((route_hash, action_hash)) = keys::decode_callback_wire(wire) else {
            return Ok(CallbackResolution::Malformed);
        };
        let Some(route) = keys::resolve_callback_route(route_hash) else {
            return Ok(CallbackResolution::UnknownRoute);
        };
        if !self.is_bound(route) {
            return Ok(CallbackResolution::MissingHandler { route });
        }
        let payload_json = store
            .get_callback(route, action_hash)
            .await?
            .unwrap_or_default();
        Ok(CallbackResolution::Dispatch {
            route,
            action_hash: action_hash.to_owned(),
            payload_json,
        })
    }
}

// ---------------------------------------------------------------------------
// In-memory double
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Value {
    Str(String),
    /// Member to score, matching `ZADD` upsert semantics.
    ZSet(HashMap<String, i64>),
    List(VecDeque<String>),
}

#[derive(Debug)]
struct Entry {
    value: Value,
    expires_at_ms: i64,
}

/// In-memory [`RecapStateStore`] with Redis-equivalent ordering and TTL rules.
///
/// Expiry is evaluated as `now >= expires_at`, so a key set with a 86,400-second
/// TTL is already gone at exactly 86,400,000 milliseconds.
pub struct InMemoryRecapStateStore {
    clock: Arc<dyn Clock>,
    state: Mutex<HashMap<String, Entry>>,
}

impl InMemoryRecapStateStore {
    /// A store driven by `clock`.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            state: Mutex::new(HashMap::new()),
        }
    }

    fn locked(&self) -> (i64, std::sync::MutexGuard<'_, HashMap<String, Entry>>) {
        let now = self.clock.now_ms();
        let mut guard = self
            .state
            .lock()
            .expect("recap state mutex is never poisoned");
        guard.retain(|_, entry| now < entry.expires_at_ms);
        (now, guard)
    }

    /// Every live key, sorted.
    pub fn keys(&self) -> Vec<String> {
        let (_, guard) = self.locked();
        let mut keys: Vec<String> = guard.keys().cloned().collect();
        keys.sort();
        keys
    }

    /// The live string stored at `key`.
    pub fn raw_string(&self, key: &str) -> Option<String> {
        let (_, guard) = self.locked();
        match guard.get(key) {
            Some(Entry {
                value: Value::Str(value),
                ..
            }) => Some(value.clone()),
            _ => None,
        }
    }

    /// `SET key value EX ttl_seconds`, bypassing the trait.
    ///
    /// The seam exists so tests can plant a value the production code never
    /// writes, such as a forwarded control key holding something other than `1`.
    pub fn set_raw_string(&self, key: &str, value: &str, ttl_seconds: i64) {
        let (now, mut guard) = self.locked();
        set_string(&mut guard, now, key, value, ttl_seconds);
    }

    /// The live sorted set at `key`, ordered by ascending score then member.
    pub fn raw_zset(&self, key: &str) -> Option<Vec<(i64, String)>> {
        let (_, guard) = self.locked();
        match guard.get(key) {
            Some(Entry {
                value: Value::ZSet(members),
                ..
            }) => {
                let mut ordered: Vec<(i64, String)> = members
                    .iter()
                    .map(|(member, score)| (*score, member.clone()))
                    .collect();
                ordered.sort();
                Some(ordered)
            }
            _ => None,
        }
    }

    /// The live list at `key`, head first.
    pub fn raw_list(&self, key: &str) -> Option<Vec<String>> {
        let (_, guard) = self.locked();
        match guard.get(key) {
            Some(Entry {
                value: Value::List(members),
                ..
            }) => Some(members.iter().cloned().collect()),
            _ => None,
        }
    }

    /// Milliseconds left before `key` expires.
    pub fn ttl_ms(&self, key: &str) -> Option<i64> {
        let (now, guard) = self.locked();
        guard.get(key).map(|entry| entry.expires_at_ms - now)
    }

    /// Force `key` to expire immediately, simulating a lost key.
    pub fn expire_key_now(&self, key: &str) {
        let (now, mut guard) = self.locked();
        if let Some(entry) = guard.get_mut(key) {
            entry.expires_at_ms = now;
        }
    }

    /// `LPUSH` an unvalidated delete-later member, simulating foreign writers.
    pub fn push_raw_delete_later_member(&self, user_id: i64, raw: &str) {
        let (now, mut guard) = self.locked();
        push_list_front(
            &mut guard,
            now,
            &keys::delete_later_key(user_id),
            raw,
            keys::DELETE_LATER_TTL_SECONDS,
        );
    }
}

fn ttl_deadline(now_ms: i64, ttl_seconds: i64) -> i64 {
    now_ms + ttl_seconds * 1_000
}

fn set_string(
    state: &mut HashMap<String, Entry>,
    now_ms: i64,
    key: &str,
    value: &str,
    ttl_seconds: i64,
) {
    state.insert(
        key.to_owned(),
        Entry {
            value: Value::Str(value.to_owned()),
            expires_at_ms: ttl_deadline(now_ms, ttl_seconds),
        },
    );
}

fn push_list_front(
    state: &mut HashMap<String, Entry>,
    now_ms: i64,
    key: &str,
    member: &str,
    ttl_seconds: i64,
) {
    let entry = state.entry(key.to_owned()).or_insert_with(|| Entry {
        value: Value::List(VecDeque::new()),
        expires_at_ms: ttl_deadline(now_ms, ttl_seconds),
    });
    if let Value::List(members) = &mut entry.value {
        members.push_front(member.to_owned());
    }
    // `EXPIRE` is issued after the push, so the deadline is always refreshed.
    entry.expires_at_ms = ttl_deadline(now_ms, ttl_seconds);
}

/// Whether Go would record this message for later deletion at all.
///
/// A zero actor, chat, or message identifier makes the pair unusable, and Go
/// returns before touching Redis.
fn is_pushable_delete_later(user_id: i64, chat_id: i64, message_id: i32) -> bool {
    user_id != 0 && chat_id != 0 && message_id != 0
}

fn refresh_expiry(state: &mut HashMap<String, Entry>, now_ms: i64, key: &str, ttl_seconds: i64) {
    // Redis `EXPIRE` on a missing key is a no-op rather than a creation.
    if let Some(entry) = state.get_mut(key) {
        entry.expires_at_ms = ttl_deadline(now_ms, ttl_seconds);
    }
}

#[async_trait]
impl RecapStateStore for InMemoryRecapStateStore {
    async fn put_callback(&self, route: &str, payload_json: &str) -> Result<String> {
        let action_hash = keys::callback_action_hash(payload_json);
        let key = keys::callback_payload_key(route, &action_hash);
        let (now, mut guard) = self.locked();
        set_string(
            &mut guard,
            now,
            &key,
            payload_json,
            keys::CALLBACK_PAYLOAD_TTL_SECONDS,
        );
        Ok(keys::callback_wire_value(route, payload_json))
    }

    async fn get_callback(&self, route: &str, action_hash: &str) -> Result<Option<String>> {
        Ok(self.raw_string(&keys::callback_payload_key(route, action_hash)))
    }

    async fn check_manual_recap_rate(
        &self,
        chat_id: i64,
        rate: i64,
        per_seconds: i64,
    ) -> Result<ManualRecapRateResult> {
        if per_seconds <= 0 {
            return Ok(ManualRecapRateResult {
                counted_rate: 0,
                ttl_seconds: 0,
                allowed: true,
            });
        }

        let key = keys::manual_recap_rate_key(chat_id);
        let (now, mut guard) = self.locked();
        let (mut counted_rate, ttl_seconds) = match guard.get(&key) {
            Some(Entry {
                value: Value::Str(value),
                expires_at_ms,
            }) => (
                value
                    .parse::<i64>()
                    .map_err(|_| anyhow!("recap Redis GET failed (TypeError)"))?,
                (*expires_at_ms - now) / 1_000,
            ),
            _ => (0, -2),
        };
        if counted_rate >= rate {
            return Ok(ManualRecapRateResult {
                counted_rate,
                ttl_seconds,
                allowed: false,
            });
        }

        counted_rate += 1;
        set_string(
            &mut guard,
            now,
            &key,
            &counted_rate.to_string(),
            per_seconds,
        );
        Ok(ManualRecapRateResult {
            counted_rate,
            ttl_seconds,
            allowed: true,
        })
    }

    async fn put_start_context(
        &self,
        domain: StartContextDomain,
        token: &str,
        json: &str,
    ) -> Result<()> {
        let key = domain.key(token);
        let (now, mut guard) = self.locked();
        set_string(&mut guard, now, &key, json, keys::START_CONTEXT_TTL_SECONDS);
        Ok(())
    }

    async fn get_start_context(
        &self,
        domain: StartContextDomain,
        token: &str,
    ) -> Result<Option<String>> {
        Ok(self.raw_string(&domain.key(token)))
    }

    async fn forwarded_active(&self, user_id: i64) -> Result<bool> {
        // Go reads the control key and compares it to the literal `1`; a key
        // holding anything else is not an ongoing session.
        Ok(self
            .raw_string(&keys::forwarded_control_key(user_id))
            .as_deref()
            == Some(keys::FORWARDED_CONTROL_ACTIVE_VALUE))
    }

    async fn start_forwarded(&self, user_id: i64) -> Result<()> {
        let already_open = self.forwarded_active(user_id).await?;
        let (now, mut guard) = self.locked();
        // The previous batch is dropped only when a session was already open,
        // and it is dropped before the control key is rewritten.
        if already_open {
            guard.remove(&keys::forwarded_batch_key(user_id));
        }
        set_string(
            &mut guard,
            now,
            &keys::forwarded_control_key(user_id),
            keys::FORWARDED_CONTROL_ACTIVE_VALUE,
            keys::FORWARDED_SESSION_TTL_SECONDS,
        );
        Ok(())
    }

    async fn append_forwarded(&self, user_id: i64, score_ms: i64, json: &str) -> Result<()> {
        let batch_key = keys::forwarded_batch_key(user_id);
        let (now, mut guard) = self.locked();
        let entry = guard.entry(batch_key.clone()).or_insert_with(|| Entry {
            value: Value::ZSet(HashMap::new()),
            expires_at_ms: ttl_deadline(now, keys::FORWARDED_SESSION_TTL_SECONDS),
        });
        if let Value::ZSet(members) = &mut entry.value {
            members.insert(json.to_owned(), score_ms);
        }
        refresh_expiry(
            &mut guard,
            now,
            &keys::forwarded_control_key(user_id),
            keys::FORWARDED_SESSION_TTL_SECONDS,
        );
        refresh_expiry(
            &mut guard,
            now,
            &batch_key,
            keys::FORWARDED_SESSION_TTL_SECONDS,
        );
        Ok(())
    }

    async fn forwarded_batch(&self, user_id: i64) -> Result<Vec<String>> {
        let Some(scored) = self.raw_zset(&keys::forwarded_batch_key(user_id)) else {
            return Ok(Vec::new());
        };
        // `ZREVRANGE 0 -1` yields descending score, then descending member for
        // ties; the caller reverses that back into replay order.
        let mut descending: Vec<String> =
            scored.into_iter().rev().map(|(_, member)| member).collect();
        descending.reverse();
        Ok(descending)
    }

    async fn cancel_forwarded(&self, user_id: i64) -> Result<bool> {
        if !self.forwarded_active(user_id).await? {
            // Go gates the cancel command on an ongoing session, so an orphan
            // batch is left in place.
            return Ok(false);
        }
        let (_, mut guard) = self.locked();
        guard.remove(&keys::forwarded_batch_key(user_id));
        guard.remove(&keys::forwarded_control_key(user_id));
        Ok(true)
    }

    async fn push_delete_later(&self, user_id: i64, chat_id: i64, message_id: i32) -> Result<()> {
        if !is_pushable_delete_later(user_id, chat_id, message_id) {
            return Ok(());
        }
        let (now, mut guard) = self.locked();
        push_list_front(
            &mut guard,
            now,
            &keys::delete_later_key(user_id),
            &keys::delete_later_member(chat_id, message_id),
            keys::DELETE_LATER_TTL_SECONDS,
        );
        Ok(())
    }

    async fn drain_delete_later(&self, user_id: i64) -> Result<Vec<(i64, i32)>> {
        let (_, mut guard) = self.locked();
        // Redis state is cleared before any Telegram deletion is attempted, so a
        // redelivery can never retry the same messages.
        let removed = guard.remove(&keys::delete_later_key(user_id));
        let Some(Entry {
            value: Value::List(members),
            ..
        }) = removed
        else {
            return Ok(Vec::new());
        };
        Ok(members
            .iter()
            .filter_map(|raw| keys::parse_delete_later_member(raw))
            .collect())
    }

    async fn auto_recap_zadd(&self, member: &str, score_ms: i64) -> Result<()> {
        let (_, mut guard) = self.locked();
        let entry = guard
            .entry(keys::AUTO_RECAP_QUEUE_KEY.to_owned())
            .or_insert_with(|| Entry {
                value: Value::ZSet(HashMap::new()),
                expires_at_ms: i64::MAX,
            });
        let Value::ZSet(members) = &mut entry.value else {
            return Err(anyhow!("recap Redis ZADD failed (TypeError)"));
        };
        members.insert(member.to_owned(), score_ms);
        entry.expires_at_ms = i64::MAX;
        Ok(())
    }

    async fn auto_recap_zpop_due(&self, now_ms: i64) -> Result<Option<String>> {
        let (_, mut guard) = self.locked();
        let Some(Entry {
            value: Value::ZSet(members),
            ..
        }) = guard.get_mut(keys::AUTO_RECAP_QUEUE_KEY)
        else {
            return Ok(None);
        };

        if !members.values().any(|score| (0..=now_ms).contains(score)) {
            return Ok(None);
        }
        let Some((member, score)) = members
            .iter()
            .map(|(member, score)| (member.clone(), *score))
            .min_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)))
        else {
            return Ok(None);
        };
        members.remove(&member);
        if score > now_ms {
            members.insert(member, score);
            return Ok(None);
        }
        Ok(Some(member))
    }

    async fn auto_recap_zrem(&self, member: &str) -> Result<()> {
        let (_, mut guard) = self.locked();
        if let Some(Entry {
            value: Value::ZSet(members),
            ..
        }) = guard.get_mut(keys::AUTO_RECAP_QUEUE_KEY)
        {
            members.remove(member);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Redis backend
// ---------------------------------------------------------------------------

/// Production [`RecapStateStore`] backed by a real Redis server.
#[derive(Clone)]
pub struct RedisRecapStateStore {
    manager: ::redis::aio::ConnectionManager,
}

/// Convert a Redis failure into a redacted error.
///
/// Only the operation name and the error kind survive: the raw error can quote
/// the connection address, credentials, or the stored payload, none of which may
/// reach a log line.
fn redacted(operation: &'static str, error: &::redis::RedisError) -> anyhow::Error {
    anyhow!("recap Redis {operation} failed ({:?})", error.kind())
}

/// Build driver connection details from typed configuration fields.
///
/// Only the scheme, host, and port are ever formatted into a string. The
/// database index and the credentials are attached afterwards as structured
/// settings, so a password-bearing URL is never constructed and cannot leak
/// through a stray format, a `Debug` print, or a driver error.
fn connection_info(config: &RedisConfig) -> Result<redis::ConnectionInfo> {
    let scheme = if config.tls_enabled {
        "rediss"
    } else {
        "redis"
    };
    let info = format!("{scheme}://{}:{}", config.host, config.port)
        .into_connection_info()
        .map_err(|error| redacted("address parsing", &error))?;
    let mut settings = redis::RedisConnectionInfo::default().set_db(i64::from(config.database));
    if let Some(username) = &config.username {
        settings = settings.set_username(username);
    }
    if let Some(password) = &config.password {
        settings = settings.set_password(password);
    }
    Ok(info.set_redis_settings(settings))
}

impl RedisRecapStateStore {
    /// Connect using `config`.
    ///
    /// Neither the address nor the credentials appear in any log line or error
    /// message; failures are reduced to an operation name and an error kind by
    /// [`redacted`].
    pub async fn connect(config: &RedisConfig) -> Result<Self> {
        let client = ::redis::Client::open(connection_info(config)?)
            .map_err(|error| redacted("client setup", &error))?;
        let manager = ::redis::aio::ConnectionManager::new(client)
            .await
            .map_err(|error| redacted("connection", &error))?;
        Ok(Self { manager })
    }

    fn connection(&self) -> ::redis::aio::ConnectionManager {
        self.manager.clone()
    }

    /// The raw string stored at `key`, for integration assertions.
    pub async fn raw_string(&self, key: &str) -> Result<Option<String>> {
        let mut connection = self.connection();
        let value: Option<String> = ::redis::cmd("GET")
            .arg(key)
            .query_async(&mut connection)
            .await
            .map_err(|error| redacted("GET", &error))?;
        Ok(value)
    }

    /// Remaining TTL of `key` in seconds, or `None` when it has none.
    pub async fn ttl_seconds(&self, key: &str) -> Result<Option<i64>> {
        let mut connection = self.connection();
        let ttl: i64 = ::redis::cmd("TTL")
            .arg(key)
            .query_async(&mut connection)
            .await
            .map_err(|error| redacted("TTL", &error))?;
        Ok((ttl >= 0).then_some(ttl))
    }

    /// Delete `keys`, used to keep integration runs non-destructive.
    pub async fn delete_keys(&self, keys: &[String]) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        let mut connection = self.connection();
        let mut command = ::redis::cmd("DEL");
        for key in keys {
            command.arg(key);
        }
        let _: i64 = command
            .query_async(&mut connection)
            .await
            .map_err(|error| redacted("DEL", &error))?;
        Ok(())
    }

    async fn set_with_ttl(&self, key: &str, value: &str, ttl_seconds: i64) -> Result<()> {
        let mut connection = self.connection();
        let _: () = ::redis::cmd("SET")
            .arg(key)
            .arg(value)
            .arg("EX")
            .arg(ttl_seconds)
            .query_async(&mut connection)
            .await
            .map_err(|error| redacted("SET", &error))?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let mut connection = self.connection();
        let _: i64 = ::redis::cmd("DEL")
            .arg(key)
            .query_async(&mut connection)
            .await
            .map_err(|error| redacted("DEL", &error))?;
        Ok(())
    }

    async fn expire(&self, key: &str, ttl_seconds: i64) -> Result<()> {
        let mut connection = self.connection();
        let _: i64 = ::redis::cmd("EXPIRE")
            .arg(key)
            .arg(ttl_seconds)
            .query_async(&mut connection)
            .await
            .map_err(|error| redacted("EXPIRE", &error))?;
        Ok(())
    }

    async fn drain_delete_later_status(&self, user_id: i64) -> Result<DeleteLaterDrain> {
        let key = keys::delete_later_key(user_id);
        let mut connection = self.connection();
        let members: Vec<String> = ::redis::cmd("LRANGE")
            .arg(&key)
            .arg(0_i64)
            .arg(-1_i64)
            .query_async(&mut connection)
            .await
            .map_err(|error| redacted("LRANGE", &error))?;
        if members.is_empty() {
            return Ok(DeleteLaterDrain {
                messages: Vec::new(),
                delete_error: None,
            });
        }
        let delete_error = self.delete(&key).await.err();
        Ok(DeleteLaterDrain {
            messages: members
                .iter()
                .filter_map(|raw| keys::parse_delete_later_member(raw))
                .collect(),
            delete_error,
        })
    }
}

#[async_trait]
impl RecapStateStore for RedisRecapStateStore {
    async fn put_callback(&self, route: &str, payload_json: &str) -> Result<String> {
        let action_hash = keys::callback_action_hash(payload_json);
        self.set_with_ttl(
            &keys::callback_payload_key(route, &action_hash),
            payload_json,
            keys::CALLBACK_PAYLOAD_TTL_SECONDS,
        )
        .await?;
        Ok(keys::callback_wire_value(route, payload_json))
    }

    async fn get_callback(&self, route: &str, action_hash: &str) -> Result<Option<String>> {
        self.raw_string(&keys::callback_payload_key(route, action_hash))
            .await
    }

    async fn check_manual_recap_rate(
        &self,
        chat_id: i64,
        rate: i64,
        per_seconds: i64,
    ) -> Result<ManualRecapRateResult> {
        if per_seconds <= 0 {
            return Ok(ManualRecapRateResult {
                counted_rate: 0,
                ttl_seconds: 0,
                allowed: true,
            });
        }

        let key = keys::manual_recap_rate_key(chat_id);
        let mut connection = self.connection();
        let counted: Option<String> = ::redis::cmd("GET")
            .arg(&key)
            .query_async(&mut connection)
            .await
            .map_err(|error| redacted("GET", &error))?;
        let mut counted_rate = counted
            .as_deref()
            .unwrap_or("0")
            .parse::<i64>()
            .map_err(|_| anyhow!("recap Redis GET failed (TypeError)"))?;
        let ttl_seconds: i64 = ::redis::cmd("TTL")
            .arg(&key)
            .query_async(&mut connection)
            .await
            .map_err(|error| redacted("TTL", &error))?;
        if counted_rate >= rate {
            return Ok(ManualRecapRateResult {
                counted_rate,
                ttl_seconds,
                allowed: false,
            });
        }

        counted_rate += 1;
        let _: () = ::redis::cmd("SET")
            .arg(&key)
            .arg(counted_rate)
            .arg("EX")
            .arg(per_seconds)
            .query_async(&mut connection)
            .await
            .map_err(|error| redacted("SET", &error))?;
        Ok(ManualRecapRateResult {
            counted_rate,
            ttl_seconds,
            allowed: true,
        })
    }

    async fn put_start_context(
        &self,
        domain: StartContextDomain,
        token: &str,
        json: &str,
    ) -> Result<()> {
        self.set_with_ttl(&domain.key(token), json, keys::START_CONTEXT_TTL_SECONDS)
            .await
    }

    async fn get_start_context(
        &self,
        domain: StartContextDomain,
        token: &str,
    ) -> Result<Option<String>> {
        self.raw_string(&domain.key(token)).await
    }

    async fn forwarded_active(&self, user_id: i64) -> Result<bool> {
        // `GET` and compare, not `EXISTS`: only the literal `1` opens a session.
        Ok(self
            .raw_string(&keys::forwarded_control_key(user_id))
            .await?
            == Some(keys::FORWARDED_CONTROL_ACTIVE_VALUE.to_owned()))
    }

    async fn start_forwarded(&self, user_id: i64) -> Result<()> {
        // Independent commands, in Go's order: drop a previous batch only when a
        // session was already open, then write the control key.
        if self.forwarded_active(user_id).await? {
            self.delete(&keys::forwarded_batch_key(user_id)).await?;
        }
        self.set_with_ttl(
            &keys::forwarded_control_key(user_id),
            keys::FORWARDED_CONTROL_ACTIVE_VALUE,
            keys::FORWARDED_SESSION_TTL_SECONDS,
        )
        .await
    }

    async fn append_forwarded(&self, user_id: i64, score_ms: i64, json: &str) -> Result<()> {
        let batch_key = keys::forwarded_batch_key(user_id);
        let mut connection = self.connection();
        let _: i64 = ::redis::cmd("ZADD")
            .arg(&batch_key)
            .arg(score_ms)
            .arg(json)
            .query_async(&mut connection)
            .await
            .map_err(|error| redacted("ZADD", &error))?;
        self.expire(
            &keys::forwarded_control_key(user_id),
            keys::FORWARDED_SESSION_TTL_SECONDS,
        )
        .await?;
        self.expire(&batch_key, keys::FORWARDED_SESSION_TTL_SECONDS)
            .await
    }

    async fn forwarded_batch(&self, user_id: i64) -> Result<Vec<String>> {
        let mut connection = self.connection();
        let mut members: Vec<String> = ::redis::cmd("ZREVRANGE")
            .arg(keys::forwarded_batch_key(user_id))
            .arg(0_i64)
            .arg(-1_i64)
            .query_async(&mut connection)
            .await
            .map_err(|error| redacted("ZREVRANGE", &error))?;
        members.reverse();
        Ok(members)
    }

    async fn cancel_forwarded(&self, user_id: i64) -> Result<bool> {
        if !self.forwarded_active(user_id).await? {
            return Ok(false);
        }
        self.delete_keys(&[
            keys::forwarded_batch_key(user_id),
            keys::forwarded_control_key(user_id),
        ])
        .await?;
        Ok(true)
    }

    async fn push_delete_later(&self, user_id: i64, chat_id: i64, message_id: i32) -> Result<()> {
        if !is_pushable_delete_later(user_id, chat_id, message_id) {
            return Ok(());
        }
        let key = keys::delete_later_key(user_id);
        let mut connection = self.connection();
        let _: i64 = ::redis::cmd("LPUSH")
            .arg(&key)
            .arg(keys::delete_later_member(chat_id, message_id))
            .query_async(&mut connection)
            .await
            .map_err(|error| redacted("LPUSH", &error))?;
        self.expire(&key, keys::DELETE_LATER_TTL_SECONDS).await
    }

    async fn drain_delete_later(&self, user_id: i64) -> Result<Vec<(i64, i32)>> {
        let drained = self.drain_delete_later_status(user_id).await?;
        if let Some(delete_error) = drained.delete_error {
            return Err(delete_error);
        }
        Ok(drained.messages)
    }

    async fn drain_delete_later_for_delivery(&self, user_id: i64) -> Result<DeleteLaterDrain> {
        self.drain_delete_later_status(user_id).await
    }

    async fn auto_recap_zadd(&self, member: &str, score_ms: i64) -> Result<()> {
        let mut connection = self.connection();
        let _: i64 = ::redis::cmd("ZADD")
            .arg(keys::AUTO_RECAP_QUEUE_KEY)
            .arg(score_ms)
            .arg(member)
            .query_async(&mut connection)
            .await
            .map_err(|error| redacted("ZADD", &error))?;
        Ok(())
    }

    async fn auto_recap_zpop_due(&self, now_ms: i64) -> Result<Option<String>> {
        let mut connection = self.connection();
        let due: Vec<String> = ::redis::cmd("ZRANGEBYSCORE")
            .arg(keys::AUTO_RECAP_QUEUE_KEY)
            .arg(0_i64)
            .arg(now_ms)
            .query_async(&mut connection)
            .await
            .map_err(|error| redacted("ZRANGEBYSCORE", &error))?;
        if due.is_empty() {
            return Ok(None);
        }

        let popped: Vec<(String, f64)> = ::redis::cmd("ZPOPMIN")
            .arg(keys::AUTO_RECAP_QUEUE_KEY)
            .arg(1_i64)
            .query_async(&mut connection)
            .await
            .map_err(|error| redacted("ZPOPMIN", &error))?;
        let Some((member, score)) = popped.into_iter().next() else {
            return Ok(None);
        };
        let score_ms = score as i64;
        if score_ms > now_ms {
            let mut last_error = None;
            for attempt in 0..100 {
                match self.auto_recap_zadd(&member, score_ms).await {
                    Ok(()) => return Ok(None),
                    Err(error) => last_error = Some(error),
                }
                if attempt < 99 {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            return Err(last_error.expect("one hundred restore attempts record an error"));
        }
        Ok(Some(member))
    }

    async fn auto_recap_zrem(&self, member: &str) -> Result<()> {
        let mut connection = self.connection();
        let _: i64 = ::redis::cmd("ZREM")
            .arg(keys::AUTO_RECAP_QUEUE_KEY)
            .arg(member)
            .query_async(&mut connection)
            .await
            .map_err(|error| redacted("ZREM", &error))?;
        Ok(())
    }
}
