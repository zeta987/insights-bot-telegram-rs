# insights-bot-telegram-rs

這是 `insights-bot` 的 Telegram Recap Rust 版本。它使用 teloxide 處理
Telegram 更新、SQLx 儲存資料，並準備與 Go `v1.0.0` 的 Rich Message recap
行為對齊。

## 設定

複製 `.env.example` 為 `.env` 後，至少提供 `TELEGRAM_BOT_TOKEN`、
`OPENAI_API_SECRET` 與 `REDIS_PORT`。`REDIS_PORT` 必須在 `1..=65535`，空白或
缺少的 `REDIS_HOST` 會使用 `localhost`。`TELEGRAM_BOT_API_ENDPOINT` 缺少時會
使用 `https://api.telegram.org`；自訂值必須是絕對 HTTP(S) base URL，並由
teloxide 與 Rich Message transport 共用。

`OPENAI_API_SECRET` 是正式變數；只有它不存在時才使用相容別名
`OPENAI_API_KEY`。詳細摘要模型預設為 `gpt-3.5-turbo`，總 token 預設為
`4096`，recap reserve 預設為 `2000`，兩者差值必須大於零。模型備援清單以逗號
分隔，會移除空白、重複值和 primary model；沒有 `CHECK_MODEL` 時忽略 Check
備援。

`REDIS_TLS_ENABLED`、`REDIS_CLIENT_CACHE_ENABLED`、三個 OpenAI 測試或 verbose
開關，以及 `AUTO_RECAP_TEST_ENABLED` 只接受 `true` 或 `1`。無效或負數的
`REDIS_DB` 會使用 `0`，無效或負數的
`HARD_LIMIT_MANUAL_RECAP_RATE_PER_SECONDS` 也會使用 `0`。啟用
`AUTO_RECAP_TEST_ENABLED` 並提供非零 `AUTO_RECAP_TEST_CHAT_ID` 時，啟動會保留
一般排程，另外佇列一次立即自動 recap。

`SARCASTIC_CONDENSED_USER_PROMPT` 在啟動時以 recap 使用的 Go-template
語法驗證，支援 `{{ .ChatHistory }}`、`printf` 與 comments；格式錯誤會在
OpenAI 請求前回傳設定錯誤。

## 驗證

```bash
cargo fmt
cargo check
cargo clippy --all-targets --all-features -D warnings
cargo test
```
