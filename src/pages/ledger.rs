//! One user's ledger panel (the 用户 page's detail view): the balance, the
//! billing entries newest first with backward paging by `before_id`, the
//! top-up / adjust forms with a confirm step, and the reconciliation result.
//!
//! Money never goes through floats: the confirm preview parses the typed
//! amount with the gateway's own `billing::credit_amount` (same rules as the
//! server) and adds it to the loaded `balance_microus` as integers. The typed
//! text itself is what the write sends; the server parses it again.
//!
//! Ledger amounts render with [`crate::fmt::usd_exact`] (all six decimals
//! when significant): one charge is often below $0.0001, and a
//! reconciliation difference of 1 µUSD must stay visible.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AppContext as _, Context, Div, ElementId, Entity, EventEmitter, Focusable as _, IntoElement,
    ParentElement, Render, Styled, Window, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme, Disableable as _, WindowExt as _};

use gateway::http_client::ReconcileReport;
use gateway::store::{CreditKind, EntryRow, GatewayKeyRow, UserRow};

use crate::fmt::{usd_exact, usd_microus};
use crate::pages::{
    LoadState, action_error_card, begin_load, card, display_time, editor_row, error_card,
    icon_button, inline_error, loading_card,
};
use crate::runtime::GatewayRuntime;

/// Entries fetched per page (the gateway clamps to 1..=500).
const PAGE_SIZE: i64 = 50;

/// Emitted after a write changed the user's balance, so the user list
/// reloads its balance column.
pub(crate) struct BalanceChanged;

/// The user, their keys (to name the key behind a charge) and the newest
/// entries, read together so they describe one moment.
struct LedgerSnapshot {
    user: UserRow,
    keys: Vec<GatewayKeyRow>,
    /// Newest first; older pages are appended.
    entries: Vec<EntryRow>,
    /// The last page came back short: there is nothing older.
    exhausted: bool,
}

/// The top-up / adjust form. One at a time; opening resets it.
struct CreditForm {
    kind: CreditKind,
    amount: Entity<InputState>,
    note: Entity<InputState>,
    saving: bool,
    error: Option<String>,
}

enum ReconcileState {
    Running,
    Done(ReconcileReport),
    Failed(String),
}

/// Ledger detail of one user.
pub(crate) struct LedgerPanel {
    user_id: i64,
    state: LoadState<LedgerSnapshot>,
    loading_more: bool,
    credit: Option<CreditForm>,
    reconcile: Option<ReconcileState>,
    action_error: Option<String>,
}

impl EventEmitter<BalanceChanged> for LedgerPanel {}

impl LedgerPanel {
    /// A panel for `user_id`; it starts loading at once.
    pub(crate) fn new(user_id: i64, cx: &mut Context<Self>) -> Self {
        let mut panel = Self {
            user_id,
            state: LoadState::Loading,
            loading_more: false,
            credit: None,
            reconcile: None,
            action_error: None,
        };
        panel.reload(cx);
        panel
    }

    pub(crate) fn user_id(&self) -> i64 {
        self.user_id
    }

    /// Re-read the user, keys and the newest page of entries. A shown
    /// reconciliation is dropped: it described the previous state.
    pub(crate) fn reload(&mut self, cx: &mut Context<Self>) {
        begin_load(&mut self.state);
        self.reconcile = None;
        let id = self.user_id;
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                let user = admin.user(id).await?;
                let keys = admin.gateway_keys(Some(id)).await?;
                let entries = admin.user_entries(id, None, Some(PAGE_SIZE)).await?;
                let exhausted = (entries.len() as i64) < PAGE_SIZE;
                Ok(LedgerSnapshot {
                    user,
                    keys,
                    entries,
                    exhausted,
                })
            });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |panel, cx| {
                panel.state = match result {
                    Ok(snapshot) => LoadState::Loaded(snapshot),
                    Err(error) => LoadState::Failed(error),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Fetch the page older than the oldest loaded entry and append it.
    fn load_older(&mut self, cx: &mut Context<Self>) {
        let LoadState::Loaded(snapshot) = &self.state else {
            return;
        };
        let Some(before_id) = snapshot.entries.last().map(|entry| entry.id) else {
            return;
        };
        if self.loading_more || snapshot.exhausted {
            return;
        }
        self.loading_more = true;
        cx.notify();
        let id = self.user_id;
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                admin
                    .user_entries(id, Some(before_id), Some(PAGE_SIZE))
                    .await
            });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |panel, cx| {
                panel.loading_more = false;
                match result {
                    Ok(page) => {
                        // A reload that landed meanwhile replaced the list;
                        // this page only continues the list it was asked for.
                        if let LoadState::Loaded(snapshot) = &mut panel.state
                            && snapshot.entries.last().map(|entry| entry.id) == Some(before_id)
                        {
                            snapshot.exhausted = (page.len() as i64) < PAGE_SIZE;
                            snapshot.entries.extend(page);
                        }
                    }
                    Err(error) => panel.action_error = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn run_reconcile(&mut self, cx: &mut Context<Self>) {
        if matches!(self.reconcile, Some(ReconcileState::Running)) {
            return;
        }
        self.reconcile = Some(ReconcileState::Running);
        cx.notify();
        let id = self.user_id;
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move { admin.reconcile(id).await });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |panel, cx| {
                panel.reconcile = Some(match result {
                    Ok(report) => ReconcileState::Done(report),
                    Err(error) => ReconcileState::Failed(error),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_credit(&mut self, kind: CreditKind, window: &mut Window, cx: &mut Context<Self>) {
        let amount_hint = match kind {
            CreditKind::Topup => "金额 USD，如 10 或 0.5",
            CreditKind::Adjust => "金额 USD，可为负，如 -1.5",
        };
        let note_hint = match kind {
            CreditKind::Topup => "备注（可选）",
            CreditKind::Adjust => "备注（必填，说明调整原因）",
        };
        let amount = cx.new(|cx| InputState::new(window, cx).placeholder(amount_hint));
        let note = cx.new(|cx| InputState::new(window, cx).placeholder(note_hint));
        let focus = amount.read(cx).focus_handle(cx);
        self.credit = Some(CreditForm {
            kind,
            amount,
            note,
            saving: false,
            error: None,
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Validate the form and open the confirm dialog showing the balance
    /// before, the change and the balance after.
    fn confirm_credit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let balance = match &self.state {
            LoadState::Loaded(snapshot) => snapshot.user.balance_microus,
            _ => return,
        };
        let Some(form) = &mut self.credit else {
            return;
        };
        if form.saving {
            return;
        }
        let kind = form.kind;
        let amount_text = form.amount.read(cx).value().trim().to_string();
        let note = form.note.read(cx).value().trim().to_string();
        let delta = match gateway::billing::credit_amount(kind, &amount_text, &note) {
            Ok(delta) => delta,
            Err(reason) => {
                form.error = Some(credit_error_text(reason).to_string());
                cx.notify();
                return;
            }
        };
        let Some(after) = balance.checked_add(delta) else {
            form.error = Some("金额过大，余额会溢出".to_string());
            cx.notify();
            return;
        };
        form.error = None;
        cx.notify();

        let title = match kind {
            CreditKind::Topup => "确认充值",
            CreditKind::Adjust => "确认调整余额",
        };
        let delta_text = if delta > 0 {
            format!("+{}", usd_exact(delta))
        } else {
            usd_exact(delta)
        };
        let summary = format!(
            "当前余额 {}\n变动 {delta_text}\n变动后 {}",
            usd_exact(balance),
            usd_exact(after)
        );
        let panel = cx.entity();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let panel = panel.clone();
            let amount_text = amount_text.clone();
            let note = note.clone();
            dialog
                .confirm()
                .title(title)
                .description(summary.clone())
                .ok_text("确认")
                .ok_variant(if delta < 0 {
                    ButtonVariant::Danger
                } else {
                    ButtonVariant::Primary
                })
                .on_ok(move |_, _, cx| {
                    let amount_text = amount_text.clone();
                    let note = note.clone();
                    panel.update(cx, |panel, cx| {
                        panel.submit_credit(kind, amount_text, note, cx);
                    });
                    true
                })
        });
    }

    fn submit_credit(
        &mut self,
        kind: CreditKind,
        amount_text: String,
        note: String,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = &mut self.credit else {
            return;
        };
        form.saving = true;
        cx.notify();
        let id = self.user_id;
        let call = cx
            .global::<GatewayRuntime>()
            .admin_call(move |admin| async move {
                match kind {
                    CreditKind::Topup => {
                        let note = (!note.is_empty()).then_some(note.as_str());
                        admin.topup(id, &amount_text, note).await
                    }
                    CreditKind::Adjust => admin.adjust(id, &amount_text, &note).await,
                }
            });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |panel, cx| match result {
                Ok(_) => {
                    panel.credit = None;
                    panel.action_error = None;
                    panel.reload(cx);
                    cx.emit(BalanceChanged);
                }
                Err(error) => {
                    if let Some(form) = &mut panel.credit {
                        form.saving = false;
                        form.error = Some(error);
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn copy_request_id(&self, entry_id: i64, cx: &mut Context<Self>) {
        let LoadState::Loaded(snapshot) = &self.state else {
            return;
        };
        let request_id = snapshot
            .entries
            .iter()
            .find(|entry| entry.id == entry_id)
            .and_then(|entry| entry.request_id.as_deref());
        if let Some(request_id) = request_id {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(request_id.to_string()));
        }
    }

    fn credit_card(&self, cx: &mut Context<Self>) -> Option<Div> {
        let form = self.credit.as_ref()?;
        let title = match form.kind {
            CreditKind::Topup => "充值",
            CreditKind::Adjust => "调整余额",
        };
        let saving = form.saving;
        let mut body = card(cx)
            .gap_3()
            .child(
                gpui::div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(cx.theme().foreground)
                    .child(title),
            )
            .child(editor_row(
                "金额 USD",
                Input::new(&form.amount),
                Some(220.),
                cx,
            ))
            .child(editor_row("备注", Input::new(&form.note), None, cx));
        if let Some(error) = &form.error {
            body = body.child(inline_error(error.clone(), cx));
        }
        Some(
            body.child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("credit-next")
                            .primary()
                            .compact()
                            .label(if saving { "提交中…" } else { "下一步" })
                            .disabled(saving)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm_credit(window, cx);
                            })),
                    )
                    .child(
                        Button::new("credit-cancel")
                            .ghost()
                            .compact()
                            .label("取消")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.credit = None;
                                cx.notify();
                            })),
                    ),
            ),
        )
    }

    fn reconcile_card(&self, snapshot: &LedgerSnapshot, cx: &Context<Self>) -> Option<Div> {
        let state = self.reconcile.as_ref()?;
        let muted = cx.theme().muted_foreground;
        let body = card(cx).gap_1().child(
            gpui::div()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(cx.theme().foreground)
                .child("对账"),
        );
        let report = match state {
            ReconcileState::Running => {
                return Some(body.child(gpui::div().text_color(muted).child("对账中…")));
            }
            ReconcileState::Failed(error) => {
                return Some(body.child(inline_error(error.clone(), cx)));
            }
            ReconcileState::Done(report) => report,
        };
        let verdict = |ok: bool| if ok { "✓ 一致" } else { "✗ 不一致" };
        let color = |ok: bool| {
            if ok {
                cx.theme().success
            } else {
                cx.theme().danger
            }
        };
        // The server owns the verdict: it also compares the request log with
        // the ledger, which the loaded page cannot see.
        let server_ok = report.consistent;
        let mut body = body.child(
            gpui::div()
                .text_size(px(12.))
                .text_color(color(server_ok))
                .child(verdict(server_ok)),
        );
        let balance_ok = report.report.balance_microus == report.report.entries_sum_microus;
        body = body.child(
            gpui::div()
                .text_size(px(12.))
                .text_color(color(balance_ok))
                .child(format!(
                    "余额 {} · 流水合计 {} · {}",
                    usd_exact(report.report.balance_microus),
                    usd_exact(report.report.entries_sum_microus),
                    verdict(balance_ok)
                )),
        );
        // What the request log says was owed against what the ledger took.
        let uncollected = report.report.uncollected_microus;
        let charged = report.report.requests_sum_microus - uncollected;
        body = body.child(
            gpui::div()
                .text_size(px(12.))
                .text_color(if uncollected == 0 {
                    muted
                } else {
                    cx.theme().danger
                })
                .child(format!(
                    "请求合计 {} · 扣费合计 {} · 差额 {}",
                    usd_exact(report.report.requests_sum_microus),
                    usd_exact(charged),
                    usd_exact(uncollected)
                )),
        );
        if report.report.keys.is_empty() {
            body = body.child(
                gpui::div()
                    .text_size(px(12.))
                    .text_color(muted)
                    .child("该用户没有 Key"),
            );
        }
        for key in &report.report.keys {
            let ok = key.spent_microus == key.entries_sum_microus;
            body = body.child(
                gpui::div()
                    .text_size(px(12.))
                    .text_color(color(ok))
                    .child(format!(
                        "Key {} · 已用 {} · 扣费合计 {} · {}",
                        key_label(&snapshot.keys, key.key_id),
                        usd_exact(key.spent_microus),
                        usd_exact(key.entries_sum_microus),
                        verdict(ok)
                    )),
            );
        }
        Some(body)
    }

    fn entry_table(&self, snapshot: &LedgerSnapshot, cx: &mut Context<Self>) -> Div {
        let mut table = gpui::div()
            .flex()
            .flex_col()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .child(
                gpui::div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(gpui::div().w(px(108.)).child("时间"))
                    .child(gpui::div().w(px(36.)).child("类型"))
                    .child(gpui::div().w(px(100.)).child("金额"))
                    .child(gpui::div().w(px(100.)).child("余额"))
                    .child(gpui::div().flex_1().child("详情")),
            );
        for entry in &snapshot.entries {
            table = table.child(self.entry_row(entry, &snapshot.keys, cx));
        }
        table
    }

    fn entry_row(&self, entry: &EntryRow, keys: &[GatewayKeyRow], cx: &mut Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        let amount_color = if entry.amount_microus < 0 {
            cx.theme().danger
        } else {
            cx.theme().success
        };
        let mut detail = gpui::div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .text_color(muted);
        if let Some(request_id) = &entry.request_id {
            let entry_id = entry.id;
            detail = detail
                .child(
                    gpui::div()
                        .font_family("monospace")
                        .text_size(px(11.))
                        .child(request_id.clone()),
                )
                .child(icon_button(
                    ElementId::NamedInteger("copy-request-id".into(), entry.id as u64),
                    IconName::Copy,
                    "复制 request_id",
                    cx.listener(move |this, _, _, cx| this.copy_request_id(entry_id, cx)),
                ));
        }
        if let Some(key_id) = entry.gateway_key_id {
            detail = detail.child(format!("Key {}", key_label(keys, key_id)));
        }
        if let Some(cost) = entry.cost_microus {
            detail = detail.child(format!("标准价 {}", usd_exact(cost)));
        }
        if let Some(rate_bp) = entry.rate_bp {
            detail = detail.child(format!("倍率 {}", gateway::billing::format_rate(rate_bp)));
        }
        if !entry.note.is_empty() {
            detail = detail.child(entry.note.clone());
        }
        if let Some(operator) = &entry.operator {
            detail = detail.child(format!("by {operator}"));
        }
        gpui::div()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border.opacity(0.5))
            .text_size(px(12.))
            .child(
                gpui::div()
                    .w(px(108.))
                    .text_color(muted)
                    .child(display_time(entry.created_at)),
            )
            .child(
                gpui::div()
                    .w(px(36.))
                    .text_color(cx.theme().foreground)
                    .child(kind_label(&entry.kind).to_string()),
            )
            .child(
                gpui::div()
                    .w(px(100.))
                    .text_color(amount_color)
                    .child(usd_exact(entry.amount_microus)),
            )
            .child(
                gpui::div()
                    .w(px(100.))
                    .text_color(muted)
                    .child(usd_exact(entry.balance_after_microus)),
            )
            .child(detail)
    }
}

impl Render for LedgerPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut content = gpui::div().flex().flex_col().gap_3();
        if let Some(error) = &self.action_error {
            content = content.child(action_error_card(error, cx));
        }
        let snapshot = match &self.state {
            LoadState::Loading => return content.child(loading_card(cx)),
            LoadState::Failed(error) => return content.child(error_card(error, cx)),
            LoadState::Loaded(snapshot) => snapshot,
        };
        let balance = snapshot.user.balance_microus;
        let reconciling = matches!(self.reconcile, Some(ReconcileState::Running));
        content = content.child(
            gpui::div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    gpui::div()
                        .text_size(px(16.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(if balance <= 0 {
                            cx.theme().danger
                        } else {
                            cx.theme().foreground
                        })
                        .child(format!("余额 {}", usd_microus(balance))),
                )
                .child(gpui::div().flex_1())
                .child(
                    Button::new("ledger-topup")
                        .label("充值")
                        .outline()
                        .compact()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_credit(CreditKind::Topup, window, cx);
                        })),
                )
                .child(
                    Button::new("ledger-adjust")
                        .label("调整")
                        .outline()
                        .compact()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_credit(CreditKind::Adjust, window, cx);
                        })),
                )
                .child(
                    Button::new("ledger-reconcile")
                        .label(if reconciling {
                            "对账中…"
                        } else {
                            "对账"
                        })
                        .outline()
                        .compact()
                        .disabled(reconciling)
                        .on_click(cx.listener(|this, _, _, cx| this.run_reconcile(cx))),
                ),
        );
        if let Some(form) = self.credit_card(cx) {
            content = content.child(form);
        }
        if let Some(reconcile) = self.reconcile_card(snapshot, cx) {
            content = content.child(reconcile);
        }
        if snapshot.entries.is_empty() {
            return content.child(
                gpui::div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child("暂无流水"),
            );
        }
        let exhausted = snapshot.exhausted;
        content = content.child(self.entry_table(snapshot, cx));
        content.when(!exhausted, |content| {
            content.child(
                Button::new("ledger-older")
                    .label(if self.loading_more {
                        "加载中…"
                    } else {
                        "加载更早"
                    })
                    .ghost()
                    .compact()
                    .disabled(self.loading_more)
                    .on_click(cx.listener(|this, _, _, cx| this.load_older(cx))),
            )
        })
    }
}

/// The label of `key_id` among the user's keys; a key gone from the list
/// shows its id.
fn key_label(keys: &[GatewayKeyRow], key_id: i64) -> String {
    keys.iter()
        .find(|key| key.id == key_id)
        .map_or_else(|| format!("#{key_id}"), |key| key.label.clone())
}

fn kind_label(kind: &str) -> &str {
    match kind {
        "charge" => "扣费",
        "topup" => "充值",
        "adjust" => "调整",
        "refund" => "退款",
        other => other,
    }
}

/// The form-level text of a `billing::credit_amount` refusal; an unknown
/// reason shows as the gateway wrote it.
fn credit_error_text(reason: &'static str) -> &'static str {
    match reason {
        "amount_usd must be a decimal USD string with at most 6 decimals" => {
            "金额须为 USD 小数，最多 6 位小数"
        }
        "top-up amount must be positive" => "充值金额必须大于 0",
        "adjustment amount must not be zero" => "调整金额不能为 0",
        "adjustment requires a note" => "调整必须填写备注",
        other => other,
    }
}
