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
//! variant resolution (pure data/logic), `quota` = quota view models, `cards`
//! = the card rendering.

mod cards;
mod catalog;
pub mod kiro_import;
mod quota;

use std::collections::HashMap;

use gpui::{
    AppContext as _, Context, Entity, Focusable as _, InteractiveElement, IntoElement,
    ParentElement, Render, Styled, Window, prelude::FluentBuilder as _,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant};
use gpui_kit::component::input::{InputState, TextareaState};
use gpui_kit::component::{Icon, Sizable, WindowExt};

use gateway::admin_ops::Bootstrap;
use gateway::http_client::GatewayHttpClient;
use gateway::kiro::KiroOAuth;

use gateway::probe::ProbeModel;
use gateway::quota::KeyQuota;
use gateway::store::{ProviderInput, ProviderKeyView, ProviderRow, RestRow};

use catalog::{PROVIDER_SPECS, Region, resolve};
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

/// Live card-drag state: which key is being dragged and over which card it
/// currently hovers (target id + whether the drop lands after it). Cards
/// reorder by drag and drop; there is no drag-end notification in gpui, so a
/// cancelled drag is healed by the page-level mouse-move handler, which
/// clears this once `App::has_active_drag()` reports false.
pub(super) struct KeyDrag {
    dragged: i64,
    target: Option<(i64, bool)>,
}

/// Loaded page data: every key across all providers in global position order,
/// plus the active cooldowns by key id (health info, shown on the cards) — one
/// key may carry several rows (whole-key plus per-model cooldowns).
struct ProvidersData {
    keys: Vec<KeyCardData>,
    rests: HashMap<i64, Vec<RestRow>>,
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
        Self { keys, rests }
    }
}

/// Add-key form: pick a provider from the catalog (with 区域 × Coding Plan
/// variants for kimi/glm), paste the credential.
struct KeyForm {
    /// Index into `PROVIDER_SPECS`.
    spec_index: usize,
    /// 区域（仅对有变体的厂商生效；默认国内）。
    region: Region,
    /// Coding Plan 入口（仅对有变体的厂商生效）。
    coding_plan: bool,
    label: Entity<InputState>,
    api_key: Entity<InputState>,
    /// Kiro uses a JSON import instead of the ordinary API-key field.
    kiro_import: Option<KiroImportForm>,
    /// Optional per-key concurrency cap (empty = 不限).
    max_concurrency: Entity<InputState>,
    /// Models the last 获取模型 probe returned (None until probed).
    probed_models: Option<Vec<ProbeModel>>,
    /// Model ids the user picked out of `probed_models`; empty = 全部模型.
    selected_models: Vec<String>,
    /// A 获取模型 probe is in flight.
    probing: bool,
    /// A save is in flight (validating the key upstream, then persisting).
    saving: bool,
    error: Option<String>,
}

/// State for the Kiro JSON import boundary. The raw document and parsed
/// credentials are cleared before the async write begins, so secrets never
/// remain in the form after either a successful or failed import.
struct KiroImportForm {
    raw_json: Entity<TextareaState>,
    provider_name: Entity<InputState>,
    key_label: Entity<InputState>,
    parsed: Option<kiro_import::ParsedKiroExport>,
    parsed_raw_json: Option<String>,
    selected_account: usize,
    default_provider_name: String,
    generation: u64,
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
    /// Models offered for picking: prefilled from the key's probe snapshot,
    /// refreshed by 重新获取.
    probed_models: Option<Vec<ProbeModel>>,
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
    error: Option<String>,
}

/// Upstream API keys, one card each.
pub struct ProvidersPage {
    state: LoadState<ProvidersData>,
    /// Stale-overwrite guard: a reload started later always wins over one still in flight.
    generation: u64,
    form_generation: u64,
    key_form: Option<KeyForm>,
    key_editor: Option<KeyEditor>,
    /// Failure of a row-level write (toggle/delete/reorder); shown above the list.
    action_error: Option<String>,
    /// Live card drag-and-drop state (see `KeyDrag`).
    drag: Option<KeyDrag>,
    /// Last seen activity-feed revisions: `slot_changes` (the dispatcher
    /// published a new occupancy) applies the pushed values locally;
    /// `bootstrap_rev` (a pushed document landed) re-applies the page slice;
    /// `resync` (a gap in the stream) re-reads the document once over HTTP.
    seen_slot_changes: u64,
    seen_resync: u64,
    seen_bootstrap: u64,
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
            drag: None,
            seen_slot_changes: feed.read(cx).slot_changes(),
            seen_resync: feed.read(cx).resync(),
            seen_bootstrap: feed.read(cx).bootstrap_rev(),
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
    /// 账号 only once data has loaded, since the form needs the provider list.
    pub(crate) fn actions(&mut self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let refresh = refresh_button(
            "providers-refresh",
            cx.listener(|this, _, _, cx| {
                this.reload(cx);
                this.refresh_quotas(cx);
            }),
        );
        match &self.state {
            LoadState::Loaded(_) => vec![
                Button::new("key-add")
                    .icon(Icon::new(IconName::Plus).small())
                    .label("添加账号")
                    .outline()
                    .compact()
                    .on_click(cx.listener(|this, _, window, cx| this.open_key_form(window, cx)))
                    .into_any_element(),
                refresh.into_any_element(),
            ],
            _ => vec![refresh.into_any_element()],
        }
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
                page.state = match result {
                    Ok(document) => LoadState::Loaded(ProvidersData::from_bootstrap(&document)),
                    Err(error) => LoadState::Failed(error),
                };
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
                            None => KeyQuotaView::new(quota, now, &[]),
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

    /// The key's enabled state as the page last loaded it (`None` if unknown).
    /// Menus read this at click time — their entities are cached between opens.
    fn key_enabled(&self, provider_id: i64, key_id: i64) -> Option<bool> {
        match &self.state {
            LoadState::Loaded(data) => data
                .keys
                .iter()
                .find(|c| c.provider.id == provider_id && c.key.id == key_id)
                .map(|c| c.key.enabled),
            _ => None,
        }
    }

    /// Open the add-key form, defaulting to the first catalog provider.
    fn open_key_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.form_generation = self.form_generation.wrapping_add(1);
        let label = cx.new(|cx| InputState::new(window, cx).placeholder("label（如 主用）"));
        let api_key = cx.new(|cx| InputState::new(window, cx).placeholder("sk-…"));
        let max_concurrency =
            cx.new(|cx| InputState::new(window, cx).placeholder("并发上限（空 = 不限）"));
        let focus = label.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        self.key_form = Some(KeyForm {
            spec_index: 0,
            // Region/plan intentionally survive switching between vendors: they act
            // as a memory of the last choice (resolve ignores them for variant-less
            // vendors), not as per-vendor state.
            region: Region::Domestic,
            coding_plan: false,
            label,
            api_key,
            kiro_import: None,
            max_concurrency,
            probed_models: None,
            selected_models: Vec::new(),
            probing: false,
            saving: false,
            error: None,
        });
        cx.notify();
    }

    /// Switch the add form between catalog entries. Kiro gets its own import
    /// state and never enters the ordinary API-key validation path.
    fn select_form_provider(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let same_provider = self
            .key_form
            .as_ref()
            .is_some_and(|form| form.spec_index == index);
        if same_provider {
            return;
        }
        self.form_generation = self.form_generation.wrapping_add(1);
        let Some(form) = &mut self.key_form else {
            return;
        };
        form.spec_index = index;
        form.probed_models = None;
        form.selected_models.clear();
        form.probing = false;
        form.kiro_import = if PROVIDER_SPECS[index].name == "kiro" {
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
                selected_account: 0,
                default_provider_name: "kiro".to_string(),
                generation: self.form_generation,
            })
        } else {
            None
        };
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
                let region = parsed
                    .summaries()
                    .next()
                    .map(|summary| summary.region)
                    .unwrap_or_else(|| "us-east-1".to_string());
                let default_name = format!("kiro-{region}");
                let current_name = import.provider_name.read(cx).value();
                if current_name.contains('/') {
                    import.parsed = None;
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
                import.parsed = Some(parsed);
                import.parsed_raw_json = Some(raw_json.to_string());
                import.selected_account = 0;
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

    /// Save the add-key form: find-or-create the provider row from the catalog
    /// spec, then create the key. Errors stay in the form.
    fn save_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.key_form else { return };
        if PROVIDER_SPECS[form.spec_index].name == "kiro" {
            self.save_kiro_import(window, cx);
            return;
        }
        if form.saving {
            return; // one save in flight at a time
        }
        let spec: &'static catalog::ProviderSpec = &PROVIDER_SPECS[form.spec_index];
        let (name, base_url) = resolve(spec, form.region, form.coding_plan);
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
        let allowed_models: Option<Vec<String>> = if form.selected_models.is_empty() {
            None
        } else {
            Some(form.selected_models.clone())
        };
        let form_generation = self.form_generation;
        // Validate the key upstream before anything is persisted (the provider's
        // model list doubles as the liveness/auth proof); the provider row is
        // find-or-created only after the key checks out.
        if let Some(form) = &mut self.key_form {
            form.saving = true;
            form.error = None;
        }
        cx.notify();
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                let input = ProviderInput {
                    name,
                    protocol: spec.protocol,
                    base_url,
                    auth_scheme: Some(spec.auth_scheme),
                    enabled: true,
                };
                let models = admin.validate_key(&input, &api_key).await?;
                let provider_id = admin.find_or_create_provider(&input).await?;
                let key_id = admin
                    .create_key(
                        provider_id,
                        &label,
                        &api_key,
                        max_concurrency,
                        allowed_models,
                    )
                    .await?;
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
        let selected_account = import.selected_account;
        let Some(oauth) = parsed.oauth(selected_account) else {
            if let Some(form) = &mut self.key_form {
                form.error = Some("所选 Kiro 账号不存在".to_string());
            }
            cx.notify();
            return;
        };
        let form_generation = import.generation;
        let endpoint = catalog::kiro_endpoint(&oauth.region);
        if let Some(import) = self
            .key_form
            .as_mut()
            .and_then(|form| form.kiro_import.as_mut())
        {
            import
                .raw_json
                .update(cx, |input, cx| input.set_value("", window, cx));
            import.parsed = None;
            import.parsed_raw_json = None;
        }
        if let Some(form) = &mut self.key_form {
            form.saving = true;
            form.error = None;
        }
        cx.notify();
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                import_kiro(&admin, &provider_name, &endpoint, &label, &oauth).await
            });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| {
                let Some(form) = &mut page.key_form else {
                    return;
                };
                let Some(import) = &form.kiro_import else {
                    return;
                };
                if import.generation != form_generation {
                    return;
                }
                match result {
                    Ok(()) => {
                        page.key_form = None;
                        page.action_error = None;
                        page.reload(cx);
                        page.refresh_quotas(cx);
                    }
                    Err(error) => {
                        form.saving = false;
                        form.error = Some(error);
                        cx.notify();
                    }
                }
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
        let spec: &'static catalog::ProviderSpec = &PROVIDER_SPECS[form.spec_index];
        let spec_index = form.spec_index;
        let region = form.region;
        let coding_plan = form.coding_plan;
        let (name, base_url) = resolve(spec, region, coding_plan);
        let api_key = form.api_key.read(cx).value().trim().to_string();
        if api_key.is_empty() {
            if let Some(form) = &mut self.key_form {
                form.error = Some("请先填写 api_key".to_string());
            }
            cx.notify();
            return;
        }
        if let Some(form) = &mut self.key_form {
            form.probing = true;
            form.error = None;
        }
        cx.notify();
        // Identity guard inputs, captured before the await: the landing only
        // applies while the form still probes the same provider/variant and the
        // same api_key — a switched or reopened form does not receive it.
        let form_generation = self.form_generation;
        let guard_key = api_key.clone();
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                let input = ProviderInput {
                    name,
                    protocol: spec.protocol,
                    base_url,
                    auth_scheme: Some(spec.auth_scheme),
                    enabled: true,
                };
                admin.validate_key(&input, &api_key).await
            });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |page, cx| {
                let Some(form) = &mut page.key_form else {
                    return;
                };
                if page.form_generation != form_generation
                    || form.spec_index != spec_index
                    || form.region != region
                    || form.coding_plan != coding_plan
                {
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
        if let Some(editor) = &mut self.key_editor {
            toggle_selection(&mut editor.selected_models, &model);
        }
        cx.notify();
    }

    /// 重新获取 (edit form): re-probe the saved key's model list (this also
    /// refreshes the key's persisted snapshot) and re-offer the ids as chips.
    fn reprobe_editor_models(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.key_editor else {
            return;
        };
        if editor.probing {
            return;
        }
        let key_id = editor.key_id;
        if let Some(editor) = &mut self.key_editor {
            editor.probing = true;
            editor.error = None;
        }
        cx.notify();
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move { admin.list_key_models(key_id).await });
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
    fn open_key_editor(
        &mut self,
        provider_id: i64,
        key: &ProviderKeyView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = cx.new(|cx| InputState::new(window, cx).placeholder("label（如 主用）"));
        label.update(cx, |input, cx| {
            input.set_value(key.label.clone(), window, cx)
        });
        let max_concurrency =
            cx.new(|cx| InputState::new(window, cx).placeholder("并发上限（空 = 不限）"));
        if let Some(limit) = key.max_concurrency {
            max_concurrency.update(cx, |input, cx| {
                input.set_value(limit.to_string(), window, cx)
            });
        }
        let focus = label.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        let key_id = key.id;
        self.key_editor = Some(KeyEditor {
            provider_id,
            key_id,
            label,
            max_concurrency,
            selected_models: key.allowed_models.clone().unwrap_or_default(),
            probed_models: key.models.clone(),
            api_key: None,
            probing: false,
            saving: false,
            show_key: false,
            error: None,
        });
        cx.notify();
        self.reveal_editor_key(cx);
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

    /// Save the key editor (label + concurrency cap), then reload.
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
            Ok(limit) => limit,
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
        if let Some(editor) = &mut self.key_editor {
            editor.saving = true;
            editor.error = None;
        }
        cx.notify();
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                admin
                    .update_key(
                        provider_id,
                        key_id,
                        Some(&label),
                        None,
                        Some(max_concurrency),
                        allowed_models,
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

async fn import_kiro(
    admin: &GatewayHttpClient,
    provider_name: &str,
    endpoint: &str,
    label: &str,
    oauth: &KiroOAuth,
) -> Result<(), gateway::http_client::ClientError> {
    let input = ProviderInput {
        name: provider_name,
        protocol: "kiro",
        base_url: endpoint,
        auth_scheme: Some("bearer"),
        enabled: true,
    };
    let (provider_id, provider_created) = match admin
        .providers()
        .await?
        .into_iter()
        .find(|provider| provider.name.trim() == provider_name)
    {
        Some(provider)
            if provider.protocol == "kiro"
                && provider.base_url.trim().trim_end_matches('/')
                    == endpoint.trim().trim_end_matches('/') =>
        {
            (provider.id, false)
        }
        Some(provider) => {
            return Err(gateway::http_client::ClientError::Decode(format!(
                "provider {provider_name} has incompatible protocol or endpoint {}",
                provider.base_url
            )));
        }
        None => match admin.create_provider(&input).await {
            Ok(provider_id) => (provider_id, true),
            Err(_) => {
                return Err(gateway::http_client::ClientError::Decode(
                    "provider creation outcome is unknown; verify before retrying".to_owned(),
                ));
            }
        },
    };
    match admin.upsert_kiro_key(provider_id, label, oauth).await {
        Ok(_) => Ok(()),
        Err(error) => {
            let cleanup_error = if provider_created {
                cleanup_new_resources(admin, provider_id, None, true, "导入 Kiro key 失败").await
            } else {
                None
            };
            Err(import_error("导入 Kiro key 失败", error, cleanup_error))
        }
    }
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            LoadState::Loaded(data) => frame
                .when_some(self.action_error.as_ref(), |this, error| {
                    this.child(action_error_card(error, cx))
                })
                .when_some(self.key_form.as_ref(), |this, form| {
                    this.child(self.key_form_card(form, cx))
                })
                .when(data.keys.is_empty() && self.key_form.is_none(), |this| {
                    this.child(empty_card(
                        "尚无账号",
                        "点击「+ 添加账号」添加第一个上游账号。",
                        cx,
                    ))
                })
                .child(
                    gpui::div()
                        .flex()
                        .flex_wrap()
                        .gap_5()
                        .children(data.keys.iter().map(|card_data| {
                            self.key_card(
                                card_data,
                                data.rests
                                    .get(&card_data.key.id)
                                    .map(Vec::as_slice)
                                    .unwrap_or(&[]),
                                cx,
                            )
                        })),
                ),
        }
    }
}

/// Toggle `model` in a whitelist selection (click = add, click again = remove).
fn toggle_selection(selected: &mut Vec<String>, model: &str) {
    if let Some(index) = selected.iter().position(|kept| kept == model) {
        selected.remove(index);
    } else {
        selected.push(model.to_owned());
    }
}

/// Move `dragged` to just before (`after` = false) or after `target` in the
/// global order, returning the new full id order. Dropping on itself or an
/// unknown id leaves the order unchanged.
fn reordered(all_ids: &[i64], dragged: i64, target: i64, after: bool) -> Vec<i64> {
    let mut order: Vec<i64> = all_ids
        .iter()
        .copied()
        .filter(|id| *id != dragged)
        .collect();
    if order.len() == all_ids.len() {
        return all_ids.to_vec(); // dragged not in the order
    }
    let Some(mut position) = order.iter().position(|id| *id == target) else {
        return all_ids.to_vec();
    };
    if after {
        position += 1;
    }
    order.insert(position, dragged);
    order
}
