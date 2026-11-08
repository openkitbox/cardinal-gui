//! Keys page: one card per upstream API key, read and written live through
//! the gateway's HTTP admin API. Each card shows the provider logo, the
//! masked credential, the vendor-probed quota (plan windows or a balance
//! line) and the row actions. Providers are NOT user-managed here: they come
//! from a hardcoded catalog ([`catalog::PROVIDER_SPECS`]) — adding a key
//! auto-creates its provider row with the catalog's fixed
//! endpoint/protocol/auth-scheme when missing. Quota renders from the STORED
//! snapshot on load (instant), then a background vendor probe
//! ([`ProvidersPage::refresh_quotas`]) refreshes the cards and writes the
//! snapshot back; a key without a reading renders no meter.
//!
//! Writes go through `Admin`'s write methods; every successful write triggers
//! a reload, and failures surface either in the form card (validation) or in
//! a page-level error card (row-level operations).
//!
//! Layout: page state + behavior here; `catalog` = the provider catalog and
//! variant resolution (pure data/logic), `quota` = quota view models,
//! `balance` = the prepaid balance meter + depletion chart, `cards` = the card
//! rendering.

mod balance;
mod cards;
mod catalog;
mod chip;
mod group_editor;
mod groups;
pub mod kiro_import;
mod order;
mod quota;
mod trend;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AppContext as _, Context, Entity, Focusable as _, InteractiveElement, IntoElement,
    ParentElement, Render, Styled, Window, prelude::FluentBuilder as _,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant};
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::component::{Disableable, Icon, Sizable, WindowExt};

use gateway::admin_ops::Bootstrap;
use gateway::http_client::{
    CursorLoginRequest, CursorLoginStart, CursorLoginStatus, GatewayHttpClient, KeyUpdate,
    OpenaiLoginRequest, OpenaiLoginStart, OpenaiLoginStatus, OpencodeLoginRequest,
    OpencodeLoginStart, OpencodeLoginStatus,
};

use gateway::probe::ProbeModel;
use gateway::quota::KeyQuota;
use gateway::store::{KeyGroupView, ProviderInput, ProviderKeyView, ProviderRow, RestRow};

use cards::CardDrag;
use catalog::{
    AccountLogin, PROTOCOL_CHOICES, PROVIDER_SPECS, ProtocolChoice, ProviderSpec, Region,
    is_custom, protocol_choice, resolve,
};
use group_editor::{GroupCreate, GroupManager};
use groups::{GroupFilter, KeyFacts, Visibility};
use quota::KeyQuotaView;

use crate::pages::{
    LoadState, action_error_card, empty_card, error_card, loading_card, page_frame, refresh_button,
};
use crate::runtime::GatewayRuntime;

/// One key card: the provider row it belongs to, the key itself, and its
/// quota reading (`Some` from the stored snapshot on load, refreshed by the
/// background probe; stays `None` when the vendor exposes nothing and no
/// probe ever succeeded).
struct KeyCardData {
    provider: ProviderRow,
    /// The credential-free key view ([`ProviderKeyView`]) — the shape the
    /// pushed bootstrap carries. The plaintext is fetched only when the
    /// editor opens ([`ProvidersPage::open_key_editor`]).
    key: ProviderKeyView,
    quota: Option<KeyQuotaView>,
}

/// Loaded page data: every key across all providers in global position order,
/// plus the active cooldowns by key id (health info, shown on the cards) — one
/// key may carry several rows (whole-key plus per-model cooldowns).
struct ProvidersData {
    keys: Vec<KeyCardData>,
    rests: HashMap<i64, Vec<RestRow>>,
    /// Account groups (id order) with their member key ids.
    groups: Vec<KeyGroupView>,
}

impl ProvidersData {
    /// Flatten the pushed bootstrap document into this page's shape: one card
    /// per provider key (global (position, id) order), stamped with its stored
    /// quota reading, plus the cooldowns grouped by key. Pure; the caller owns
    /// the document.
    fn from_bootstrap(document: &Bootstrap) -> Self {
        let providers: HashMap<i64, &ProviderRow> =
            document.providers.iter().map(|row| (row.id, row)).collect();
        let mut snapshots: HashMap<i64, (KeyQuota, chrono::DateTime<chrono::Utc>)> = document
            .quota_snapshots
            .iter()
            .map(|row| (row.key_id, (row.quota.clone(), row.fetched_at)))
            .collect();
        // Daily probe samples per key, for the cards' trend charts (absent on an
        // older gateway — the charts then show the current point alone).
        let mut histories: HashMap<i64, Vec<gateway::store::QuotaHistoryPoint>> = HashMap::new();
        for point in &document.quota_history {
            histories
                .entry(point.key_id)
                .or_default()
                .push(point.clone());
        }
        // The current balance segment per key (opener first), for the cards'
        // depletion charts.
        let mut balance_histories: HashMap<i64, Vec<gateway::store::BalanceHistoryPoint>> =
            HashMap::new();
        for point in &document.balance_history {
            balance_histories
                .entry(point.key_id)
                .or_default()
                .push(point.clone());
        }
        // A key always has its provider (same document, same transaction-free
        // read; the row carries a FK) — a key without one is skipped rather
        // than rendered without its provider.
        let mut keys: Vec<KeyCardData> = document
            .keys
            .iter()
            .filter_map(|key| {
                let provider = providers.get(&key.provider_id)?;
                Some(KeyCardData {
                    provider: (*provider).clone(),
                    key: key.clone(),
                    quota: snapshots.remove(&key.id).map(|(quota, probed_at)| {
                        KeyQuotaView::new(
                            quota,
                            probed_at,
                            histories.get(&key.id).map(Vec::as_slice).unwrap_or(&[]),
                            balance_histories
                                .get(&key.id)
                                .map(Vec::as_slice)
                                .unwrap_or(&[]),
                        )
                    }),
                })
            })
            .collect();
        keys.sort_by(|a, b| {
            a.key
                .position
                .cmp(&b.key.position)
                .then(a.key.id.cmp(&b.key.id))
        });
        let mut rests: HashMap<i64, Vec<RestRow>> = HashMap::new();
        for rest in &document.rests {
            rests.entry(rest.key_id).or_default().push(rest.clone());
        }
        // One boundary copy: the document is borrowed, the page owns its data.
        Self {
            keys,
            rests,
            groups: document.key_groups.clone(),
        }
    }

    /// The filter-relevant facts of every account, in card order.
    fn key_facts(&self) -> impl Iterator<Item = KeyFacts> + '_ {
        self.keys.iter().map(|card| KeyFacts {
            id: card.key.id,
            provider_id: card.provider.id,
            archived: card.key.archived_at.is_some(),
        })
    }

    /// The stored name of the provider row `id`, taken from any of its
    /// accounts' cards; `None` when no account of it is loaded.
    fn provider_name(&self, id: i64) -> Option<&str> {
        self.keys
            .iter()
            .find(|card| card.provider.id == id)
            .map(|card| card.provider.name.as_str())
    }
}

/// Where the add form's provider parameters come from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FormSource {
    /// Index into `PROVIDER_SPECS`: fixed endpoint/protocol/auth scheme.
    Catalog(usize),
    /// A user-defined provider: the chosen protocol plus the typed base_url;
    /// the label doubles as the provider name.
    Custom { protocol: ProtocolChoice },
}

impl FormSource {
    /// The Kiro catalog entry, which uses the JSON import instead of an API key.
    fn is_kiro(self) -> bool {
        matches!(self, FormSource::Catalog(index) if PROVIDER_SPECS[index].name == "kiro")
    }

    /// The catalog entry this source names, if it is one.
    fn spec(self) -> Option<&'static ProviderSpec> {
        match self {
            FormSource::Catalog(index) => Some(&PROVIDER_SPECS[index]),
            FormSource::Custom { .. } => None,
        }
    }

    /// The login flow of a catalog entry whose credential can be an account
    /// login rather than a pasted API key — see
    /// [`catalog::ProviderSpec::login`].
    fn account_login(self) -> Option<AccountLogin> {
        self.spec().and_then(|spec| spec.login)
    }

    /// Whether this source offers an account login at all.
    fn is_account_login(self) -> bool {
        self.account_login().is_some()
    }

    /// The login block this source needs: a fresh idle session owned by the
    /// form generation that is opening it.
    fn login_form(self, generation: u64) -> Option<AccountLoginForm> {
        self.account_login().map(|kind| AccountLoginForm {
            kind,
            progress: LoginProgress::Idle,
            generation,
        })
    }
}

/// How the add form authenticates a vendor that offers both (OpenCode Go): the
/// console account login is the default, a pasted API key the alternative.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CredentialMode {
    AccountLogin,
    ApiKey,
}

impl CredentialMode {
    /// The dropdown trigger's label and the matching menu line.
    fn label(self) -> &'static str {
        match self {
            CredentialMode::AccountLogin => "账号登录",
            CredentialMode::ApiKey => "API Key",
        }
    }
}

/// The provider row parameters the add form submits, copied out of the
/// inputs once at submit time so the async call can own them.
#[derive(Clone, PartialEq, Eq)]
struct ProviderTarget {
    name: String,
    protocol: &'static str,
    base_url: String,
    /// `None` lets the gateway infer the scheme from the protocol.
    auth_scheme: Option<&'static str>,
}

impl ProviderTarget {
    fn input(&self) -> ProviderInput<'_> {
        ProviderInput {
            name: &self.name,
            protocol: self.protocol,
            base_url: &self.base_url,
            auth_scheme: self.auth_scheme,
            enabled: true,
        }
    }
}

/// Add-key form: pick a provider from the catalog (with 区域 × Coding Plan
/// variants for kimi/glm) or define a custom one, paste the credential.
struct KeyForm {
    source: FormSource,
    /// 区域（仅对有变体的厂商生效；默认国内）。
    region: Region,
    /// Coding Plan 入口（仅对有变体的厂商生效）。
    coding_plan: bool,
    label: Entity<InputState>,
    api_key: Entity<InputState>,
    /// Custom provider endpoint root (rendered only for `FormSource::Custom`).
    base_url: Entity<InputState>,
    /// Kiro uses a JSON import instead of the ordinary API-key field.
    kiro_import: Option<KiroImportForm>,
    /// An account-login vendor (OpenCode Go, Cursor) logs in through the
    /// gateway instead of an API key; the block owns that session's state.
    /// Present for the vendor whether or not the login is the chosen
    /// [`CredentialMode`].
    login: Option<AccountLoginForm>,
    /// Which credential the form writes for a vendor that offers both: the
    /// console login (default) or a pasted API key.
    credential_mode: CredentialMode,
    /// Optional per-key concurrency cap (empty = 不限).
    max_concurrency: Entity<InputState>,
    /// Models the last 获取模型 probe returned (None until probed).
    probed_models: Option<Vec<ProbeModel>>,
    /// Narrows the rendered chips to model ids containing this text. A pure
    /// view filter over `probed_models`: it never touches the selection, so
    /// clearing it brings every chip back. The input owns the text — nothing
    /// mirrors it — and every keystroke just re-renders.
    model_filter: Entity<InputState>,
    /// Model ids the user picked out of `probed_models`; empty = 全部模型.
    selected_models: Vec<String>,
    /// A 获取模型 probe is in flight.
    probing: bool,
    /// Bumped every time a probe starts; a landing whose sequence is no
    /// longer current is stale and ignored.
    probe_seq: u64,
    /// A save is in flight (validating the key upstream, then persisting).
    saving: bool,
    error: Option<String>,
}

impl KeyForm {
    /// Whether this form's one write is the account login rather than a pasted
    /// API key: a vendor that supports a login, with the login chosen.
    fn uses_account_login(&self) -> bool {
        self.source.is_account_login() && self.credential_mode == CredentialMode::AccountLogin
    }

    /// Derive the provider parameters from the source: catalog entries
    /// resolve their variant, custom providers take the typed label (as the
    /// name) and base_url. Err carries the message for the form.
    fn target(&self, cx: &gpui::App) -> Result<ProviderTarget, &'static str> {
        match self.source {
            FormSource::Catalog(index) => {
                let spec = &PROVIDER_SPECS[index];
                let (name, base_url) = resolve(spec, self.region, self.coding_plan);
                Ok(ProviderTarget {
                    name: name.to_string(),
                    protocol: spec.protocol,
                    base_url: base_url.to_string(),
                    auth_scheme: Some(spec.auth_scheme),
                })
            }
            FormSource::Custom { protocol } => {
                let label = self.label.read(cx).value();
                let name = label.trim();
                if name.is_empty() {
                    return Err("自定义 provider 的 label 不能为空（同时作为 provider 名）");
                }
                if name.contains('/') {
                    return Err("label 同时作为 provider 名，不能包含 /");
                }
                let base_url = self.base_url.read(cx).value();
                let base_url = base_url.trim();
                if base_url.is_empty() {
                    return Err("base_url 不能为空");
                }
                Ok(ProviderTarget {
                    name: name.to_string(),
                    protocol: protocol.protocol,
                    base_url: base_url.to_string(),
                    auth_scheme: None,
                })
            }
        }
    }
}

/// State for the Kiro JSON import boundary. The raw document and parsed
/// credentials are cleared before the async write begins, so secrets never
/// remain in the form after either a successful or failed import.
struct KiroImportForm {
    raw_json: Entity<TextareaState>,
    provider_name: Entity<InputState>,
    /// The label of a single-account document, or the `{prefix}-NNN` prefix
    /// of a multi-account one.
    key_label: Entity<InputState>,
    parsed: Option<kiro_import::ParsedKiroExport>,
    parsed_raw_json: Option<String>,
    default_provider_name: String,
    /// Groups every imported key joins; empty = none.
    selected_groups: Vec<i64>,
    /// The running (or last finished) import; `None` before the first save.
    progress: Option<ImportProgress>,
    generation: u64,
}

/// Progress of one Kiro import run. Failure lines carry the label and the
/// gateway's error text only, never credential content.
struct ImportProgress {
    done: usize,
    total: usize,
    /// `(label, error)` per failed account, sorted by label.
    failures: Vec<(String, String)>,
}

/// What a finished import run reports back to the form.
struct ImportOutcome {
    /// Accounts the run attempted; more than `failures.len()` means at
    /// least one account landed.
    total: usize,
    failures: Vec<(String, String)>,
    /// Set when the provider row was created by this run, every account
    /// failed, and removing the now-empty row failed too.
    cleanup_error: Option<String>,
}

/// Accounts imported in parallel within one run (each is one gateway POST
/// that may refresh a token upstream).
const KIRO_IMPORT_CONCURRENCY: usize = 4;

/// State for one account login. The gateway owns the flow: it opens the
/// session with the vendor, polls it and writes the key row. This block only
/// shows what the user needs (the URL, and for a device flow the code) and
/// asks the gateway what the session's state is.
struct AccountLoginForm {
    /// Which vendor flow this is — decides the admin routes and the block's
    /// wording.
    kind: AccountLogin,
    progress: LoginProgress,
    /// The add form generation this login belongs to: a landing that arrives
    /// after the form was reopened or switched away is stale and ignored.
    generation: u64,
}

/// Where one login attempt stands.
enum LoginProgress {
    /// No session: the form's button starts one.
    Idle,
    /// The vendor opened a session and waits for the user to authorize it.
    Waiting {
        /// The gateway's session id (shared with the poll loop, never copied).
        session: Arc<str>,
        /// The code the user confirms on the console (a device flow); a
        /// browser login carries everything in its URL.
        user_code: Option<String>,
        /// The URL the user opens (prefilled code, or the challenge).
        verification_url: String,
        /// When the session started: the block reports the elapsed time.
        started: Instant,
        /// How often the block re-reads the session, in seconds: the
        /// console's own cadence for a device flow, 1 s for a browser login.
        interval: u64,
        /// How long the session stays open, from the start response.
        expires_in: u64,
    },
    /// The session ended in an error; the form stays open so the user can
    /// retry the login.
    Failed(String),
}

impl LoginProgress {
    /// The session this block is waiting on, if it is still waiting on one.
    fn session(&self) -> Option<&Arc<str>> {
        match self {
            LoginProgress::Waiting { session, .. } => Some(session),
            LoginProgress::Idle | LoginProgress::Failed(_) => None,
        }
    }

    /// Whether the block is waiting for the user to authorize in the browser.
    fn is_waiting(&self) -> bool {
        self.session().is_some()
    }
}

/// What starting a login gives the block, whichever flow opened it.
struct LoginStart {
    session: String,
    /// The device flow's code; a browser login has none.
    user_code: Option<String>,
    verification_url: String,
    expires_in: u64,
    /// The block's poll cadence in seconds.
    interval: u64,
}

impl From<OpencodeLoginStart> for LoginStart {
    fn from(start: OpencodeLoginStart) -> Self {
        Self {
            session: start.session,
            user_code: Some(start.user_code),
            verification_url: start.verification_url,
            expires_in: start.expires_in,
            interval: start.interval,
        }
    }
}

impl From<OpenaiLoginStart> for LoginStart {
    fn from(start: OpenaiLoginStart) -> Self {
        Self {
            session: start.session,
            user_code: Some(start.user_code),
            verification_url: start.verification_url,
            expires_in: start.expires_in,
            interval: start.interval,
        }
    }
}

impl From<CursorLoginStart> for LoginStart {
    fn from(start: CursorLoginStart) -> Self {
        Self {
            session: start.session,
            user_code: None,
            verification_url: start.login_url,
            expires_in: start.expires_in,
            interval: CURSOR_LOGIN_POLL_SECS,
        }
    }
}

/// How often the block re-reads a Cursor login: the gateway polls Cursor
/// every 500 ms, so a one-second read sees a landing within a second.
const CURSOR_LOGIN_POLL_SECS: u64 = 1;

/// One session state read, whichever flow it belongs to: the state word and
/// the gateway's display-safe failure message. The account facts a success
/// carries (key id, email, team) are not read here — the page reloads the
/// cards, which show them from the key row.
struct LoginState {
    state: String,
    message: Option<String>,
}

impl From<OpencodeLoginStatus> for LoginState {
    fn from(status: OpencodeLoginStatus) -> Self {
        Self {
            state: status.state,
            message: status.message,
        }
    }
}

impl From<OpenaiLoginStatus> for LoginState {
    fn from(status: OpenaiLoginStatus) -> Self {
        Self {
            state: status.state,
            message: status.message,
        }
    }
}

impl From<CursorLoginStatus> for LoginState {
    fn from(status: CursorLoginStatus) -> Self {
        Self {
            state: status.state,
            message: status.message,
        }
    }
}

/// Inline edit form for one existing key. The credential itself is NOT
/// editable — rotating a key is delete + re-create (#1248: position/cooldown
/// identity follow the new key row).
struct KeyEditor {
    provider_id: i64,
    key_id: i64,
    label: Entity<InputState>,
    /// Concurrency cap; empty = 不限 (PATCH clears the column).
    max_concurrency: Entity<InputState>,
    /// Model whitelist selection (prefilled from the key; empty = 全部模型).
    selected_models: Vec<String>,
    /// Group memberships being edited (prefilled from the loaded groups).
    selected_groups: Vec<i64>,
    /// The memberships the editor opened with: save submits `group_ids` only
    /// when `selected_groups` differs from this as a set.
    initial_groups: Vec<i64>,
    /// Models offered for picking: prefilled from the key's probe snapshot,
    /// refreshed by 重新获取.
    probed_models: Option<Vec<ProbeModel>>,
    /// Narrows the rendered chips to model ids containing this text. A pure
    /// view filter over `probed_models`: it never touches the selection, so
    /// clearing it brings every chip back. The input owns the text — nothing
    /// mirrors it — and every keystroke just re-renders.
    model_filter: Entity<InputState>,
    /// The revealed plaintext credential: `None` until the reveal lands
    /// (opening the editor fetches it), `Some("")` when the row stores none.
    /// The list shapes carry no credential, so this is its only source.
    api_key: Option<String>,
    /// A 重新获取 probe is in flight.
    probing: bool,
    /// A save is in flight (double-click guard, mirrors KeyForm.saving).
    saving: bool,
    /// Whether the read-only API key is shown in plaintext (masked by default).
    show_key: bool,
    /// Endpoint editing for a custom provider row (`catalog::is_custom`);
    /// `None` for catalog and Kiro rows.
    custom: Option<CustomEdit>,
    error: Option<String>,
}

/// The custom provider's editable endpoint plus the values the editor opened
/// with, so save writes only what changed. The label input doubles as the
/// provider name (name = label invariant).
struct CustomEdit {
    protocol: ProtocolChoice,
    base_url: Entity<InputState>,
    initial_name: String,
    initial_protocol: ProtocolChoice,
    initial_base_url: String,
}

/// The custom endpoint as typed, validated and copied out once at submit time.
struct CustomTarget {
    protocol: ProtocolChoice,
    base_url: String,
}

/// The provider PATCH a custom-row save sends (name = the key label).
struct ProviderWrite {
    protocol: &'static str,
    base_url: String,
    /// Carried over from the loaded row: the PATCH is a full replace.
    enabled: bool,
    /// The row's stored scheme while the protocol is unchanged; `None` after a
    /// protocol switch lets the gateway infer the scheme for the new protocol.
    auth_scheme: Option<String>,
    /// The revealed key, present when the endpoint changed and must be
    /// re-validated (and its model snapshot rewritten) before the PATCH.
    validate_with: Option<String>,
}

impl CustomEdit {
    /// Validate the typed name (the trimmed label) and base_url. Err carries
    /// the message for the editor card.
    fn target(&self, name: &str, cx: &gpui::App) -> Result<CustomTarget, &'static str> {
        if name.is_empty() {
            return Err("label 不能为空（同时作为 provider 名）");
        }
        if name.contains('/') {
            return Err("label 同时作为 provider 名，不能包含 /");
        }
        let base_url = self.base_url.read(cx).value();
        let base_url = base_url.trim();
        if base_url.is_empty() {
            return Err("base_url 不能为空");
        }
        Ok(CustomTarget {
            protocol: self.protocol,
            base_url: base_url.to_string(),
        })
    }

    /// Protocol or base_url differ from the stored row: the key must be
    /// re-validated against the new endpoint before anything is written.
    fn endpoint_changed(&self, target: &CustomTarget) -> bool {
        target.protocol != self.initial_protocol || target.base_url != self.initial_base_url
    }
}

/// Live card-drag state: which account is being dragged and over which card it
/// currently sits. Local view state only: a finished drop or a cancelled drag
/// clears it, and a stale leftover is healed by the page-level mouse-move
/// handler once `App::has_active_drag()` reports false (gpui has no drag-end
/// notification). `dragged` alone decides the faded source; the target pair
/// decides where the insert line shows.
pub(super) struct KeyDrag {
    dragged: i64,
    /// The drop target card and the side of it the card would land on
    /// ([`order::DropSide`]). `None` until the pointer is over a card.
    target: Option<(i64, order::DropSide)>,
}

/// Upstream API keys, one card each.
pub struct ProvidersPage {
    state: LoadState<ProvidersData>,
    /// Stale-overwrite guard: a reload started later always wins over one still in flight.
    generation: u64,
    form_generation: u64,
    key_form: Option<KeyForm>,
    key_editor: Option<KeyEditor>,
    /// Failure of a row-level write (toggle/archive/delete); shown above the list.
    action_error: Option<String>,
    /// Which accounts the 分组 dropdown admits (local view state).
    group_filter: GroupFilter,
    /// The 分组 dropdown list is open.
    group_list_open: bool,
    /// The 新建分组 card (mutually exclusive with `group_manager`).
    group_create: Option<GroupCreate>,
    /// The 管理分组 card (mutually exclusive with `group_create`).
    group_manager: Option<GroupManager>,
    /// Last seen activity-feed revisions: `slot_changes` (the dispatcher
    /// published a new occupancy) applies the pushed values locally;
    /// `bootstrap_rev` (a pushed document landed) re-applies the page slice;
    /// `resync` (a gap in the stream) re-reads the document once over HTTP.
    seen_slot_changes: u64,
    seen_resync: u64,
    seen_bootstrap: u64,
    /// Last seen `quota_rev`: a finished probe round re-reads the document.
    seen_quota: u64,
    /// Live card drag-and-drop state (see [`KeyDrag`]).
    drag: Option<KeyDrag>,
    /// A `PUT /api/keys/order` is in flight. While it is, a further drop is
    /// ignored: its rollback target would be an optimistic order the server
    /// never stored, so two overlapping drops could both fail and leave the
    /// grid on neither the served nor the requested order.
    order_write_in_flight: bool,
    /// Accounts whose email the card shows in clear: the eye on an account
    /// card toggles one entry. View state only — the address is already on the
    /// card, masked, and nothing is fetched to reveal it.
    revealed_accounts: HashSet<i64>,
}

impl ProvidersPage {
    pub fn new(feed: gpui::Entity<crate::activity::ActivityFeed>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            state: LoadState::Loading,
            generation: 0,
            form_generation: 0,
            key_form: None,
            key_editor: None,
            action_error: None,
            group_filter: GroupFilter::All,
            group_list_open: false,
            group_create: None,
            group_manager: None,
            seen_slot_changes: feed.read(cx).slot_changes(),
            seen_resync: feed.read(cx).resync(),
            seen_bootstrap: feed.read(cx).bootstrap_rev(),
            seen_quota: feed.read(cx).quota_rev(),
            drag: None,
            order_write_in_flight: false,
            revealed_accounts: HashSet::new(),
        };
        // The page renders from the PUSHED bootstrap document — providers,
        // every key (with its model snapshot), the stored quota readings and
        // the cooldowns. Opening the client therefore costs no request at all
        // (the document rides the activity stream's connect frame); `slot`
        // events keep occupancy live, and the 刷新 action or a `resync` gap
        // re-fetches the same document over HTTP.
        cx.observe(&feed, |page, feed, cx| {
            let bootstrap_rev = feed.read(cx).bootstrap_rev();
            if bootstrap_rev != page.seen_bootstrap {
                page.seen_bootstrap = bootstrap_rev;
                let live = feed.read(cx);
                if let Some(document) = live.bootstrap() {
                    page.state = LoadState::Loaded(ProvidersData::from_bootstrap(document));
                }
                cx.notify();
            }
            let resync = feed.read(cx).resync();
            if resync != page.seen_resync {
                page.seen_resync = resync;
                page.reload(cx);
            }
            // The gateway's background prober persisted fresh quota snapshots;
            // nothing else pushes them, so re-read (the old cards stay up).
            let quota_rev = feed.read(cx).quota_rev();
            if quota_rev != page.seen_quota {
                page.seen_quota = quota_rev;
                page.reload(cx);
            }
            let slot_changes = feed.read(cx).slot_changes();
            if slot_changes != page.seen_slot_changes {
                page.seen_slot_changes = slot_changes;
                // Apply in place while the feed is borrowed — no map clone.
                let live = feed.read(cx);
                let mut changed = false;
                if let LoadState::Loaded(data) = &mut page.state {
                    for card in &mut data.keys {
                        if let Some(&inflight) = live.key_inflight().get(&card.key.id) {
                            card.key.inflight = i32::try_from(inflight).ok();
                            changed = true;
                        }
                    }
                }
                if changed {
                    cx.notify();
                }
            }
        })
        .detach();
        // A document that landed before this page existed still applies.
        if let Some(document) = feed.read(cx).bootstrap() {
            this.state = LoadState::Loaded(ProvidersData::from_bootstrap(document));
        }
        this
    }

    /// Tab-row actions (the Status container renders them right-aligned next to
    /// the tabs): refresh reloads the list AND re-probes vendor quotas; 添加
    /// 账号 and 分组 only once data has loaded, since the form needs the
    /// provider list and the dropdown needs the groups.
    pub(crate) fn actions(&mut self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let refresh = refresh_button(
            "providers-refresh",
            cx.listener(|this, _, _, cx| {
                this.reload(cx);
                this.refresh_quotas(cx);
            }),
        );
        match &self.state {
            LoadState::Loaded(data) => {
                // A running import owns the form; replacing it would drop the
                // run's result.
                let saving = self.key_form.as_ref().is_some_and(|form| form.saving);
                // The dropdown only names the selection; it needs no visibility set.
                let filter = self.group_filter.effective(&data.groups, data.key_facts());
                vec![
                    Button::new("key-add")
                        .icon(Icon::new(IconName::Plus).small())
                        .label("添加账号")
                        .outline()
                        .compact()
                        .disabled(saving)
                        .on_click(cx.listener(|this, _, window, cx| this.open_key_form(window, cx)))
                        .into_any_element(),
                    self.group_dropdown(data, filter, cx),
                    refresh.into_any_element(),
                ]
            }
            _ => vec![refresh.into_any_element()],
        }
    }

    /// The group filter resolved against the loaded data, and the visibility it
    /// gives — the render boundary's single resolution. Derived per call, never
    /// cached: the page body and the tab-row actions are separate entry points
    /// (the Status container calls [`Self::actions`] on its own), so each
    /// resolves the part it needs.
    fn view_filter(&self, data: &ProvidersData) -> (GroupFilter, Visibility) {
        let filter = self.group_filter.effective(&data.groups, data.key_facts());
        let visibility = Visibility::new(filter, &data.groups, data.key_facts());
        (filter, visibility)
    }

    /// (Re)load the page snapshot over the admin API: ONE
    /// `GET /api/bootstrap` — the same document the activity stream pushes as
    /// its connect frame. Flattened into key cards in global (position, id)
    /// order. Snapshots render instantly — no vendor probing here; that lives
    /// in [`Self::refresh_quotas`], fired on manual refresh and after adding
    /// an account.
    pub(super) fn reload(&mut self, cx: &mut Context<Self>) {
        crate::pages::begin_load(&mut self.state);
        self.generation += 1;
        let generation = self.generation;
        let load = cx
            .global::<GatewayRuntime>()
            .admin_call(|admin| async move { admin.bootstrap().await });
        cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |page, cx| {
                if page.generation != generation {
                    return; // a newer reload is in flight; this snapshot is stale
                }
                match result {
                    Ok(document) => {
                        page.state = LoadState::Loaded(ProvidersData::from_bootstrap(&document))
                    }
                    Err(error) => page.state = LoadState::Failed(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Probe vendors in the background and merge the fresh readings by key id
    /// (a fresh reading stamps `now` as its probe time; the probe side also
    /// persists the snapshot for the next load). Failures are invisible — a key
    /// without a reading simply renders no meter. Merging is by key id into
    /// whatever list is loaded when the probe lands, so no generation guard is
    /// needed.
    /// Known transient: a reload that started before the probe lands can finish
    /// after it and briefly repaint the older snapshot — the probe already
    /// upserted the reading, so the next reload self-heals. A generation guard
    /// here would not help (the probe is intentionally generation-free so a
    /// slow vendor cannot be orphaned by an unrelated reload).
    fn refresh_quotas(&mut self, cx: &mut Context<Self>) {
        let load = cx
            .global::<GatewayRuntime>()
            .admin_call(|admin| async move { admin.quotas().await });
        cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |page, cx| {
                let (Ok(quotas), LoadState::Loaded(data)) = (result, &mut page.state) else {
                    return;
                };
                let mut quotas: HashMap<i64, KeyQuota> = quotas
                    .into_iter()
                    .map(|row| (row.key_id, row.quota))
                    .collect();
                let now = chrono::Utc::now();
                for card in &mut data.keys {
                    if let Some(quota) = quotas.remove(&card.key.id) {
                        card.quota = Some(match card.quota.take() {
                            Some(previous) => previous.live(quota, now),
                            None => KeyQuotaView::new(quota, now, &[], &[]),
                        });
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Run one write against the HTTP admin API: on success clear the error
    /// and reload; on failure show the message above the list.
    fn write<F, Fut>(&mut self, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(GatewayHttpClient) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), gateway::http_client::ClientError>>
            + Send
            + 'static,
    {
        crate::pages::run_write(cx, f);
    }

    /// Begin dragging one account's card: its grip was grabbed. The card is a
    /// drag source and every card is a drop target, so this only records the
    /// source; the target follows the pointer ([`Self::aim_card_drop`]). The
    /// card's label comes back for the drag ghost, looked up once here rather
    /// than cloned on every render of every card.
    fn start_card_drag(&mut self, provider_id: i64, key_id: i64, cx: &mut Context<Self>) -> String {
        self.drag = Some(KeyDrag {
            dragged: key_id,
            target: None,
        });
        cx.notify();
        self.key_label(provider_id, key_id).unwrap_or_default()
    }

    /// The pointer moved over card `target` during a drag: remember which side
    /// of that card the drop would land on. Repaint only when the aimed pair
    /// actually changes — the listener fires on every mouse move.
    fn aim_card_drop(&mut self, target: i64, side: order::DropSide, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.as_mut() else {
            return;
        };
        let aimed = Some((target, side));
        if drag.target == aimed {
            return;
        }
        drag.target = aimed;
        cx.notify();
    }

    /// The card carrying `dragged` was dropped: move it to the aimed slot, then
    /// write the resulting global order back to the gateway. The cards move
    /// first (optimistic), so a failed write restores the order loaded before
    /// the drop — see [`Self::submit_key_order`].
    fn finish_card_drop(&mut self, dragged: i64, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        // The drop is over: the faded source and the insert line go whatever
        // happens next.
        cx.notify();
        // One reorder write at a time: `submit_key_order`'s rollback restores
        // the order the server had when the drop was made, so a second drop
        // taken while the first is still in flight could roll back onto an
        // order the server never stored. The drop is refused and the refusal
        // is stated, so the drag does not just appear to do nothing.
        if self.order_write_in_flight {
            self.action_error = Some("上一步排序仍在保存，请稍后再拖".to_string());
            cx.notify();
            return;
        }
        if drag.dragged != dragged {
            return; // a stale payload, not the card this page is dragging
        }
        let Some((target, side)) = drag.target else {
            return;
        };
        let LoadState::Loaded(data) = &self.state else {
            return;
        };
        // Only 全部 allows reordering (its cards are the only ones wired to
        // drag at all); a drop arriving under any other filter is refused
        // rather than turned into a write.
        let (filter, _) = self.view_filter(data);
        if !filter.allows_reorder() {
            return;
        }
        let all: Vec<i64> = data.keys.iter().map(|card| card.key.id).collect();
        let visible = self.visible_ids(data);
        let order = order::reorder_visible(&all, &visible, dragged, target, side);
        // Dropped on itself or somewhere the move cannot be expressed: the
        // global order is unchanged, so no write is fired.
        if order == all {
            return;
        }
        let previous = all;
        self.apply_key_order(&order);
        cx.notify();
        self.submit_key_order(cx, order, previous);
    }

    /// The account ids the current filter shows, in the global (position, id)
    /// order the grid renders them — the same [`Visibility::admits`] predicate
    /// the render uses, so a drop moves exactly the cards the user sees.
    fn visible_ids(&self, data: &ProvidersData) -> Vec<i64> {
        let (_, visibility) = self.view_filter(data);
        data.keys
            .iter()
            .map(|card| card.key.id)
            .filter(|id| visibility.admits(*id))
            .collect()
    }

    /// Re-seat the loaded cards in `order` — a permutation of their ids — so
    /// the grid immediately shows the new sequence. The cards are moved into
    /// their slots (a sort by target rank), never copied; the same call runs
    /// the optimistic move and its rollback.
    fn apply_key_order(&mut self, order: &[i64]) {
        let LoadState::Loaded(data) = &mut self.state else {
            return;
        };
        let rank: HashMap<i64, usize> = order
            .iter()
            .enumerate()
            .map(|(slot, id)| (*id, slot))
            .collect();
        // An id outside `order` cannot happen (`reorder_visible` returns a
        // permutation or the input unchanged); it would sort last.
        data.keys
            .sort_by_key(|card| rank.get(&card.key.id).copied().unwrap_or(usize::MAX));
    }

    /// Write a dropped order back as the gateway's global key order. This is
    /// the one write that cannot go through
    /// [`crate::pages::run_write`]: the cards already moved, so a failure has
    /// to restore `previous` (the order loaded before the drop) or the page
    /// would keep showing an order the server never stored. Failures still
    /// surface in the page's action-error slot; success clears it and reloads,
    /// and the bootstrap brings the new `position` values back.
    fn submit_key_order(&mut self, cx: &mut Context<Self>, order: Vec<i64>, previous: Vec<i64>) {
        self.order_write_in_flight = true;
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move { admin.reorder_keys(&order).await });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| match result {
                Ok(()) => {
                    page.order_write_in_flight = false;
                    page.action_error = None;
                    page.reload(cx);
                }
                Err(error) => {
                    page.order_write_in_flight = false;
                    page.apply_key_order(&previous);
                    page.action_error = Some(error);
                    // Re-read once as well: the rollback restores the order the
                    // page had at drop time, and a pushed bootstrap may have
                    // landed since. The reload is the authority; the error
                    // stays on screen either way.
                    page.reload(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// One key's loaded card (`None` if unknown, or once it left the list).
    /// Menus capture ids only and resolve through this at click time, because
    /// their entities are cached between opens and a captured row goes stale.
    fn key_row(&self, provider_id: i64, key_id: i64) -> Option<&KeyCardData> {
        let LoadState::Loaded(data) = &self.state else {
            return None;
        };
        data.keys
            .iter()
            .find(|card| card.provider.id == provider_id && card.key.id == key_id)
    }

    /// The key's enabled state as the page last loaded it (`None` if unknown).
    fn key_enabled(&self, provider_id: i64, key_id: i64) -> Option<bool> {
        self.key_row(provider_id, key_id)
            .map(|card| card.key.enabled)
    }

    /// Whether a 429 may cool the account down, as the page last loaded it
    /// (`None` if unknown). Read at click time like [`Self::key_enabled`].
    fn key_rest_on_429(&self, provider_id: i64, key_id: i64) -> Option<bool> {
        self.key_row(provider_id, key_id)
            .map(|card| card.key.rest_on_429)
    }

    /// Whether the key is archived as the page last loaded it (`None` if
    /// unknown). Read at click time like [`Self::key_enabled`].
    fn key_archived(&self, provider_id: i64, key_id: i64) -> Option<bool> {
        self.key_row(provider_id, key_id)
            .map(|card| card.key.archived_at.is_some())
    }

    /// The key's label as the page last loaded it (`None` if unknown): the
    /// delete confirmation names the account and the drag ghost shows it, both
    /// read at the moment they are needed rather than capturing one at render
    /// time.
    fn key_label(&self, provider_id: i64, key_id: i64) -> Option<String> {
        self.key_row(provider_id, key_id)
            .map(|card| card.key.label.clone())
    }

    /// Archive or unarchive one key. Same success/failure handling as
    /// [`Self::write`]; an unarchive additionally probes quotas, because the
    /// gateway skipped the key while it was archived and its reading is stale.
    fn set_key_archived(
        &mut self,
        provider_id: i64,
        key_id: i64,
        archived: bool,
        cx: &mut Context<Self>,
    ) {
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                admin
                    .update_key(
                        provider_id,
                        key_id,
                        &KeyUpdate {
                            archived: Some(archived),
                            ..Default::default()
                        },
                    )
                    .await
            });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| match result {
                Ok(()) => {
                    page.action_error = None;
                    page.reload(cx);
                    if !archived {
                        page.refresh_quotas(cx);
                    }
                }
                Err(error) => {
                    page.action_error = Some(error);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// The model-whitelist search input. A probe list can run to hundreds of
    /// entries (Cursor returns 270), so both pickers narrow their chips by
    /// typing; every keystroke re-renders the page so the chips follow the
    /// text. The text lives only in the input — the render reads it back.
    fn model_filter_input(window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("搜索模型"));
        cx.subscribe(&filter, |_, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                cx.notify();
            }
        })
        .detach();
        filter
    }

    /// Open the add-key form, defaulting to the first catalog provider. A
    /// form that is still saving is kept, so its run can land its result.
    fn open_key_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.key_form.as_ref().is_some_and(|form| form.saving) {
            return;
        }
        self.form_generation = self.form_generation.wrapping_add(1);
        let label = cx.new(|cx| InputState::new(window, cx).placeholder("label（如 主用）"));
        let api_key = cx.new(|cx| InputState::new(window, cx).placeholder("sk-…"));
        let base_url = cx.new(|cx| {
            InputState::new(window, cx).placeholder(PROTOCOL_CHOICES[0].base_url_placeholder)
        });
        let max_concurrency =
            cx.new(|cx| InputState::new(window, cx).placeholder("并发上限（空 = 不限）"));
        let focus = label.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        let source = FormSource::Catalog(0);
        self.key_form = Some(KeyForm {
            source,
            // Region/plan intentionally survive switching between vendors: they act
            // as a memory of the last choice (resolve ignores them for variant-less
            // vendors), not as per-vendor state.
            region: Region::Domestic,
            coding_plan: false,
            label,
            api_key,
            base_url,
            kiro_import: None,
            login: source.login_form(self.form_generation),
            credential_mode: CredentialMode::AccountLogin,
            max_concurrency,
            probed_models: None,
            model_filter: Self::model_filter_input(window, cx),
            selected_models: Vec::new(),
            probing: false,
            probe_seq: 0,
            saving: false,
            error: None,
        });
        cx.notify();
    }

    /// Switch the add form between catalog entries and custom protocols. Kiro
    /// gets its own import state and never enters the ordinary API-key
    /// validation path.
    fn select_form_provider(
        &mut self,
        source: FormSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let same_provider = self
            .key_form
            .as_ref()
            .is_some_and(|form| form.source == source);
        if same_provider {
            return;
        }
        self.form_generation = self.form_generation.wrapping_add(1);
        let Some(form) = &mut self.key_form else {
            return;
        };
        form.source = source;
        form.probed_models = None;
        form.selected_models.clear();
        form.probing = false;
        if let FormSource::Custom { protocol } = source {
            form.base_url.update(cx, |input, cx| {
                input.set_placeholder(protocol.base_url_placeholder, window, cx)
            });
        }
        form.kiro_import = if source.is_kiro() {
            let raw_json = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder("粘贴 Kiro 导出的 JSON（不会发送网络请求）")
            });
            let provider_name = cx.new(|cx| InputState::new(window, cx).default_value("kiro"));
            let key_label =
                cx.new(|cx| InputState::new(window, cx).placeholder("label（如 主用）"));
            Some(KiroImportForm {
                raw_json,
                provider_name,
                key_label,
                parsed: None,
                parsed_raw_json: None,
                default_provider_name: "kiro".to_string(),
                selected_groups: Vec::new(),
                progress: None,
                generation: self.form_generation,
            })
        } else {
            None
        };
        form.login = source.login_form(self.form_generation);
        form.credential_mode = CredentialMode::AccountLogin;
        // The account login labels the new key after the vendor; an empty field
        // would make the gateway create an unnamed account.
        if let Some(spec) = source.spec().filter(|spec| spec.login.is_some())
            && form.label.read(cx).value().trim().is_empty()
        {
            form.label
                .update(cx, |input, cx| input.set_value(spec.name, window, cx));
        }
        cx.notify();
    }

    /// Parse the Kiro export locally. This is intentionally synchronous and
    /// pure: malformed input must not create an HTTP request or expose values.
    fn parse_kiro_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &mut self.key_form else {
            return;
        };
        let Some(import) = &mut form.kiro_import else {
            return;
        };
        let raw_json = import.raw_json.read(cx).value();
        match kiro_import::parse(&raw_json) {
            Ok(parsed) => {
                // The provider row's endpoint is the accounts' API region, so
                // the default name follows it (not the OIDC `region`).
                let default_name = match parsed.api_region() {
                    Ok(api_region) => format!("kiro-{api_region}"),
                    Err(error) => {
                        import.parsed = None;
                        import.parsed_raw_json = None;
                        form.error = Some(error.to_string());
                        cx.notify();
                        return;
                    }
                };
                let current_name = import.provider_name.read(cx).value();
                if current_name.contains('/') {
                    import.parsed = None;
                    import.parsed_raw_json = None;
                    form.error = Some("Provider 名称不能包含 /".to_string());
                    cx.notify();
                    return;
                }
                if current_name.trim().is_empty() || current_name == import.default_provider_name {
                    import
                        .provider_name
                        .update(cx, |input, cx| input.set_value(&default_name, window, cx));
                }
                import.default_provider_name = default_name;
                let placeholder = if parsed.account_count() > 1 {
                    "label 前缀（自动编号 前缀-001…）"
                } else {
                    "label（如 主用）"
                };
                import.key_label.update(cx, |input, cx| {
                    input.set_placeholder(placeholder, window, cx)
                });
                import.parsed = Some(parsed);
                import.parsed_raw_json = Some(raw_json.to_string());
                import.progress = None;
                form.error = None;
            }
            Err(error) => {
                import.parsed = None;
                import.parsed_raw_json = None;
                form.error = Some(error.to_string());
            }
        }
        cx.notify();
    }

    /// Save the add-key form: validate the key upstream, then find-or-create
    /// the catalog provider row (or create the custom one — a duplicate name
    /// is an error, never a silent reuse), then create the key. Errors stay
    /// in the form.
    fn save_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.key_form else { return };
        if form.source.is_kiro() {
            self.save_kiro_import(window, cx);
            return;
        }
        // An account-login vendor has no key to validate or persist: the whole
        // write is the gateway's device flow (which the block polls).
        if form.uses_account_login() {
            self.start_account_login(cx);
            return;
        }
        if form.saving {
            return; // one save in flight at a time
        }
        let custom = matches!(form.source, FormSource::Custom { .. });
        let label = form.label.read(cx).value().trim().to_string();
        let api_key = form.api_key.read(cx).value().trim().to_string();
        let concurrency_raw = form.max_concurrency.read(cx).value();
        let max_concurrency = match Self::parse_concurrency(&concurrency_raw) {
            Ok(limit) => limit,
            Err(message) => {
                if let Some(form) = &mut self.key_form {
                    form.error = Some(message.to_string());
                }
                cx.notify();
                return;
            }
        };
        if label.is_empty() || api_key.is_empty() {
            if let Some(form) = &mut self.key_form {
                form.error = Some("label 与 api_key 均不能为空".to_string());
            }
            cx.notify();
            return;
        }
        let target = match form.target(cx) {
            Ok(target) => target,
            Err(message) => {
                if let Some(form) = &mut self.key_form {
                    form.error = Some(message.to_string());
                }
                cx.notify();
                return;
            }
        };
        let allowed_models: Option<Vec<String>> = if form.selected_models.is_empty() {
            None
        } else {
            Some(form.selected_models.clone())
        };
        let form_generation = self.form_generation;
        // Validate the key upstream before anything is persisted (the provider's
        // model list doubles as the liveness/auth proof); the provider row is
        // created (custom) or find-or-created (catalog) only after the key checks
        // out, and a custom row is deleted again if the key cannot be created.
        if let Some(form) = &mut self.key_form {
            form.saving = true;
            form.error = None;
        }
        cx.notify();
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                let input = target.input();
                let models = admin.validate_key(&input, &api_key).await?;
                // Custom provider ↔ key is one-to-one (name = label): creating
                // fails on a taken name instead of attaching to someone else's row.
                let (provider_id, provider_created) = if custom {
                    (admin.create_provider(&input).await?, true)
                } else {
                    (admin.find_or_create_provider(&input).await?, false)
                };
                let key_id = match admin
                    .create_key(
                        provider_id,
                        &label,
                        &api_key,
                        max_concurrency,
                        allowed_models,
                    )
                    .await
                {
                    Ok(key_id) => key_id,
                    Err(error) if provider_created => {
                        let cleanup_error =
                            cleanup_new_resources(&admin, provider_id, None, true, "创建 key 失败")
                                .await;
                        return Err(import_error("创建 key 失败", error, cleanup_error));
                    }
                    Err(error) => return Err(error),
                };
                // Keep the model list validation just fetched as the key's snapshot
                // (route derivation on /v1/models reads it); losing it only defers
                // derived attributes until the next successful probe — never fail the
                // save over it.
                if let Err(error) = admin.save_key_models(key_id, &models).await {
                    tracing::warn!("model snapshot of key {key_id} was not saved: {error}");
                }
                Ok(())
            });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| {
                let Some(form) = &mut page.key_form else {
                    return;
                };
                if page.form_generation != form_generation {
                    return;
                }
                form.saving = false;
                match result {
                    Ok(()) => {
                        page.key_form = None;
                        page.action_error = None;
                        page.reload(cx);
                        // The new account has no snapshot yet — probe right away so its
                        // card gets a reading without waiting for a manual refresh.
                        page.refresh_quotas(cx);
                    }
                    Err(error) => {
                        if let Some(form) = &mut page.key_form {
                            form.error = Some(error);
                        } else {
                            page.action_error = Some(error);
                        }
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn save_kiro_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.key_form else { return };
        let Some(import) = &form.kiro_import else {
            return;
        };
        if form.saving {
            return;
        }
        let Some(parsed_raw_json) = import.parsed_raw_json.as_deref() else {
            if let Some(form) = &mut self.key_form {
                form.error = Some("请先解析 JSON".to_string());
            }
            cx.notify();
            return;
        };
        let current_raw_json = import.raw_json.read(cx).value();
        if current_raw_json != parsed_raw_json {
            if let Some(form) = &mut self.key_form {
                form.error = Some("JSON 内容已变化或无效，请重新解析后再保存".to_string());
            }
            cx.notify();
            return;
        }
        let parsed = match kiro_import::parse_for_save(&current_raw_json) {
            Ok(parsed) => parsed,
            Err(_) => {
                if let Some(form) = &mut self.key_form {
                    form.error = Some("JSON 内容已变化或无效，请重新解析后再保存".to_string());
                }
                cx.notify();
                return;
            }
        };
        let provider_name = import.provider_name.read(cx).value().to_string();
        let label = import.key_label.read(cx).value().to_string();
        let (provider_name, label) =
            match kiro_import::validate_import_fields(&provider_name, &label) {
                Ok(values) => values,
                Err(message) => {
                    if let Some(form) = &mut self.key_form {
                        form.error = Some(message.to_string());
                    }
                    cx.notify();
                    return;
                }
            };
        let existing_keys: Option<&[KeyCardData]> = match &self.state {
            LoadState::Loaded(data) => Some(&data.keys),
            LoadState::Loading | LoadState::Failed(_) => None,
        };
        // Auto-numbering continues after the loaded labels; numbering blind
        // could hit an existing label, which the gateway upserts in place.
        if existing_keys.is_none() && parsed.account_count() > 1 {
            if let Some(form) = &mut self.key_form {
                form.error = Some("账号列表尚未加载，无法自动编号".to_string());
            }
            cx.notify();
            return;
        }
        let existing_labels = existing_keys
            .unwrap_or_default()
            .iter()
            .map(|card| card.key.label.as_str());
        let labels = match parsed.import_labels(&label, existing_labels) {
            Ok(labels) => labels,
            Err(message) => {
                if let Some(form) = &mut self.key_form {
                    form.error = Some(message.to_string());
                }
                cx.notify();
                return;
            }
        };
        let endpoint = match parsed.api_region() {
            Ok(api_region) => catalog::kiro_endpoint(api_region),
            Err(error) => {
                if let Some(form) = &mut self.key_form {
                    form.error = Some(error.to_string());
                }
                cx.notify();
                return;
            }
        };
        let max_concurrency = form.max_concurrency.read(cx).value();
        let max_concurrency = match Self::parse_concurrency(&max_concurrency) {
            Ok(Some(limit)) => Some(Some(limit)),
            // An omitted cap lets a new key use the server default (unlimited)
            // while preserving an existing key's cap during label-based upsert.
            Ok(None) => None,
            Err(message) => {
                if let Some(form) = &mut self.key_form {
                    form.error = Some(message.to_string());
                }
                cx.notify();
                return;
            }
        };
        // A group deleted since the page loaded would fail every item with
        // the same 400; refuse the batch up front instead.
        if !import.selected_groups.is_empty() {
            let live = match &self.state {
                LoadState::Loaded(data) => data.groups.as_slice(),
                LoadState::Loading | LoadState::Failed(_) => &[],
            };
            let existing = groups::retain_existing(&import.selected_groups, live);
            if existing.len() != import.selected_groups.len() {
                if let Some(form) = &mut self.key_form {
                    form.error = Some("所选分组已被删除，请刷新后重新选择".to_string());
                }
                cx.notify();
                return;
            }
        }
        let group_ids: Option<Arc<[i64]>> = if import.selected_groups.is_empty() {
            None
        } else {
            Some(Arc::from(import.selected_groups.as_slice()))
        };
        let form_generation = import.generation;
        let jobs: Vec<(String, kiro_import::ImportItem)> =
            labels.into_iter().zip(parsed.into_import_items()).collect();
        let total = jobs.len();
        if let Some(form) = &mut self.key_form {
            if let Some(import) = &mut form.kiro_import {
                import
                    .raw_json
                    .update(cx, |input, cx| input.set_value("", window, cx));
                import.parsed = None;
                import.parsed_raw_json = None;
                import.progress = Some(ImportProgress {
                    done: 0,
                    total,
                    failures: Vec::new(),
                });
            }
            form.saving = true;
            form.error = None;
        }
        cx.notify();
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                import_kiro_accounts(
                    admin,
                    &provider_name,
                    &endpoint,
                    jobs,
                    group_ids,
                    max_concurrency,
                    progress_tx,
                )
                .await
            });
        // Every finished account ticks the counter; the loop ends once the
        // import (and with it every sender) is gone.
        cx.spawn(async move |this, cx| {
            while progress_rx.recv().await.is_some() {
                let ticked = this.update(cx, |page, cx| {
                    if let Some(progress) = page
                        .kiro_import_mut(form_generation)
                        .and_then(|import| import.progress.as_mut())
                    {
                        progress.done = (progress.done + 1).min(progress.total);
                        cx.notify();
                    }
                });
                if ticked.is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| {
                page.finish_kiro_import(form_generation, result, cx)
            })
            .ok();
        })
        .detach();
    }

    /// The open Kiro import form, if it is still the one `generation` names.
    fn kiro_import_mut(&mut self, generation: u64) -> Option<&mut KiroImportForm> {
        self.key_form
            .as_mut()?
            .kiro_import
            .as_mut()
            .filter(|import| import.generation == generation)
    }

    /// Land a finished import run: a clean run closes the form; failures keep
    /// it open with the per-label failure lines. Any imported account
    /// triggers a reload and a quota probe, even when the form that started
    /// the run is gone — only the form update is skipped then.
    fn finish_kiro_import(
        &mut self,
        generation: u64,
        result: Result<ImportOutcome, String>,
        cx: &mut Context<Self>,
    ) {
        let imported_any = result
            .as_ref()
            .is_ok_and(|outcome| outcome.failures.len() < outcome.total);
        if imported_any {
            self.reload(cx);
            self.refresh_quotas(cx);
        }
        if self.kiro_import_mut(generation).is_some() {
            match result {
                Ok(outcome) if outcome.failures.is_empty() => {
                    self.key_form = None;
                    self.action_error = None;
                }
                Ok(outcome) => {
                    if let Some(form) = &mut self.key_form {
                        form.saving = false;
                        form.error = outcome.cleanup_error;
                        let progress = form
                            .kiro_import
                            .as_mut()
                            .and_then(|import| import.progress.as_mut());
                        if let Some(progress) = progress {
                            progress.done = progress.total;
                            progress.failures = outcome.failures;
                        }
                    }
                }
                Err(error) => {
                    if let Some(form) = &mut self.key_form {
                        form.saving = false;
                        form.error = Some(error);
                        if let Some(import) = &mut form.kiro_import {
                            import.progress = None;
                        }
                    }
                }
            }
        }
        cx.notify();
    }

    /// Start an account login (add form): find-or-create the provider row, ask
    /// the gateway to open a session with the vendor, then let the block poll
    /// it. The provider row is created here because a session needs a real
    /// provider id and the row may not exist yet.
    fn start_account_login(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.key_form else { return };
        if form.saving {
            return; // one write in flight at a time
        }
        let Some(kind) = form.login.as_ref().map(|login| login.kind) else {
            // The block that owns the session is missing (the source is not an
            // account-login one): fail closed rather than lock a form no login
            // can ever finish.
            return;
        };
        let label = form.label.read(cx).value().trim().to_string();
        if label.is_empty() {
            if let Some(form) = &mut self.key_form {
                form.error = Some("label 不能为空".to_string());
            }
            cx.notify();
            return;
        }
        let concurrency_raw = form.max_concurrency.read(cx).value();
        let max_concurrency = match Self::parse_concurrency(&concurrency_raw) {
            Ok(limit) => limit,
            Err(message) => {
                if let Some(form) = &mut self.key_form {
                    form.error = Some(message.to_string());
                }
                cx.notify();
                return;
            }
        };
        let target = match form.target(cx) {
            Ok(target) => target,
            Err(message) => {
                if let Some(form) = &mut self.key_form {
                    form.error = Some(message.to_string());
                }
                cx.notify();
                return;
            }
        };
        let generation = self.form_generation;
        if let Some(form) = &mut self.key_form {
            form.saving = true;
            form.error = None;
            if let Some(login) = &mut form.login {
                // A fresh attempt replaces whatever the block was showing.
                login.progress = LoginProgress::Idle;
                login.generation = generation;
            }
        }
        cx.notify();
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                let provider_id = admin.find_or_create_provider(&target.input()).await?;
                // The two bodies are one type; the route differs by flow.
                let request = OpencodeLoginRequest {
                    label,
                    key_id: None,
                    group_ids: Vec::new(),
                    max_concurrency,
                };
                match kind {
                    AccountLogin::OpencodeDevice => admin
                        .start_opencode_login(provider_id, &request)
                        .await
                        .map(LoginStart::from),
                    AccountLogin::CursorBrowser => {
                        let request: CursorLoginRequest = request;
                        admin
                            .start_cursor_login(provider_id, &request)
                            .await
                            .map(LoginStart::from)
                    }
                    // OpenAI's route takes the provider in the body.
                    AccountLogin::OpenaiDevice => {
                        let request = OpenaiLoginRequest {
                            provider_id: Some(provider_id),
                            label: request.label,
                            key_id: None,
                            group_ids: request.group_ids,
                            max_concurrency: request.max_concurrency,
                        };
                        admin
                            .start_openai_login(&request)
                            .await
                            .map(LoginStart::from)
                    }
                }
            });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| page.land_login_start(generation, result, cx))
                .ok();
        })
        .detach();
    }

    /// Show the session the gateway opened (or why it could not open one) and
    /// start polling it. The form stays locked meanwhile: the login is this
    /// form's one write, and switching providers underneath it would abandon a
    /// session that may still land an account.
    fn land_login_start(
        &mut self,
        generation: u64,
        result: Result<LoginStart, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(login) = self.login_mut(generation) else {
            return; // the form closed while the gateway was opening the session
        };
        let session = match result {
            Ok(start) => {
                let session: Arc<str> = Arc::from(start.session);
                // The URL already carries the code (or the challenge), so
                // handing it to the system browser saves the operator a
                // copy-and-paste round trip. The link (and code) stay on
                // screen for the case where no browser came up.
                cx.open_url(&start.verification_url);
                login.progress = LoginProgress::Waiting {
                    session: Arc::clone(&session),
                    user_code: start.user_code,
                    verification_url: start.verification_url,
                    started: Instant::now(),
                    interval: start.interval,
                    expires_in: start.expires_in,
                };
                session
            }
            Err(error) => {
                login.progress = LoginProgress::Failed(error);
                if let Some(form) = &mut self.key_form {
                    form.saving = false;
                }
                cx.notify();
                return;
            }
        };
        cx.notify();
        self.poll_account_login(generation, session, false, cx);
    }

    /// Poll the login session once: wait the block's interval, read the
    /// state and land it. The landing re-arms this until the session ends.
    fn poll_account_login(
        &mut self,
        generation: u64,
        session: Arc<str>,
        retry: bool,
        cx: &mut Context<Self>,
    ) {
        let Some((kind, interval)) =
            self.login_mut(generation)
                .and_then(|login| match &login.progress {
                    LoginProgress::Waiting { interval, .. } => Some((login.kind, *interval)),
                    LoginProgress::Idle | LoginProgress::Failed(_) => None,
                })
        else {
            return; // cancelled, switched or closed: nothing left to poll
        };
        let call_session = Arc::clone(&session);
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                match kind {
                    AccountLogin::OpencodeDevice => admin
                        .opencode_login_status(&call_session)
                        .await
                        .map(LoginState::from),
                    AccountLogin::CursorBrowser => admin
                        .cursor_login_status(&call_session)
                        .await
                        .map(LoginState::from),
                    AccountLogin::OpenaiDevice => admin
                        .openai_login_status(&call_session)
                        .await
                        .map(LoginState::from),
                }
            });
        cx.spawn(async move |this, cx| {
            // The vendor's own cadence (the gateway polls on the same or a
            // finer interval), so the read never races the flow it watches.
            cx.background_executor()
                .timer(Duration::from_secs(interval.max(1)))
                .await;
            let result = call.await;
            this.update(cx, |page, cx| {
                page.land_login_status(generation, session, result, retry, cx)
            })
            .ok();
        })
        .detach();
    }

    /// Apply one status read: waiting re-arms the poll, a failure keeps the
    /// form open with the gateway's message, and the terminal states end the
    /// session (`succeeded` closes the form like a save does).
    fn land_login_status(
        &mut self,
        generation: u64,
        session: Arc<str>,
        result: Result<LoginState, String>,
        retry: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.login_waiting(generation, &session) {
            return; // cancelled, switched or closed while the read was in flight
        }
        let status = match result {
            Ok(status) => status,
            // A dropped read is not a failed login: give it one more chance
            // before saying so — the user may be authorizing right now.
            Err(_) if !retry => {
                self.poll_account_login(generation, session, true, cx);
                return;
            }
            Err(error) => {
                self.fail_login(generation, error, cx);
                return;
            }
        };
        match status.state.as_str() {
            "waiting" => {
                // Repaint: the block's elapsed hint is derived at render time.
                cx.notify();
                self.poll_account_login(generation, session, false, cx);
            }
            "succeeded" => {
                // The gateway wrote the account, its model snapshot and the
                // routes: this form is done, exactly like a finished save.
                self.key_form = None;
                self.action_error = None;
                self.reload(cx);
                self.refresh_quotas(cx);
                cx.notify();
            }
            "failed" => {
                let message = status.message.unwrap_or_else(|| "登录失败".to_string());
                self.fail_login(generation, message, cx);
            }
            "expired" => self.reset_login(generation, "登录已过期，请重新开始", cx),
            "cancelled" => self.reset_login(generation, "登录已取消", cx),
            other => self.fail_login(generation, format!("未知的登录状态：{other}"), cx),
        }
    }

    /// End the block's session with a message it shows; the form stays open so
    /// the login can be retried.
    fn fail_login(&mut self, generation: u64, message: String, cx: &mut Context<Self>) {
        let Some(login) = self.login_mut(generation) else {
            return;
        };
        login.progress = LoginProgress::Failed(message);
        if let Some(form) = &mut self.key_form {
            form.saving = false;
        }
        cx.notify();
    }

    /// Return the block to idle with a hint on the form: the session ended
    /// without an account, so nothing was written.
    fn reset_login(&mut self, generation: u64, hint: &str, cx: &mut Context<Self>) {
        let Some(login) = self.login_mut(generation) else {
            return;
        };
        login.progress = LoginProgress::Idle;
        if let Some(form) = &mut self.key_form {
            form.saving = false;
            form.error = Some(hint.to_string());
        }
        cx.notify();
    }

    /// The open login block, if it is still the one `generation` names.
    fn login_mut(&mut self, generation: u64) -> Option<&mut AccountLoginForm> {
        self.key_form
            .as_mut()?
            .login
            .as_mut()
            .filter(|login| login.generation == generation)
    }

    /// Whether the block still waits on login session `session` — the poll
    /// loop's stop signal (cancel, provider switch and form close clear it).
    fn login_waiting(&self, generation: u64, session: &str) -> bool {
        let Some(login) = self.key_form.as_ref().and_then(|form| form.login.as_ref()) else {
            return false;
        };
        login.generation == generation
            && login
                .progress
                .session()
                .is_some_and(|current| &**current == session)
    }

    /// End the block's login session (the block's 取消): the gateway stops
    /// polling the vendor and forgets the session's secret, so no account can
    /// land from it.
    fn cancel_account_login(&mut self, cx: &mut Context<Self>) {
        let Some(login) = self.key_form.as_ref().and_then(|form| form.login.as_ref()) else {
            return;
        };
        let Some(session) = login.progress.session() else {
            return;
        };
        let session = Arc::clone(session);
        let generation = login.generation;
        let kind = login.kind;
        if let Some(form) = &mut self.key_form {
            if let Some(login) = &mut form.login {
                // The block stops showing the code at once: the exit is the
                // user's click, not the gateway's round trip.
                login.progress = LoginProgress::Idle;
            }
            form.saving = true;
            form.error = None;
        }
        cx.notify();
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                match kind {
                    AccountLogin::OpencodeDevice => admin.cancel_opencode_login(&session).await,
                    AccountLogin::CursorBrowser => admin.cancel_cursor_login(&session).await,
                    AccountLogin::OpenaiDevice => admin.cancel_openai_login(&session).await,
                }
            });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| {
                let Some(form) = page.key_form.as_mut() else {
                    return;
                };
                if page.form_generation != generation {
                    return;
                }
                form.saving = false;
                // A session that already ended answers 404; the block is idle
                // either way, and the message says what happened.
                if let Err(error) = result {
                    form.error = Some(error);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 获取模型 (add form): probe the typed credential against the chosen
    /// provider's model list — validation only, nothing is persisted — and
    /// offer the returned ids as selectable whitelist chips.
    fn probe_form_models(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.key_form else { return };
        if form.probing || form.saving {
            return;
        }
        let api_key = form.api_key.read(cx).value().trim().to_string();
        if api_key.is_empty() {
            if let Some(form) = &mut self.key_form {
                form.error = Some("请先填写 api_key".to_string());
            }
            cx.notify();
            return;
        }
        let target = match form.target(cx) {
            Ok(target) => target,
            Err(message) => {
                if let Some(form) = &mut self.key_form {
                    form.error = Some(message.to_string());
                }
                cx.notify();
                return;
            }
        };
        let Some(form) = &mut self.key_form else {
            return;
        };
        form.probing = true;
        form.probe_seq = form.probe_seq.wrapping_add(1);
        form.error = None;
        let probe_seq = form.probe_seq;
        cx.notify();
        // Identity guard inputs, captured before the await: the landing only
        // applies while the form still resolves to the same provider target and
        // the same api_key — a switched or reopened form does not receive it.
        let form_generation = self.form_generation;
        let guard_key = api_key.clone();
        let guard_target = target.clone();
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(
                move |admin| async move { admin.validate_key(&target.input(), &api_key).await },
            );
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| {
                let Some(form) = &mut page.key_form else {
                    return;
                };
                if page.form_generation != form_generation {
                    return;
                }
                // A newer probe started since this one: its landing owns the
                // probing flag and the results.
                if form.probe_seq != probe_seq {
                    return;
                }
                // Same form, but its target moved (variant chip, custom label or
                // base_url edited): the result describes another provider — drop it
                // and release the probe so 获取模型 is clickable again.
                if form.target(cx).as_ref() != Ok(&guard_target) {
                    form.probing = false;
                    form.probed_models = None;
                    cx.notify();
                    return;
                }
                if !form.probing {
                    return;
                }
                if form.api_key.read(cx).value().trim() != guard_key {
                    form.probing = false;
                    form.probed_models = None;
                    form.selected_models.clear();
                    form.error = None;
                    cx.notify();
                    return;
                }
                form.probing = false;
                match result {
                    Ok(models) => {
                        // Keep selections the fresh list no longer offers, but marked:
                        // they still count (a probe listing only some models must not
                        // silently narrow the whitelist), they just render flagged.
                        form.probed_models = Some(models);
                    }
                    Err(error) => form.error = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Toggle one model chip in the add form's whitelist selection.
    fn toggle_form_model(&mut self, model: String, cx: &mut Context<Self>) {
        if let Some(form) = self.key_form.as_mut().filter(|form| !form.saving) {
            toggle_selection(&mut form.selected_models, &model);
        }
        cx.notify();
    }

    /// Toggle one model chip in the key editor's whitelist selection.
    fn toggle_editor_model(&mut self, model: String, cx: &mut Context<Self>) {
        if let Some(editor) = self.key_editor.as_mut().filter(|editor| !editor.saving) {
            toggle_selection(&mut editor.selected_models, &model);
        }
        cx.notify();
    }

    /// Toggle one group chip in the key editor's membership selection.
    fn toggle_editor_group(&mut self, group_id: i64, cx: &mut Context<Self>) {
        if let Some(editor) = self.key_editor.as_mut().filter(|editor| !editor.saving) {
            toggle_selection(&mut editor.selected_groups, &group_id);
        }
        cx.notify();
    }

    /// Toggle one group chip in the Kiro import's membership selection.
    fn toggle_import_group(&mut self, group_id: i64, cx: &mut Context<Self>) {
        if let Some(form) = self.key_form.as_mut().filter(|form| !form.saving)
            && let Some(import) = &mut form.kiro_import
        {
            toggle_selection(&mut import.selected_groups, &group_id);
        }
        cx.notify();
    }

    /// The eye on an account card: show that account's email in clear, or mask
    /// it again.
    fn toggle_account_reveal(&mut self, key_id: i64, cx: &mut Context<Self>) {
        if !self.revealed_accounts.remove(&key_id) {
            self.revealed_accounts.insert(key_id);
        }
        cx.notify();
    }

    /// Switch the add form's credential between the account login and a pasted
    /// API key. A login session the user abandons on the way is cancelled: the
    /// gateway keeps polling a waiting session until it expires, and a landing
    /// after the switch would create an account nobody asked for.
    fn set_form_credential_mode(&mut self, mode: CredentialMode, cx: &mut Context<Self>) {
        let Some(form) = self.key_form.as_ref().filter(|form| !form.saving) else {
            return;
        };
        if form.credential_mode == mode {
            return;
        }
        let abandon_login = mode == CredentialMode::ApiKey
            && form
                .login
                .as_ref()
                .is_some_and(|login| login.progress.is_waiting());
        if let Some(form) = &mut self.key_form {
            form.credential_mode = mode;
            form.error = None;
        }
        if abandon_login {
            self.cancel_account_login(cx);
        }
        cx.notify();
    }

    /// Put a local validation message on the key editor card.
    fn fail_editor(&mut self, message: &str, cx: &mut Context<Self>) {
        if let Some(editor) = &mut self.key_editor {
            editor.error = Some(message.to_string());
        }
        cx.notify();
    }

    /// 重新获取 (edit form): re-probe the key's model list and re-offer the ids
    /// as chips. Custom rows probe the edited protocol/base_url.
    fn reprobe_editor_models(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.key_editor else {
            return;
        };
        if editor.probing {
            return;
        }
        let key_id = editor.key_id;
        // A custom row with an edited endpoint probes the new endpoint with the
        // revealed key (nothing is persisted until save); otherwise the saved
        // key is re-probed, which also refreshes its stored snapshot.
        let endpoint_probe = match &editor.custom {
            None => None,
            Some(custom) => {
                let label = editor.label.read(cx).value().trim().to_string();
                let target = custom.target(&label, cx);
                let api_key = editor.api_key.clone();
                match target {
                    Err(message) => {
                        self.fail_editor(message, cx);
                        return;
                    }
                    Ok(target) if custom.endpoint_changed(&target) => match api_key {
                        Some(api_key) => Some((label, target, api_key)),
                        None => {
                            self.fail_editor("密钥尚未加载，请稍后再获取", cx);
                            return;
                        }
                    },
                    Ok(_) => None,
                }
            }
        };
        if let Some(editor) = &mut self.key_editor {
            editor.probing = true;
            editor.error = None;
        }
        cx.notify();
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                match endpoint_probe {
                    Some((name, target, api_key)) => {
                        let input = ProviderInput {
                            name: &name,
                            protocol: target.protocol.protocol,
                            base_url: &target.base_url,
                            auth_scheme: None,
                            enabled: true,
                        };
                        admin.validate_key(&input, &api_key).await
                    }
                    None => admin.list_key_models(key_id).await,
                }
            });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| {
                // Identity guard: the probe belongs to key_id; if the editor was
                // closed or now shows another key, discard the result entirely.
                let Some(editor) = &mut page.key_editor else {
                    return;
                };
                if !editor.probing || editor.key_id != key_id {
                    return;
                }
                editor.probing = false;
                match result {
                    Ok(models) => {
                        // Never silently drop a selected model: selections not in the
                        // fresh list stay (they may be plan-only models the listing omits)
                        // and render flagged as "not in list" instead.
                        editor.probed_models = Some(models);
                    }
                    Err(error) => editor.error = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Parse a concurrency-cap input: empty = 不限 (None), else a positive int.
    /// Shared by the add-key form and the key editor.
    fn parse_concurrency(raw: &str) -> Result<Option<i32>, &'static str> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(None);
        }
        match raw.parse::<i32>() {
            Ok(limit) if limit > 0 => Ok(Some(limit)),
            _ => Err("并发上限必须是正整数"),
        }
    }

    /// Open the inline edit form for one existing key, prefilled from the
    /// credential-free view, and fetch the plaintext credential once (the only
    /// credential read: the list shapes deliberately carry none).
    ///
    /// The ⋯ menu captures only the ids (its entity is cached between opens),
    /// so the row is resolved here; a key that left the list since then has
    /// nothing to edit and the call does nothing.
    fn open_key_editor(
        &mut self,
        provider_id: i64,
        key_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(card) = self.key_row(provider_id, key_id) else {
            return;
        };
        let key_label = card.key.label.clone();
        // Only a pasted credential can be revealed; a login-maintained one has
        // no key, and the editor renders the login instead of the key row.
        let has_static_key = card.key.auth_kind == "static";
        let limit = card.key.max_concurrency;
        let selected_models = card.key.allowed_models.clone().unwrap_or_default();
        let probed_models = card.key.models.clone();
        let label = cx.new(|cx| InputState::new(window, cx).placeholder("label（如 主用）"));
        label.update(cx, |input, cx| {
            input.set_value(key_label.clone(), window, cx)
        });
        let max_concurrency =
            cx.new(|cx| InputState::new(window, cx).placeholder("并发上限（空 = 不限）"));
        if let Some(limit) = limit {
            max_concurrency.update(cx, |input, cx| {
                input.set_value(limit.to_string(), window, cx)
            });
        }
        let focus = label.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        let initial_groups = match &self.state {
            LoadState::Loaded(data) => groups::memberships(&data.groups, key_id),
            _ => Vec::new(),
        };
        // Custom rows (no catalog entry, not Kiro) whose protocol is one of the
        // offered choices and whose name equals the key label also edit their
        // endpoint (name = label). Any other row gets the plain label +
        // concurrency editor and its provider is never renamed.
        let custom = self
            .provider_row(provider_id)
            .filter(|row| is_custom(&row.name, &row.protocol) && row.name == key_label)
            .and_then(|row| {
                let protocol = protocol_choice(&row.protocol)?;
                let base_url = cx.new(|cx| {
                    InputState::new(window, cx).placeholder(protocol.base_url_placeholder)
                });
                base_url.update(cx, |input, cx| {
                    input.set_value(row.base_url.clone(), window, cx)
                });
                Some(CustomEdit {
                    protocol,
                    base_url,
                    initial_name: row.name.clone(),
                    initial_protocol: protocol,
                    initial_base_url: row.base_url.clone(),
                })
            });
        self.key_editor = Some(KeyEditor {
            provider_id,
            key_id,
            label,
            max_concurrency,
            selected_models,
            selected_groups: initial_groups.clone(),
            initial_groups,
            probed_models,
            model_filter: Self::model_filter_input(window, cx),
            api_key: None,
            probing: false,
            saving: false,
            show_key: false,
            custom,
            error: None,
        });
        cx.notify();
        if has_static_key {
            self.reveal_editor_key(cx);
        }
    }

    /// The loaded provider row behind a key card, if the page has data.
    fn provider_row(&self, provider_id: i64) -> Option<&ProviderRow> {
        let LoadState::Loaded(data) = &self.state else {
            return None;
        };
        data.keys
            .iter()
            .map(|card| &card.provider)
            .find(|provider| provider.id == provider_id)
    }

    /// Pick the custom provider's protocol in the key editor.
    fn select_editor_protocol(
        &mut self,
        choice: ProtocolChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(custom) = self
            .key_editor
            .as_mut()
            .filter(|editor| !editor.saving)
            .and_then(|editor| editor.custom.as_mut())
        else {
            return;
        };
        if custom.protocol == choice {
            return;
        }
        custom.protocol = choice;
        custom.base_url.update(cx, |input, cx| {
            input.set_placeholder(choice.base_url_placeholder, window, cx)
        });
        cx.notify();
    }

    /// Fetch the editor's plaintext credential (one loopback reveal) into the
    /// open editor. A failure leaves it unset; the card shows 读取失败 and the
    /// next open retries.
    fn reveal_editor_key(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.key_editor else {
            return;
        };
        let provider_id = editor.provider_id;
        let key_id = editor.key_id;
        let load = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move { admin.reveal_key(provider_id, key_id).await });
        cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |page, cx| {
                let Some(editor) = &mut page.key_editor else {
                    return; // the editor closed while the reveal was in flight
                };
                if editor.key_id != key_id {
                    return; // a different key's editor took over
                }
                editor.api_key = result.map(|view| view.api_key).ok();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Save the key editor (label, concurrency cap, model whitelist, groups;
    /// for custom rows also the provider's name, protocol and base_url), then
    /// reload.
    fn save_key_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.key_editor else {
            return;
        };
        if editor.saving || editor.probing {
            return;
        }
        let provider_id = editor.provider_id;
        let key_id = editor.key_id;
        let label = editor.label.read(cx).value().trim().to_string();
        let concurrency_raw = editor.max_concurrency.read(cx).value();
        let max_concurrency = match Self::parse_concurrency(&concurrency_raw) {
            Ok(Some(limit)) => Some(Some(limit)),
            // The editor's blank value explicitly clears the existing cap.
            Ok(None) => Some(None),
            Err(message) => {
                if let Some(editor) = &mut self.key_editor {
                    editor.error = Some(message.to_string());
                }
                cx.notify();
                return;
            }
        };
        if label.is_empty() {
            if let Some(editor) = &mut self.key_editor {
                editor.error = Some("label 不能为空".to_string());
            }
            cx.notify();
            return;
        }
        // The chips editor is always authoritative on save: an empty selection
        // clears the whitelist (inner None = unrestricted), a non-empty one sets it.
        let selected = editor.selected_models.clone();
        let allowed_models: Option<Option<Vec<String>>> =
            Some((!selected.is_empty()).then_some(selected));
        // Group membership is replaced wholesale on the server, so send it only
        // when the selection really changed (an unchanged save writes nothing);
        // clearing every group is a change and goes out as an empty array.
        // Groups deleted since the editor opened are dropped from both sides
        // first: their chips are gone, and a stale id would fail the write.
        let live: &[KeyGroupView] = match &self.state {
            LoadState::Loaded(data) => &data.groups,
            _ => &[],
        };
        let initial_groups = groups::retain_existing(&editor.initial_groups, live);
        let selected_groups = groups::retain_existing(&editor.selected_groups, live);
        let group_ids: Option<Vec<i64>> =
            groups::selection_changed(&initial_groups, &selected_groups).then_some(selected_groups);
        // Custom rows: the provider row follows the label (name) and the edited
        // endpoint. A changed endpoint needs the revealed key to re-validate
        // before anything is written.
        let provider_write = match &editor.custom {
            None => None,
            Some(custom) => {
                let target = match custom.target(&label, cx) {
                    Ok(target) => target,
                    Err(message) => {
                        self.fail_editor(message, cx);
                        return;
                    }
                };
                let endpoint_changed = custom.endpoint_changed(&target);
                let validate_with = if endpoint_changed {
                    match editor.api_key.clone() {
                        Some(api_key) => Some(api_key),
                        None => {
                            self.fail_editor("密钥尚未加载，请稍后再保存", cx);
                            return;
                        }
                    }
                } else {
                    None
                };
                // Fail closed: the PATCH is a full replace, so without the loaded
                // row there is no trustworthy `enabled`/`auth_scheme` to carry over.
                let Some(row) = self.provider_row(provider_id) else {
                    if let Some(editor) = &mut self.key_editor {
                        editor.error = Some("找不到对应的 provider，请刷新后重试".to_string());
                        editor.saving = false;
                    }
                    cx.notify();
                    return;
                };
                let enabled = row.enabled;
                let auth_scheme =
                    (target.protocol == custom.initial_protocol).then(|| row.auth_scheme.clone());
                (endpoint_changed || label != custom.initial_name).then_some(ProviderWrite {
                    protocol: target.protocol.protocol,
                    base_url: target.base_url,
                    enabled,
                    auth_scheme,
                    validate_with,
                })
            }
        };
        if let Some(editor) = &mut self.key_editor {
            editor.saving = true;
            editor.error = None;
        }
        cx.notify();
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                // Provider PATCH goes before the key PATCH: a rejected endpoint or
                // a taken name leaves the key untouched.
                if let Some(write) = &provider_write {
                    let input = ProviderInput {
                        name: &label,
                        protocol: write.protocol,
                        base_url: &write.base_url,
                        auth_scheme: write.auth_scheme.as_deref(),
                        enabled: write.enabled,
                    };
                    let models = match &write.validate_with {
                        Some(api_key) => Some(admin.validate_key(&input, api_key).await?),
                        None => None,
                    };
                    admin.update_provider(provider_id, &input).await?;
                    // The new endpoint's model list becomes the key's snapshot;
                    // losing it only defers derived attributes to the next probe.
                    if let Some(models) = models
                        && let Err(error) = admin.save_key_models(key_id, &models).await
                    {
                        tracing::warn!("model snapshot of key {key_id} was not saved: {error}");
                    }
                }
                admin
                    .update_key(
                        provider_id,
                        key_id,
                        &KeyUpdate {
                            label: Some(&label),
                            max_concurrency,
                            allowed_models,
                            group_ids: group_ids.as_deref(),
                            ..Default::default()
                        },
                    )
                    .await
            });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| match result {
                Ok(()) => {
                    page.key_editor = None;
                    page.action_error = None;
                    page.reload(cx);
                }
                Err(error) => {
                    if let Some(editor) = &mut page.key_editor {
                        editor.saving = false;
                        editor.error = Some(error);
                    } else {
                        page.action_error = Some(error);
                    }
                    cx.notify();
                    // An earlier step (provider PATCH) may have landed.
                    page.reload(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn confirm_delete_key(
        &mut self,
        provider_id: i64,
        key_id: i64,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page = cx.entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let on_ok_page = page.clone();
            dialog
                .confirm()
                .title("删除账号?")
                .description(format!("账号「{label}」将被删除；引用它的路由会失效。"))
                .ok_text("删除")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    on_ok_page.update(cx, |page, cx| {
                        page.write(cx, move |admin| async move {
                            admin.delete_key(provider_id, key_id).await
                        })
                    });
                    true
                })
        });
    }
}

fn cleanup_failure_message(
    reason: &str,
    provider_id: i64,
    key_id: Option<i64>,
    key_error: Option<&str>,
    provider_error: Option<&str>,
) -> String {
    let mut message = format!("{reason}；清理未完成：provider {provider_id}");
    if let Some(key_id) = key_id {
        message.push_str(&format!("，key {key_id}"));
    }
    if let Some(error) = key_error {
        message.push_str(&format!(" 删除失败（{error}）"));
    }
    if let Some(error) = provider_error {
        message.push_str(&format!(" 删除失败（{error}）"));
    }
    message
}

async fn cleanup_new_resources(
    admin: &GatewayHttpClient,
    provider_id: i64,
    key_id: Option<i64>,
    provider_created: bool,
    reason: &str,
) -> Option<String> {
    let mut key_error = None;
    let key_deleted = match key_id {
        Some(key_id) => match admin.delete_key(provider_id, key_id).await {
            Ok(()) => true,
            Err(error) => {
                key_error = Some(error.to_string());
                false
            }
        },
        None => true,
    };
    let mut provider_error = None;
    if provider_created && key_deleted {
        // The emptiness predicate is part of the server-side DELETE, not a
        // separate list-then-delete sequence: a concurrently added key makes
        // this a conflict and keeps the provider instead of orphaning it.
        match admin.delete_provider_if_empty(provider_id).await {
            Ok(()) => {}
            Err(gateway::http_client::ClientError::Http { status, .. })
                if status.as_u16() == 404 => {}
            Err(error) => provider_error = Some(error.to_string()),
        }
    }
    if key_error.is_some() || provider_error.is_some() {
        Some(cleanup_failure_message(
            reason,
            provider_id,
            key_id,
            key_error.as_deref(),
            provider_error.as_deref(),
        ))
    } else {
        None
    }
}

fn import_error(
    reason: &str,
    error: gateway::http_client::ClientError,
    cleanup_error: Option<String>,
) -> gateway::http_client::ClientError {
    let message = match cleanup_error {
        Some(cleanup_error) => format!("{reason}：{error}；{cleanup_error}"),
        None => format!("{reason}：{error}"),
    };
    gateway::http_client::ClientError::Decode(message)
}

/// Per-account failure text: well-known gateway errors in Chinese, anything
/// else verbatim.
fn import_failure_message(error: &str) -> String {
    if error.contains("unknown group id") {
        "分组不存在".to_string()
    } else {
        error.to_string()
    }
}

/// Import `jobs` (label + account) under one find-or-created Kiro provider,
/// at most [`KIRO_IMPORT_CONCURRENCY`] at a time, sending one tick per
/// finished account. Per-account failures are collected, not fatal; when
/// this run created the provider and every account failed, the empty row is
/// removed again. Only the provider lookup itself fails the whole run.
async fn import_kiro_accounts(
    admin: GatewayHttpClient,
    provider_name: &str,
    endpoint: &str,
    jobs: Vec<(String, kiro_import::ImportItem)>,
    group_ids: Option<Arc<[i64]>>,
    max_concurrency: Option<Option<i32>>,
    progress: tokio::sync::mpsc::UnboundedSender<()>,
) -> Result<ImportOutcome, gateway::http_client::ClientError> {
    let (provider_id, provider_created) = admin
        .find_or_create_kiro_provider(provider_name, endpoint)
        .await?;
    let total = jobs.len();
    let limit = Arc::new(tokio::sync::Semaphore::new(KIRO_IMPORT_CONCURRENCY));
    let mut tasks = tokio::task::JoinSet::new();
    // Labels live here, keyed by task, so even a panicked task's failure
    // line names its account.
    let mut labels: HashMap<tokio::task::Id, String> = HashMap::with_capacity(total);
    for (label, item) in jobs {
        let admin = admin.clone();
        let limit = Arc::clone(&limit);
        let group_ids = group_ids.clone();
        let progress = progress.clone();
        let task_label = label.clone();
        let handle = tasks.spawn(async move {
            let _permit = limit
                .acquire_owned()
                .await
                .expect("import semaphore is never closed");
            let result = admin
                .import_kiro(
                    provider_id,
                    &task_label,
                    &item.credentials,
                    group_ids.as_deref(),
                    max_concurrency,
                )
                .await;
            // A closed receiver only means nobody watches the counter.
            let _ = progress.send(());
            result
                .err()
                .map(|error| import_failure_message(&error.to_string()))
        });
        labels.insert(handle.id(), label);
    }
    drop(progress);
    let mut failures: Vec<(String, String)> = Vec::new();
    while let Some(joined) = tasks.join_next_with_id().await {
        match joined {
            Ok((_, None)) => {}
            Ok((id, Some(error))) => {
                let label = labels.remove(&id).unwrap_or_default();
                failures.push((label, error));
            }
            Err(error) => {
                let label = labels.remove(&error.id()).unwrap_or_default();
                failures.push((label, format!("导入任务异常：{error}")));
            }
        }
    }
    failures.sort();
    let cleanup_error = if provider_created && failures.len() == total {
        cleanup_new_resources(&admin, provider_id, None, true, "Kiro 账号全部导入失败").await
    } else {
        None
    };
    Ok(ImportOutcome {
        total,
        failures,
        cleanup_error,
    })
}

impl crate::pages::ActionPage for ProvidersPage {
    fn action_error_slot(&mut self) -> &mut Option<String> {
        &mut self.action_error
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        ProvidersPage::reload(self, cx);
    }
}

impl Render for ProvidersPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_group_manager(window, cx);
        // No page title/subtitle (the tab bar names the view) and no action row:
        // the actions live in the tab row, so the content starts immediately.
        // Self-heal for a cancelled card drag (gpui has no drag-end event): once
        // the OS reports no active drag, any leftover drag state is stale.
        let frame_weak = cx.weak_entity();
        let frame = page_frame("", "", cx).on_mouse_move(move |_, _, cx| {
            frame_weak
                .update(cx, |this, cx| {
                    if this.drag.is_some() && !cx.has_active_drag() {
                        this.drag = None;
                        cx.notify();
                    }
                })
                .ok();
        });
        match &self.state {
            LoadState::Loading => frame.child(loading_card(cx)),
            LoadState::Failed(error) => frame.child(error_card(error, cx)),
            LoadState::Loaded(data) => {
                // Derived per render from the loaded groups: nothing cached.
                let (filter, visibility) = self.view_filter(data);
                // Only 全部 reorders accounts: every other view is a filtered
                // subset of the global order and stays read-only.
                let drag = if filter.allows_reorder() {
                    CardDrag::Enabled
                } else {
                    CardDrag::Disabled
                };
                // One clock reading for the whole render: every card judges
                // expired cooldowns and renewed windows against the same instant.
                let now = chrono::Utc::now();
                let any_visible = data.keys.iter().any(|card| visibility.admits(card.key.id));
                frame
                    .when_some(self.action_error.as_ref(), |this, error| {
                        this.child(action_error_card(error, cx))
                    })
                    .when_some(self.key_form.as_ref(), |this, form| {
                        this.child(self.key_form_card(form, cx))
                    })
                    .when_some(self.group_create.as_ref(), |this, form| {
                        this.child(self.group_create_card(form, cx))
                    })
                    .when_some(self.group_manager.as_ref(), |this, manager| {
                        this.child(self.group_manager_card(manager, data, cx))
                    })
                    .when(
                        !any_visible && (filter != GroupFilter::All || self.key_form.is_none()),
                        |this| {
                            let (title, hint) = filter.empty_copy();
                            this.child(empty_card(title, hint, cx))
                        },
                    )
                    .child({
                        let visible = data
                            .keys
                            .iter()
                            .filter(|card_data| visibility.admits(card_data.key.id));
                        let grid = gpui::div().flex().flex_wrap().gap_5();
                        grid.children(visible.map(|card_data| {
                            self.key_card(
                                card_data,
                                data.rests
                                    .get(&card_data.key.id)
                                    .map(Vec::as_slice)
                                    .unwrap_or(&[]),
                                &data.groups,
                                now,
                                drag,
                                cx,
                            )
                        }))
                    })
            }
        }
    }
}

/// Toggle `item` in a chip selection (click = add, click again = remove).
fn toggle_selection<T: PartialEq + Clone>(selected: &mut Vec<T>, item: &T) {
    if let Some(index) = selected.iter().position(|kept| kept == item) {
        selected.remove(index);
    } else {
        selected.push(item.clone());
    }
}
