# Approved deviations from pinned Go `02aee8ce`

The single registry of places where the Rust port deliberately differs from
production Go v1.0.0. Anything not listed here is expected to match Go
byte-for-byte on the Telegram wire; see
`docs/adr/0001-go-parity-adjudication.md` for how the contested items were
ruled.

## UX enhancements kept on purpose

- `set_my_commands` runs at startup (`src/bot/mod.rs`); Go never registers a
  command menu. Failures must never block startup.

## Dependency limitations

- `REDIS_CLIENT_CACHE_ENABLED` is parsed into `RedisConfig` but unwired: Go's
  rueidis client toggles client-side caching, and the Rust `redis` crate has
  no equivalent seam.

## Defensive hardenings

- `TELEGRAM_BOT_API_ENDPOINT` is validated fail-fast (scheme, host, no
  query/fragment, trailing-slash trim); Go concatenates raw strings and only
  fails on the first live call.
- The Rich transport falls back to the HTTP status code when Telegram's
  error JSON omits `error_code`; Go stores zero. Observable only against
  malformed proxies.
- `REDIS_PORT` is required at config load; Go silently defaults to an empty
  string and fails later inside the Redis client.

## Bounded replacements for Go crashes and bugs

- A missing recap-options row is materialised via `find_one_or_create` on
  the toggle and pin paths instead of reproducing Go's nil-pointer process
  crash; the mode handler's impossible missing-row arm edits the general
  error text.
- Go ships only `en` and `zh-CN` bundles and its BCP 47 matcher resolves
  every `zh-*` sender (including zh-TW/zh-HK) to `zh-CN`. Rust serves each
  of its three bundles under its own locale via a case-insensitive
  language-code mapping; zh-Hant keys that Go never shipped in Traditional
  carry the Simplified text Go would actually serve those users, so no
  translations are invented and no Chinese user regresses to English.
- Message-entity offsets are computed with checked UTF-16 arithmetic where
  Go would panic on surrogate-boundary edge cases; the divergence is pinned
  by `tests/message_entity_tests.rs`.

## Test-only seams

- `handle_auto_recap_capsule` and `generate_and_deliver_auto_recap` are
  `#[doc(hidden)] pub` test seams, and the capsule handler returns its
  preparation plus the Generate branch's `JoinHandle` so integration tests
  can await completion without sleep/poll loops, which this repository
  forbids. The production caller inspects only errors, so behavior is
  unchanged. The `AUTO_RECAP_TEST_ENABLED` immediate-dispatch path remains
  untested because making it awaitable would change the startup signature.
