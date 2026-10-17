//! The gateway runtime bridge — the GUI side of the gateway/GUI split
//! (docs/specs/gateway-gui-split-plan.md, Step 2).
//!
//! The gateway relay is a separate process (`cardinal-gateway`); this module
//! owns a dedicated OS thread running a tokio runtime that talks to it over
//! HTTP: every admin read/write for the dashboard pages goes through
//! [`GatewayHttpClient`] (spec §8 decision 2), and the live-activity feed is
//! the client's reconnecting SSE subscription to `/api/activity` (decision 7).
//! Startup is a `/health` probe (decision 1 — manual start): an unreachable
//! gateway is the [`RelayMode::Unreachable`] state, not a fatal error, and
//! the status bar offers a retry that re-runs the probe. Every failure
//! surfaces as an `Err(String)` the pages render.
//!
//! Exactly **one** [`GatewayHttpClient`] exists per process — one reqwest
//! connection pool, built once on the admin runtime. Both the SSE pump and
//! every admin call share it. The client is built on the admin thread because
//! reqwest's connector init spawns background tasks via hyper-util, which
//! panics without a tokio reactor context (the GPUI main thread has none).
//! The startup probe likewise runs on the admin runtime; pages only *observe*
//! its outcome through a [`watch`] channel, so a page load can never drive
//! reqwest from the main thread.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gateway::http_client::{GatewayHttpClient, UNREACHABLE_MESSAGE};
use tokio::sync::watch;

/// How the gateway relates to this process, for the status bar.
#[derive(Clone)]
pub(crate) enum RelayMode {
    /// `/health` answered (carries the listen address).
    Connected(String),
    /// The gateway is not running (spec §8.1: manual start; the UI shows
    /// 「gateway 未运行，请先运行 `cardinal-gateway`」 plus a retry).
    Unreachable,
}

/// Startup probe state: driven by the admin runtime, observed by pages.
#[derive(Clone, PartialEq, Eq)]
enum Link {
    /// The probe has not resolved yet.
    Pending,
    /// The probe answered: admin calls may run.
    Connected,
    /// The probe did not answer.
    Unreachable,
}

/// Process-wide handle to the gateway runtime, installed as a GPUI global
/// before the window opens.
pub(crate) struct GatewayRuntime {
    /// Tokio handle for dispatching admin operations onto the admin thread.
    /// `None` only when the runtime itself failed to build.
    handle: Option<tokio::runtime::Handle>,
    /// The single gateway HTTP client (one connection pool for the whole
    /// process), built once on the admin runtime. `None` only on a failed
    /// runtime.
    client: Option<GatewayHttpClient>,
    /// Base URL of the relay (`http(s)://{listen}`) for the chat panel.
    chat_url: String,
    /// Startup probe state. The admin runtime drives it; pages clone the
    /// receiver and await a non-`Pending` value — observing only, never
    /// driving the probe.
    link: watch::Receiver<Link>,
    /// The probe trigger (kept to re-run the probe on
    /// [`GatewayRuntime::reconnect`]).
    link_tx: Option<watch::Sender<Link>>,
    /// The live-activity subscription (single-consumer): the GUI's feed takes
    /// it once via [`GatewayRuntime::take_activity`]. The SSE pump starts with
    /// the runtime and retries on its own schedule — a gateway that comes up
    /// later (after the status-bar retry, or independently) starts feeding
    /// events without any rewiring.
    activity: Mutex<Option<gateway::activity::ActivityReceiver>>,
    /// Set when an admin call fails at connect (gateway died mid-session),
    /// cleared on the next success or probe. The status bar reads it so a
    /// dead gateway turns red even though the one-shot probe resolved
    /// Connected long ago.
    link_down: Arc<AtomicBool>,
    /// `Some` only on a runtime that never built; every awaiter surfaces this
    /// message.
    failure: Option<Arc<str>>,
}

impl gpui::Global for GatewayRuntime {}

/// A runtime whose startup has already failed with `message`.
fn failed(message: String) -> GatewayRuntime {
    tracing::error!(%message, "gateway runtime failed");
    let message: Arc<str> = Arc::from(message);
    let (_tx, link) = watch::channel(Link::Pending);
    GatewayRuntime {
        handle: None,
        client: None,
        chat_url: String::new(),
        link,
        link_tx: None,
        activity: Mutex::new(None),
        link_down: Arc::new(AtomicBool::new(false)),
        failure: Some(message),
    }
}

/// Probe `GET /health` (no auth) on the admin runtime and publish the outcome
/// to `tx`. Runs entirely on the admin thread — never on the GPUI main thread.
fn probe(handle: &tokio::runtime::Handle, client: GatewayHttpClient, tx: watch::Sender<Link>) {
    drop(handle.spawn(async move {
        let link = match client.health().await {
            Ok(()) => {
                tracing::info!("gateway /health answered; admin surface is HTTP");
                Link::Connected
            }
            Err(error) => {
                tracing::warn!(%error, "gateway unreachable; run `cardinal-gateway`");
                Link::Unreachable
            }
        };
        let _ = tx.send(link);
    }));
}

/// Spawn the dedicated admin thread, build the single gateway client on it,
/// and probe the gateway. Never blocks the caller on startup progress: the
/// outcome lands in the `link` watch channel.
pub(crate) fn spawn(config: gateway::config::Config) -> GatewayRuntime {
    let chat_url = if config.listen.contains("://") {
        config.listen.clone()
    } else {
        format!("http://{}", config.listen)
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return failed(format!("failed to build the admin tokio runtime: {error}")),
    };
    let handle = runtime.handle().clone();
    // Park the runtime on its own thread for the life of the process; in-flight
    // tasks shut down with process exit.
    if let Err(error) = std::thread::Builder::new()
        .name("cardinal-gateway-admin".to_string())
        .spawn(move || {
            runtime.block_on(std::future::pending::<()>());
        })
    {
        return failed(format!("failed to spawn the admin thread: {error}"));
    }

    // Build the single gateway client on the admin runtime. reqwest's
    // connector init spawns background tasks via hyper-util (`TokioExecutor`),
    // which panics without a reactor context — so this must not run on the
    // GPUI main thread.
    let (client_tx, client_rx) = std::sync::mpsc::channel();
    let build_config = config.clone();
    drop(handle.spawn(async move {
        let client = GatewayHttpClient::new(&build_config.listen, &build_config.gateway_key);
        let _ = client_tx.send(client);
    }));
    let client = match client_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(client) => client,
        Err(_) => return failed("timed out building the gateway client".to_string()),
    };

    // The activity SSE pump starts immediately and reconnects on its own
    // schedule: the feed comes alive whenever the gateway does, regardless of
    // when (or whether) the status-bar retry runs. `subscribe_activity` spawns
    // on the admin runtime's handle explicitly, so it is safe from this thread.
    let activity = Mutex::new(Some(client.subscribe_activity(&handle)));

    // Startup probe: driven on the admin runtime, observed by pages.
    let (link_tx, link) = watch::channel(Link::Pending);
    probe(&handle, client.clone(), link_tx.clone());

    GatewayRuntime {
        handle: Some(handle),
        client: Some(client),
        chat_url,
        link,
        link_tx: Some(link_tx),
        activity,
        link_down: Arc::new(AtomicBool::new(false)),
        failure: None,
    }
}

impl GatewayRuntime {
    /// Resolve the startup probe (or the runtime failure). Cloned handles only,
    /// so the returned future is `'static` and a page load never drives the
    /// probe itself.
    async fn resolved_link(
        mut link: watch::Receiver<Link>,
        failure: Option<Arc<str>>,
    ) -> Result<Link, String> {
        if let Some(message) = failure {
            return Err(message.to_string());
        }
        match link.wait_for(|state| *state != Link::Pending).await {
            Ok(state) => Ok(state.clone()),
            Err(_) => Err("admin runtime unavailable".to_string()),
        }
    }

    /// Take the live-activity subscription (single-consumer; the first taker —
    /// the GUI's activity feed — owns it, later calls get `None`). The SSE pump
    /// reconnects and reseeds on its own, so this resolves immediately.
    pub(crate) fn take_activity(
        &self,
    ) -> impl Future<Output = Option<gateway::activity::ActivityReceiver>> + Send + 'static {
        let activity = self.activity.lock().ok().and_then(|mut slot| slot.take());
        async move { activity }
    }

    /// Re-run the startup probe (the status-bar retry for the Unreachable
    /// state): the probe state resets to `Pending` and a fresh probe runs on
    /// the admin runtime. Every pending and future observer resolves against
    /// the new probe.
    pub(crate) fn reconnect(&self) {
        let (Some(handle), Some(client), Some(link_tx)) = (
            self.handle.as_ref(),
            self.client.as_ref(),
            self.link_tx.as_ref(),
        ) else {
            return;
        };
        self.link_down.store(false, Ordering::Relaxed); // probe pending: optimistic until it resolves
        let _ = link_tx.send(Link::Pending);
        probe(handle, client.clone(), link_tx.clone());
    }

    /// Run one operation against the gateway's HTTP admin API, executed on the
    /// admin thread. Await the returned future inside `cx.spawn`; an
    /// unreachable gateway, query errors, and a vanished runtime all come back
    /// as `Err`.
    pub(crate) fn admin_call<T, F, Fut>(
        &self,
        f: F,
    ) -> impl Future<Output = Result<T, String>> + 'static
    where
        T: Send + 'static,
        F: FnOnce(GatewayHttpClient) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, gateway::http_client::ClientError>> + Send + 'static,
    {
        let handle = self.handle.clone();
        let client = self.client.clone();
        let link = self.link.clone();
        let failure = self.failure.clone();
        let link_down = self.link_down.clone();
        async move {
            let state = Self::resolved_link(link, failure).await?;
            let (Some(handle), Some(client)) = (handle, client) else {
                return Err("admin runtime unavailable".to_string());
            };
            if state != Link::Connected {
                return Err(UNREACHABLE_MESSAGE.to_string());
            }
            let (tx, rx) = tokio::sync::oneshot::channel();
            drop(handle.spawn(async move {
                let _ = tx.send(f(client).await);
            }));
            match rx.await {
                Ok(Ok(value)) => {
                    link_down.store(false, Ordering::Relaxed); // the link answered; heal the status bar
                    Ok(value)
                }
                Ok(Err(error)) if error.is_unreachable() => {
                    // The gateway died mid-session: surface the guidance message (not
                    // a raw "request failed") and flag the link so the status bar
                    // turns red even though the one-shot probe resolved Connected.
                    link_down.store(true, Ordering::Relaxed);
                    Err(UNREACHABLE_MESSAGE.to_string())
                }
                Ok(Err(error)) => Err(format!("{error:#}")),
                Err(_) => Err("admin runtime stopped mid-query".to_string()),
            }
        }
    }

    /// True while the last admin call failed to connect (the gateway died
    /// mid-session). Read by the status bar on every render.
    pub(crate) fn link_down(&self) -> bool {
        self.link_down.load(Ordering::Relaxed)
    }

    /// Resolve once startup completes: how the gateway relates to this
    /// process, or why startup itself failed. For the status bar.
    pub(crate) fn relay_mode(&self) -> impl Future<Output = Result<RelayMode, String>> + 'static {
        let link = self.link.clone();
        let failure = self.failure.clone();
        let addr = self
            .chat_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .to_string();
        async move {
            let state = Self::resolved_link(link, failure).await?;
            Ok(match state {
                Link::Connected => RelayMode::Connected(addr),
                _ => RelayMode::Unreachable,
            })
        }
    }

    /// Resolve once startup completes: `Ok(())` when connected, `Err` with
    /// the unreachable guidance otherwise.
    pub(crate) fn await_ready(&self) -> impl Future<Output = Result<(), String>> + 'static {
        let link = self.link.clone();
        let failure = self.failure.clone();
        async move {
            match Self::resolved_link(link, failure).await? {
                Link::Connected => Ok(()),
                _ => Err(UNREACHABLE_MESSAGE.to_string()),
            }
        }
    }

    /// The gateway's base URL (for the settings page).
    pub(crate) fn base_url(&self) -> &str {
        &self.chat_url
    }
}
