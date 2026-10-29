//! Live-activity feed (GUI side of `gateway::activity`): drains the SSE
//! subscription to the standalone gateway's `/api/activity` (reconnect +
//! reseed live in `gateway::http_client`) into a single GPUI entity the
//! status pages observe. The entity owns every event application — pages
//! only render `live()` and react to the persisted counter.
//!
//! Semantics (mirrors the publisher's): the feed is decorative, the ledger
//! is truth. A lost event heals two ways — a slow ticker re-reads the ledger
//! and sweeps ghost rows, and the pages re-query on `persisted` bumps.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use gpui::{Context, Hsla};

use gateway::activity::{ActivityEvent, EndMetrics, EndStatus, StartEvent};
use gateway::admin_ops::Bootstrap;

use crate::runtime::GatewayRuntime;

/// A live row that saw no event for this long lost its End: sweep it. (The
/// clock is the last event, not the start — long streams stay alive as long
/// as Progress keeps arriving.)
const LIVE_TTL: Duration = Duration::from_secs(600);
/// Safety-net poll interval: heals dropped events and ages the live labels.
const POLL: Duration = Duration::from_secs(15);
/// How many finished attempts the settled ring keeps (newest first); the
/// oldest are dropped past the cap. An End moves the attempt out of `live`
/// into this buffer instead of dropping it. The feed renders every in-flight
/// attempt first (live work is never hidden, so the live block is uncapped),
/// then the newest settled rows up to this same display limit.
pub(crate) const SETTLED_CAP: usize = 17;

/// One in-flight relay attempt (one ledger row once it settles).
#[derive(Clone)]
pub(crate) struct LiveAttempt {
    pub client_model: String,
    /// Route alias when the client hit a route (None = direct provider/model).
    pub route: Option<String>,
    pub account: Option<String>,
    pub served_model: String,
    /// The Smart strategy's reason for choosing this model ("" = ordered route
    /// or a resting candidate); carried over to the settled row on End.
    pub reason: String,
    /// The caller's identity (gateway key label); None renders as 本机.
    pub caller: Option<String>,
    /// Which client the request's User-Agent named; display wins over `caller`.
    pub agent: Option<String>,
    /// The provider key serving the attempt (identity for stage matching —
    /// the account label is a clipped display string, the id is not).
    pub key_id: Option<i64>,
    pub stream: bool,
    /// True once First arrived: the client is receiving bytes.
    pub streaming: bool,
    pub started: Instant,
    /// Last event for this attempt — the ghost sweep clocks this, not `started`.
    last_seen: Instant,
    /// Client-bound wire bytes so far (throttled Progress snapshots).
    pub bytes: u64,
}

impl LiveAttempt {
    /// Phase label: connecting until the first byte, streaming after (a
    /// non-streaming attempt stays "connecting" — headers-to-body is not
    /// observed).
    pub(crate) fn phase(&self) -> &'static str {
        if self.streaming {
            if self.stream {
                "传输中"
            } else {
                "接收中"
            }
        } else {
            "连接中"
        }
    }

    /// Settle the attempt into the feed's GUI-local projection, stamped with the
    /// End status, the ledger metrics the End carried, the settle time and the
    /// Start→End duration. The single owner of the field mapping, so the End arm
    /// just calls it.
    fn into_settled(self, status: EndStatus, metrics: EndMetrics, now: Instant) -> SettledAttempt {
        SettledAttempt {
            client_model: self.client_model,
            route: self.route,
            account: self.account,
            served_model: self.served_model,
            caller: self.caller,
            agent: self.agent,
            reason: self.reason,
            status,
            metrics,
            settled_at: chrono::Utc::now(),
            duration: now.saturating_duration_since(self.started),
        }
    }
}

/// One finished relay attempt: a live row that saw its End, kept in the
/// settled ring so it stays visible on the 动态 feed after it resolves. A
/// GUI-local projection of the attempt's display fields — NOT a ledger row
/// (the 请求 page owns the full record).
#[derive(Clone)]
pub(crate) struct SettledAttempt {
    /// Model id as the client sent it (route alias or `provider/model`).
    pub client_model: String,
    /// Route alias when the client hit a route (None = direct provider/model).
    pub route: Option<String>,
    /// Account (key) label; the served model stands in when None.
    pub account: Option<String>,
    pub served_model: String,
    /// The caller's identity (gateway key label); None renders as 本机.
    pub caller: Option<String>,
    /// Which client the request's User-Agent named; display wins over `caller`.
    pub agent: Option<String>,
    /// The Smart strategy's reason for choosing this model, carried from the
    /// Start event ("" = no reason).
    pub reason: String,
    /// How the attempt ended — colours the row (ok muted, error danger).
    pub status: EndStatus,
    /// The ledger's numbers for this attempt, as the End carried them: the
    /// settled row's speed reads them (output tokens over the decode window).
    pub metrics: EndMetrics,
    /// When End landed: the row's timestamp.
    pub settled_at: chrono::DateTime<chrono::Utc>,
    /// Start → End on the feed's own clock — the same clock the live row
    /// counted up, so a row does not jump when it settles. The ledger's
    /// `total_ms` (in [`Self::metrics`]) is what the speed divides by.
    pub duration: Duration,
}

/// The single feed entity: every event lands here, pages observe it.
pub(crate) struct ActivityFeed {
    /// In-flight attempts keyed by (origin, seq) — cross-process unique.
    live: HashMap<(u32, u64), LiveAttempt>,
    /// Insertion order, for stable rendering (newest last).
    order: Vec<(u32, u64)>,
    /// Finished attempts, newest first: a ring capped at [`SETTLED_CAP`].
    settled: VecDeque<SettledAttempt>,
    /// An event landed since the last ticker beat (drives the slow heal poll).
    dirty: bool,
    /// Bumped on Persisted (and the dirty ticker beat); pages refresh their
    /// ledger/aggregates.
    persisted: u64,
    /// Bumped whenever a dispatched attempt starts or resolves — i.e. when a
    /// key's concurrency slot is taken or released. Pages showing live
    /// occupancy reload on this (not `persisted`: a settled request has already
    /// freed its slot, so a post-hoc read always sees zero).
    slot_changes: u64,
    /// Live key-level occupancy mirrored from `slot` events (absolute counts).
    /// The dispatcher is the single owner of slot state; this is its value
    /// pushed to the client, not a client-side tally.
    key_inflight: HashMap<i64, u32>,
    /// Bumped when the stream (re)connected or lagged: any mirror may be
    /// stale, so pages do one authoritative re-read.
    resync: u64,
    /// The pushed bootstrap document (the subscription's connect frame):
    /// every parameterless page read — providers, keys, routes, rests, quota
    /// snapshots, gateway keys, model-success. Pages render their slice from
    /// it, so opening the client costs one connection instead of a request
    /// per page. None until the frame lands.
    bootstrap: Option<Bootstrap>,
    /// Bumped when the gateway announces a finished quota probe round: the
    /// snapshots in the database moved, so quota-showing pages re-read.
    quota_rev: u64,
    /// Bumped when a bootstrap lands: pages re-apply their slice.
    bootstrap_rev: u64,
}

impl ActivityFeed {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            live: HashMap::new(),
            order: Vec::new(),
            settled: VecDeque::new(),
            dirty: false,
            persisted: 0,
            slot_changes: 0,
            key_inflight: HashMap::new(),
            resync: 0,
            bootstrap: None,
            bootstrap_rev: 0,
            quota_rev: 0,
        };
        this.start_drain(cx);
        this.start_ticker(cx);
        this
    }

    /// In-flight attempts in arrival order.
    pub(crate) fn live(&self) -> impl Iterator<Item = &LiveAttempt> {
        self.order.iter().filter_map(|key| self.live.get(key))
    }

    /// Finished attempts, newest first (the settled ring).
    pub(crate) fn settled(&self) -> impl Iterator<Item = &SettledAttempt> {
        self.settled.iter()
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.live.is_empty()
    }

    pub(crate) fn persisted(&self) -> u64 {
        self.persisted
    }

    /// Slot-change revision: bumped per `slot` event, so a page that mirrors
    /// live concurrency swings repaints when it actually changed.
    pub(crate) fn slot_changes(&self) -> u64 {
        self.slot_changes
    }

    /// Live key-level occupancy, as last published by the dispatcher. Absolute
    /// counts: a key with no entry has not changed hands since the last resync.
    pub(crate) fn key_inflight(&self) -> &HashMap<i64, u32> {
        &self.key_inflight
    }

    /// Resync revision: bumped on a gap or a fresh subscription. Pages that
    /// depend on a mirror re-read their source once when this changes.
    pub(crate) fn resync(&self) -> u64 {
        self.resync
    }

    /// The pushed bootstrap document, if the connect frame has landed.
    pub(crate) fn bootstrap(&self) -> Option<&Bootstrap> {
        self.bootstrap.as_ref()
    }

    /// Bootstrap revision: bumped when a pushed document lands, so each page
    /// re-applies its slice exactly once per connect.
    pub(crate) fn bootstrap_rev(&self) -> u64 {
        self.bootstrap_rev
    }

    /// Quota-refresh revision: bumped per `QuotaRefreshed` event, so the
    /// account page re-reads its snapshots exactly when a probe round landed.
    pub(crate) fn quota_rev(&self) -> u64 {
        self.quota_rev
    }

    /// One synthetic persisted bump without an SSE event: used when the
    /// gateway link heals (unreachable → connected). The pump's first connect
    /// deliberately baselines without emitting Persisted, so without this the
    /// pages whose loads failed while the gateway was down would stay errored
    /// until real traffic lands.
    pub(crate) fn notify_persisted(&mut self, cx: &mut Context<Self>) {
        self.persisted += 1;
        cx.notify();
    }

    /// Apply one wire event. Unknown ids are ignored (events can arrive for
    /// attempts whose Start predates this listener or got dropped). Persisted
    /// bumps its counter immediately — that is the whole point of the event
    /// (pages refresh the moment rows land, not at the next ticker beat).
    fn apply(&mut self, event: ActivityEvent, cx: &mut Context<Self>) {
        // The connect frame: the whole page snapshot, pushed once. Store it,
        // seed the occupancy mirror from it (so a fresh subscription needs no
        // admin round-trip), and stop — nothing else consumes it.
        let event = match event {
            ActivityEvent::Bootstrap(document) => {
                self.key_inflight = document
                    .keys
                    .iter()
                    .filter_map(|key| {
                        key.inflight
                            .map(|inflight| (key.id, inflight.max(0) as u32))
                    })
                    .collect();
                self.bootstrap = Some(*document);
                self.bootstrap_rev += 1;
                self.slot_changes += 1;
                cx.notify();
                return;
            }
            other => other,
        };
        match &event {
            ActivityEvent::Persisted => {
                self.persisted += 1;
                cx.notify();
                return;
            }
            ActivityEvent::Slot { key_id, inflight } => {
                // Absolute value straight from the slot owner: overwrite, never
                // accumulate, so one dropped frame cannot skew the mirror.
                self.key_inflight.insert(*key_id, *inflight);
                self.slot_changes += 1;
                cx.notify();
                return;
            }
            ActivityEvent::QuotaRefreshed => {
                self.quota_rev += 1;
                cx.notify();
                return;
            }
            ActivityEvent::Resync => {
                // Gap or fresh subscription: drop the mirror and let the pages
                // re-read once (the ledger pages ride `persisted`).
                self.key_inflight.clear();
                self.persisted += 1;
                self.resync += 1;
                cx.notify();
                return;
            }
            _ => {}
        }
        self.dirty |= apply_to(&mut self.live, &mut self.order, &mut self.settled, event);
        cx.notify();
    }

    /// Drain the gateway's activity subscription until the feed (or the
    /// channel) dies. No subscription (a broken client build) leaves the feed
    /// silent — the pages still work, just without the live block.
    fn start_drain(&mut self, cx: &mut Context<Self>) {
        let take = cx.global::<GatewayRuntime>().take_activity();
        cx.spawn(async move |this, cx| {
            let Some(mut rx) = take.await else { return };
            while let Some(event) = rx.recv().await {
                if this.update(cx, |feed, cx| feed.apply(event, cx)).is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// Safety net: every POLL interval, sweep ghosts (re-rendering the live
    /// block and aging its labels) and — only if events actually landed —
    /// bump `persisted` so observers re-read their ledger view (heals dropped
    /// events). An idle feed costs zero queries.
    fn start_ticker(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                let alive = this
                    .update(cx, |feed, cx| {
                        let swept =
                            sweep(&mut feed.live, &mut feed.order, Instant::now() - LIVE_TTL);
                        let had_events = feed.dirty;
                        if had_events {
                            feed.dirty = false;
                            feed.persisted += 1;
                        }
                        // Re-render when there is something to show (or something just
                        // left); an idle feed needs no repaint.
                        if had_events || swept || !feed.live.is_empty() {
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    return;
                }
            }
        })
        .detach();
    }
}

/// Sweep live rows whose last event is older than `cutoff` (their End was
/// lost). Returns whether anything was removed.
fn sweep(
    live: &mut HashMap<(u32, u64), LiveAttempt>,
    order: &mut Vec<(u32, u64)>,
    cutoff: Instant,
) -> bool {
    let stale: Vec<(u32, u64)> = live
        .iter()
        .filter(|(_, attempt)| attempt.last_seen < cutoff)
        .map(|(key, _)| *key)
        .collect();
    for key in &stale {
        live.remove(key);
    }
    if !stale.is_empty() {
        order.retain(|key| !stale.contains(key));
    }
    !stale.is_empty()
}

/// The publisher truncates user-controlled strings to the wire budget
/// (char-boundary safe) before they go out; compare wire values against the
/// equally truncated local form, or a long label/model id silently never
/// matches. The rule itself lives with the publisher
/// ([`gateway::activity::clip_len`]) — borrowed rather than copied, so the two
/// sides cannot drift. Borrowed on purpose: the stage compares every route
/// entry against every live attempt on its 66ms beat, and a `String` here
/// would allocate per frame.
pub(crate) fn clipped(text: &str) -> &str {
    &text[..gateway::activity::clip_len(text)]
}

/// Client name for a caller identity pair. Both present → `caller-agent`
/// ("john-opencode": who they are + which client they used). Only agent →
/// agent alone; only caller → caller (prefix-stripped). Neither → 本机.
pub(crate) fn named_client(agent: Option<&str>, caller: Option<&str>) -> String {
    let agent = agent.filter(|a| !a.is_empty());
    let caller = caller.filter(|c| !c.is_empty()).map(client_of);
    match (caller, agent) {
        (Some(caller), Some(agent)) if caller != agent => {
            clipped(&format!("{caller}-{agent}")).to_string()
        }
        (Some(caller), _) => clipped(caller).to_string(),
        (None, Some(agent)) => clipped(agent).to_string(),
        (None, None) => "本机".to_string(),
    }
}

/// Client name of a caller identity. The agent is derived from the API
/// token's suffix (`cardinal-opencode` → `opencode`):
/// cardinal follows that rule on the caller key's VALUE: the auth hoop
/// passes a `cardinal-`-prefixed key value through as the identity (see
/// api.rs gateway_auth), and this strips the prefix. A registered key without
/// the prefix carries its stored label instead, which shows as-is. Two
/// identities deriving to the same name are one client.
pub(crate) fn client_of(caller: &str) -> &str {
    caller
        .strip_prefix("cardinal-")
        .filter(|rest| !rest.is_empty())
        .unwrap_or(caller)
}

/// Known clients keep their maker's colour.
const CLIENT_HUES: [(&str, u32); 14] = [
    ("claude", 0xd97757),
    ("codex", 0x6366f1),
    ("gemini", 0x0ea5e9),
    ("copilot", 0xa855f7),
    ("cursor", 0x14b8a6),
    ("opencode", 0xeab308),
    ("crush", 0xec4899),
    ("goose", 0x84cc16),
    ("pi", 0x10b981),
    ("omp", 0xf43f5e),
    ("omo", 0x7c3aed),
    ("dsh", 0x06b6d4),
    ("commandcode", 0xf97316),
    ("mimocode", 0x3b82f6),
];
/// Unknown clients hash into this spare palette.
const CLIENT_HUES_SPARE: [u32; 8] = [
    0x8b5cf6, 0x22c55e, 0xe11d48, 0x0891b2, 0xca8a04, 0xdb2777, 0x2563eb, 0x65a30d,
];

/// The stable hue of a client:
/// named agents keep their maker's colour, the rest hash into the spare
/// palette with the same h·31+c hash, so a client's colour survives restarts.
/// Client identity lives here, next to [`named_client`].
pub(crate) fn client_hue(client: &str) -> Hsla {
    let rgb = CLIENT_HUES
        .iter()
        .find_map(|(name, rgb)| (*name == client).then_some(*rgb))
        .unwrap_or_else(|| {
            let mut hash: u32 = 0;
            for byte in client.bytes() {
                hash = hash.wrapping_mul(31).wrapping_add(u32::from(byte));
            }
            CLIENT_HUES_SPARE[(hash % CLIENT_HUES_SPARE.len() as u32) as usize]
        });
    gpui::rgb(rgb).into()
}

/// Event application, context-free so the state machine is testable (the
/// entity wrapper just notifies). A Start inserts into `live`; First/Progress
/// mutate it; the End moves it into `settled` (newest first, capped at
/// [`SETTLED_CAP`]) instead of dropping it. Returns whether the state changed.
fn apply_to(
    live: &mut HashMap<(u32, u64), LiveAttempt>,
    order: &mut Vec<(u32, u64)>,
    settled: &mut VecDeque<SettledAttempt>,
    event: ActivityEvent,
) -> bool {
    match event {
        ActivityEvent::Start(event) => {
            let StartEvent {
                origin,
                seq,
                client_model,
                route,
                account,
                served_model,
                caller,
                agent,
                key_id,
                reason,
                stream,
                ..
            } = *event;
            let now = Instant::now();
            live.insert(
                (origin, seq),
                LiveAttempt {
                    client_model,
                    route,
                    account,
                    served_model,
                    reason: reason.into_string(),
                    caller,
                    agent,
                    key_id,
                    stream,
                    streaming: false,
                    started: now,
                    last_seen: now,
                    bytes: 0,
                },
            );
            order.push((origin, seq));
            true
        }
        ActivityEvent::First { origin, seq } => {
            if let Some(attempt) = live.get_mut(&(origin, seq)) {
                attempt.streaming = true;
                attempt.last_seen = Instant::now();
                return true;
            }
            false
        }
        ActivityEvent::Progress {
            origin,
            seq,
            bytes,
            frames: _,
        } => {
            if let Some(attempt) = live.get_mut(&(origin, seq)) {
                attempt.bytes = bytes;
                attempt.last_seen = Instant::now();
                return true;
            }
            false
        }
        ActivityEvent::End {
            origin,
            seq,
            status,
            metrics,
        } => {
            // No prior Start (its Start predates the listener, or the sweep already
            // took the ghost): nothing to settle.
            let Some(attempt) = live.remove(&(origin, seq)) else {
                return false;
            };
            order.retain(|key| *key != (origin, seq));
            settled.push_front(attempt.into_settled(status, metrics, Instant::now()));
            settled.truncate(SETTLED_CAP);
            true
        }
        ActivityEvent::Persisted => false,
        // Slot / Resync / Bootstrap are applied to the occupancy mirror (and
        // the pushed document) in `apply` before this runs; they carry
        // nothing for the live/settled state machine.
        ActivityEvent::Slot { .. } | ActivityEvent::Resync | ActivityEvent::Bootstrap(_) => false,
        // QuotaRefreshed is applied in `apply` (it bumps `quota_rev`); the
        // live/settled state machine has nothing to do for it.
        ActivityEvent::QuotaRefreshed => false,
    }
}
