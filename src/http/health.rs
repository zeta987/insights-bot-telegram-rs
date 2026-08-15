//! Composite `/health` readiness endpoint.
//!
//! Mirrors Go's `internal/services/health/health.go`: a JSON body carrying an
//! aggregated `status` plus per-check `details`, served by
//! `github.com/alexliesenfeld/health`. This port reimplements the same
//! response shape and HTTP status mapping (200 when up, 503 otherwise) over
//! the shared [`LifecycleFlags`], since Go's checker library has no Rust
//! equivalent in this crate's dependency graph. Go's checker carries no
//! database check, so this port carries none either (see
//! `docs/adr/0001-go-parity-adjudication.md`, decision 7).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use serde::Serialize;
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};
use tracing::{error, info, warn};

use crate::lifecycle::LifecycleFlags;

/// One named check's JSON shape, matching Go's `health.CheckResult`
/// marshaller (`status` only; Go's `error`/`timestamp` fields are always
/// empty for these boolean-flag checks, so they are omitted here).
#[derive(Debug, Serialize, PartialEq, Eq)]
struct CheckResult {
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct HealthDetails {
    telegram_bot: CheckResult,
    #[serde(rename = "auto recap timecapsule digger")]
    auto_recap_timecapsule_digger: CheckResult,
    auto_recap: CheckResult,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    details: HealthDetails,
}

const fn status_label(up: bool) -> &'static str {
    if up { "up" } else { "down" }
}

async fn health_handler(State(lifecycle): State<Arc<LifecycleFlags>>) -> impl IntoResponse {
    let telegram_bot = lifecycle.bot_authorized();
    let poller = lifecycle.poller_started();
    let auto_recap = lifecycle.auto_recap_started();
    let all_up = telegram_bot && poller && auto_recap;

    let body = HealthResponse {
        status: status_label(all_up),
        details: HealthDetails {
            telegram_bot: CheckResult {
                status: status_label(telegram_bot),
            },
            auto_recap_timecapsule_digger: CheckResult {
                status: status_label(poller),
            },
            auto_recap: CheckResult {
                status: status_label(auto_recap),
            },
        },
    };

    // Go's handler maps StatusDown/StatusUnknown to 503 and everything else
    // to 200 (`handler.go`'s `mapHTTPStatusCode`); this port has only up/down.
    let code = if all_up {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(body))
}

/// Build the `/health` router for the given lifecycle state, independent of
/// binding, so tests can exercise it directly (e.g. with `axum::Router::oneshot`
/// or a bound listener).
pub fn router(lifecycle: Arc<LifecycleFlags>) -> Router {
    Router::new()
        .route("/health", get(health_handler))
        .with_state(lifecycle)
}

/// A bound, running health server. Dropping this handle leaves the server
/// running; call [`HealthServerHandle::shutdown`] to stop it gracefully.
pub struct HealthServerHandle {
    /// The actual bound address (useful when `addr`'s port was `0`).
    pub local_addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join_handle: JoinHandle<()>,
}

impl HealthServerHandle {
    /// Gracefully stop the server, bounded by Go's ten-second `Shutdown`
    /// timeout (`health.go`'s `OnStop` hook: `context.WithTimeout(...,
    /// 10*time.Second)`).
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if tokio::time::timeout(Duration::from_secs(10), &mut self.join_handle)
            .await
            .is_err()
        {
            warn!("health server did not shut down within ten seconds");
        }
    }
}

/// Bind the health listener and start serving, returning only after the
/// listener is bound. This mirrors Go's `net.Listen` (synchronous) followed
/// by `go server.Serve()` (backgrounded) in `health.Run`.
pub async fn serve(lifecycle: Arc<LifecycleFlags>, addr: SocketAddr) -> Result<HealthServerHandle> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind health listener on {addr}"))?;
    let local_addr = listener
        .local_addr()
        .context("failed to read health listener address")?;
    info!("health listener bound on {local_addr}");

    let app = router(lifecycle);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let join_handle = tokio::spawn(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await;
        if let Err(source) = result {
            error!(error = %source, "health server stopped unexpectedly");
        }
    });

    Ok(HealthServerHandle {
        local_addr,
        shutdown_tx: Some(shutdown_tx),
        join_handle,
    })
}
