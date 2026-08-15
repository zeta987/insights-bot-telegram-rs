# Claude Code Handoff — Go Rich Recap Port

Updated: 2026-08-16

## Objective

Continue the 1:1 port of Telegram Rich recap behavior from the production Go
v1.0.0 source into Rust. The pinned Go source revision is
`02aee8ce260165592e2152eb5a024a602e4eced1`.

`/smr` generation is excluded. Keep only the `/smr` compatibility feedback
route/buttons required by Rich recap messages.

## Repositories

- Rust target and current working directory:
  `D:\Data\Coding_Github\Projects\_TG_BOT\insights-bot-workspace\insights-bot-telegram-rs`
- Go source:
  `D:\Data\Coding_Github\Projects\_TG_BOT\insights-bot-workspace\insights-bot-go`
- Rust branch: `feat/rich-recap-parity`
- Behavior ledger:
  `docs/parity/go-v1.0.0-rich-recap-ledger.md`

Always treat the latest signed local `HEAD`, `git status --short`, this file,
and the current Codex session as the progress source of truth. The session may
contain work completed after this file's timestamp.

## Completed checkpoints

The branch already contains signed local commits for message capture, Rich
formatting and transport, Rich delivery and generation, public manual recap,
subscriptions, and forwarded recap. Run `git log --oneline -12` after the
required lock preflight to see the exact current list.

The latest completed automatic-recap commit is `be73d17` (`feat: port automatic
rich recaps`). A later local checkpoint adds the presentation half of
`/configure_recap`:

- `src/bot/handlers/recap_configure.rs` exposes `build_configure_keyboard`.
- `tests/recap_configure_tests.rs` fixes the disabled five-row and enabled
  nine-row keyboards, exact Simplified Chinese labels, `nop` callback payload,
  compact camel-case JSON, hashed callback wires, and the pin payload without
  `fromId`.
- `src/bot/handlers/mod.rs` exports the new module.
- The focused keyboard test passed before the checkpoint was committed.

The current automatic-recap checkpoint adds:

- Go TimeCapsule Redis ZSET key and deterministic padded standard-Base64 member.
- One-second poll cadence, no-TTL ZADD rescore, due pop-before-handler behavior,
  fixed-offset 2/3/4-per-day scheduling, and 12/8/6-hour history windows.
- Ten-attempt feature/options/subscriber reads, disabled/private-only decisions,
  pre-generation requeue, and the bounded Rust replacement for Go's nil-options
  dereference.
- Rich recap generation from the parity history/log repositories, public-first
  delivery, duplicate subscriber fan-out, shared five-per-second send limiter,
  subscriber unsubscribe keyboards, partial-send persistence, pin replacement,
  and `sent_messages` tracking.

Relevant files:

- `src/services/autorecap.rs`
- `src/services/autorecap_queue.rs`
- `src/services/autorecap_delivery.rs`
- `src/redis/keys.rs`
- `src/redis/recap_state.rs`
- `tests/autorecap_worker_tests.rs`
- `tests/autorecap_queue_tests.rs`
- `tests/autorecap_delivery_tests.rs`

Latest verification before this handoff:

- `cargo check` passed.
- `cargo clippy --all-targets --all-features -- -D warnings` passed.
- Full `cargo test` passed.
- Automatic-recap focused tests passed: worker 9, queue 11, delivery 8.

## Next required slice

Finish `/configure_recap` from the pinned Go callback implementation. The exact
keyboard and callback-allocation primitive already exists, but
`src/bot/handlers/recap.rs` still runs the legacy `recap_configs` command and
raw `cfg:*` callback path. Replace that runtime path with the new module, parity
repositories, and the five registered configure routes.

Required behavior includes the feature toggle, public/private send mode,
2/3/4-per-day rate, pin toggle, exact Go callback payload JSON and labels,
owner/admin gates, and completion behavior. Enabling recap and changing the
daily rate must immediately add or rescore the deterministic TimeCapsule member.
Disabling recap must leave an already queued member in Redis; its later disabled
read consumes it without requeueing. Add one RED handler test before each
vertical implementation slice.

The remaining production work is the command handler plus toggle, assign-mode,
complete, rates-per-day, and pin callbacks. Preserve these pinned details:

- Toggle accepts creator, administrator, or the exact Group Anonymous Bot;
  assign-mode, rate, and pin are creator-only; complete accepts creator,
  administrator, or anonymous and does not recheck bot-admin status.
- Toggle, assign-mode, rate, and complete validate callback chat and actor, with
  the anonymous original-command exception. Pin intentionally has no such
  payload guard.
- Complete best-effort deletes both the settings message and its replied-to
  command without sending a completion message.
- Rate changes rescore even while the feature is disabled. Disable never
  removes the existing capsule.
- Pin rebuilds the keyboard with its requested pin status incorrectly reused as
  recap-enabled status. This visible Go quirk must remain fixed by a test.
- The approved ledger resolves the first-enable missing-options crash by using
  a usable first-enable options row before queueing; do not add a process panic.

After configuration is complete, perform a read-only Go/Rust parity review of
the automatic worker, run the full verification set, scan the exact staged diff
for secrets and personal data, then create a signed local commit. Do not push.
The bore tunnel on port 9487 and AyuGram are available for a later live test.

## Mandatory repository rules

- Read `AGENTS.md` before changing files.
- Never read or stage `.env` or `.env.golang1`.
- Use `apply_patch` for every manual file edit; formatters may modify formatting.
- Before every Git command, run `Test-Path .git\index.lock` and proceed only when
  it prints `False`. Never delete a lock that may belong to another process.
- Local commits only until the user explicitly asks to push.
- Commit with `git commit -S` and the exact trailer:
  `Co-authored-by: Codex <noreply@openai.com>`
- Verify the signature with `git verify-commit HEAD` and verify the exact trailer.
- Scan staged changes for API keys, tokens, passwords, private endpoints, email
  addresses, and personal data before committing.
- Preserve unrelated user changes and stage only the intended module files.
- `cargo fmt --check` has a known unrelated baseline difference in
  `src/db/recap_config.rs` and `src/services/recap.rs`; use scoped rustfmt for
  touched Rust files and do not reformat those two files as collateral change.

## Paste-ready continuation prompt

Read `AGENTS.md`, this handoff file, and
`docs/parity/go-v1.0.0-rich-recap-ledger.md` completely. If the Codex session is
available, read its latest messages too. Then check `.git/index.lock`, current
branch, latest signed commits, and the unstaged/staged diff without reading any
`.env*` file. Preserve the completed automatic-recap checkpoint and continue
the pinned Go v1.0.0 1:1 port from the `/configure_recap` slice described above.
Use TDD, keep edits scoped, run focused plus full Rust verification, perform a
staged secrets/PII scan, and create signed local commits with the exact Codex
co-author trailer. Never push unless the user explicitly authorizes that push.
