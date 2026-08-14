//! Isolated real-Redis fixture for recap state integration tests.
//!
//! The fixture is deliberately hermetic: it never inspects environment
//! variables and never reads a production Redis address. It only ever points at
//! a loopback server on a dedicated test database index, so running the suite
//! cannot touch a deployed instance. When no loopback server answers, the
//! fixture reports `None` and the calling test skips instead of failing.

use std::{
    net::{SocketAddr, TcpStream},
    time::Duration,
};

use insights_bot_telegram_rs::{config::RedisConfig, redis::recap_state::RedisRecapStateStore};
use uuid::Uuid;

/// How long the presence probe waits before declaring the port closed.
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Redis database index reserved for this suite.
const TEST_DATABASE_INDEX: u32 = 15;

/// Loopback-only Redis configuration.
///
/// Every field is a literal. No environment lookup happens here or anywhere
/// else in the fixture, so a production address can never reach these tests.
pub fn loopback_test_config() -> RedisConfig {
    RedisConfig {
        host: "127.0.0.1".to_owned(),
        port: 6379,
        tls_enabled: false,
        username: None,
        password: None,
        database: TEST_DATABASE_INDEX,
        client_cache_enabled: false,
    }
}

/// A host that can never parse into a URL authority.
pub const UNPARSEABLE_HOST: &str = "not a host";
/// Sentinel credentials that must never reach an error message.
pub const SENTINEL_USERNAME: &str = "sentinel-actor";
/// Sentinel credentials that must never reach an error message.
pub const SENTINEL_CREDENTIAL: &str = "sentinel-credential";

/// A configuration whose connection attempt fails before any socket is opened.
///
/// The host is syntactically invalid, so the driver rejects it during address
/// parsing instead of retrying a real endpoint. That keeps the redaction check
/// instantaneous and offline.
pub fn unparseable_address_test_config() -> RedisConfig {
    RedisConfig {
        host: UNPARSEABLE_HOST.to_owned(),
        port: 6379,
        tls_enabled: false,
        username: Some(SENTINEL_USERNAME.to_owned()),
        password: Some(SENTINEL_CREDENTIAL.to_owned()),
        database: TEST_DATABASE_INDEX,
        client_cache_enabled: false,
    }
}

/// Whether anything is listening on the loopback test port.
///
/// The driver's connection manager retries with backoff for several seconds
/// before giving up, so a bounded TCP probe runs first to keep the skip path
/// cheap.
fn loopback_is_listening(config: &RedisConfig) -> bool {
    let Ok(address) = format!("{}:{}", config.host, config.port).parse::<SocketAddr>() else {
        return false;
    };
    TcpStream::connect_timeout(&address, PROBE_TIMEOUT).is_ok()
}

/// Connect to the loopback test server, or report `None` when it is absent.
pub async fn connect() -> Option<RedisRecapStateStore> {
    let config = loopback_test_config();
    if !loopback_is_listening(&config) {
        return None;
    }
    RedisRecapStateStore::connect(&config).await.ok()
}

/// A collision-free actor identifier so concurrent runs never share keys.
///
/// Key isolation is achieved by unique identifiers rather than by flushing the
/// database, so the fixture stays non-destructive.
pub fn unique_actor_id() -> i64 {
    let bytes = Uuid::new_v4().into_bytes();
    let raw = u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]);
    i64::try_from(raw >> 2).expect("62 bits always fit in a positive i64")
}

/// A collision-free JSON payload so callback keys never collide between runs.
pub fn unique_payload_json() -> String {
    format!("{{\"probe\":\"{}\"}}", Uuid::new_v4())
}
