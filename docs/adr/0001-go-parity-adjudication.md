# 0001 — Go-parity adjudication for the rich-recap port

Status: accepted (2026-08-16)

## Context

The Rust port tracks production Go `insights-bot` pinned at
`02aee8ce260165592e2152eb5a024a602e4eced1` with a strict 1:1 mandate for all
non-`/smr` Telegram behavior. A seven-module audit closed every confirmed
Telegram-callsite gap but left twelve deferred questions where parity,
production quality, and existing Rust behavior pulled in different
directions. The owner delegated the ruling; an independent Gemini review
endorsed every lean below without dissent.

## Decision

1. Error edits that Go routes through `ExceptionError` are emitted as bare
   `editMessageText` payloads: no `reply_markup`, no `parse_mode`. Go's
   `processExceptionError` never reads the markup field, so re-attaching the
   previous keyboard was a port-side invention. `MessageError`-class edits
   (HTML permission errors) keep their markup, as Go honors it there.
2. A failed automatic-recap queue write during configuration toggle-enable
   or rate-change surfaces as that stage's error edit, matching Go. The
   shared queue helper returns its error; the worker and startup seeding
   keep Go's log-and-continue handling.
3. `set_my_commands` at startup is kept as a recorded deviation: Go v1.0.0
   never registers a command menu, but removing it would regress user-facing
   UX for no behavioral gain.
4. The unused Telegraph client is removed; Go v1.0.0 removed the
   integration and the Rust construction was dead scaffolding.
5. The never-consulted governor-based `CommandRateLimiter` is removed.
6. Startup order aligns with Go: bind the health listener, hold Go's
   one-second pause, start the Telegram dispatcher, then arm the
   automatic-recap poller. The health port becomes an environment variable
   defaulting to Go's `7069`.
7. `/health` becomes Go's composite readiness JSON (bot authorized, poller
   started, automatic recap started) instead of a bare `SELECT 1`.
8. SIGINT/SIGTERM graceful shutdown is implemented with Go's reverse stop
   order: dispatcher, database pool, HTTP server with a ten-second timeout,
   poller.
9. `REDIS_CLIENT_CACHE_ENABLED` stays parsed-but-unwired as a recorded
   deviation: the `redis` crate offers no client-side-caching seam.
10. The module-private automatic-recap internals gain a crate-visible,
    awaitable test seam so the remaining integration tests can exist without
    sleep/poll synchronization, which this repository forbids.
11. Three Rust hardenings stay: fail-fast endpoint URL validation, the
    HTTP-status fallback for a missing `error_code`, and fail-fast
    `REDIS_PORT` parsing.
12. Pushing the local commits remains gated on the owner's explicit
    approval.

## Consequences

Decisions 1–2 change visible Telegram behavior (error edits drop the stale
keyboard exactly as production Go does) and required flipping committed
tests that had pinned the port-side invention. Decisions 6–8 change
deployment-facing surface: probes must read the new JSON, the default health
port moves to 7069 unless overridden, and the process now exits cleanly on
signals. Every deviation that survives this ruling is listed in
`docs/parity/go-parity-deviations.md`, which is the single registry future
parity questions should consult first.
